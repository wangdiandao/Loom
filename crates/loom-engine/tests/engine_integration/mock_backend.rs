//! Deterministic mock [`PlatformBackend`] and helpers for headless integration tests.
#![allow(dead_code, unused_imports)]
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use loom_config::AppSettings;
use loom_engine::{
    CommandKind, CommandSuccess, EngineCommand, EngineConfig, EngineCoordinator,
    LeakDetectorConfig, ProjectionEngine, ViewProjectionConfig,
};
use loom_pal_api::{
    AppHistoryReader, FastSystemSampler, PalError, PerfSampleScratch, PlatformBackend,
    ProcessSampleScratch, ServiceManager, SessionManager, SlowMetadataProber, StartupManager,
    SystemOperator,
};
use loom_types::{
    AppHistoryRecord, AppHistorySortColumn, CapabilityFlags, CpuAffinityMask, CpuCoreKind,
    CpuStaticInfo, DetailsSortColumn, DetailsViewMode, DiskDeviceSample, DiskMediaType,
    ExeIconRgba, ExeStaticAsset, FileLockHolder, GpuAdapterSample, GpuEngineSample,
    HardwareStaticInfo, HistoryRing, LeakKind, MemoryHardwareInfo, MonitorDomain, NetAdapterSample,
    NetConnectionType, PlatformProfile, ProcessArch, ProcessCategory, ProcessColdMeta,
    ProcessHotSample, ProcessKey, ProcessPriority, ProcessRowKind, ProcessSocketRecord,
    ProcessSortColumn, ProcessStatusFlags, ProcessViewMode, RefreshSpeed, RunTaskRequest,
    ServiceRecord, ServiceState, ServicesSortColumn, SocketProtocol, SortDirection, StartupEntry,
    StartupImpact, StartupSortColumn, SystemPerfSample, Timestamp100Ns, TreeKillItem,
    TreeKillReport, UacVirtualization, UserSessionRecord, UserSessionState, UsersRowProjection,
    UsersSortColumn, WaitChainNode, WebView2Role,
};
use parking_lot::Mutex;

#[derive(Debug, Clone)]
pub(crate) struct MockProcSpec {
    pub(crate) key: ProcessKey,
    pub(crate) parent_pid: u32,
    pub(crate) session_id: u32,
    pub(crate) name: String,
    pub(crate) window_title: String,
    pub(crate) category: ProcessCategory,
    cpu_time_100ns: u64,
    pub(crate) cpu_delta_per_tick: u64,
    pub(crate) disk_bytes: u64,
    pub(crate) disk_delta_per_tick: u64,
    pub(crate) net_bytes: u64,
    pub(crate) net_delta_per_tick: u64,
    pub(crate) private_mem_bytes: u64,
    pub(crate) handle_count: u32,
    pub(crate) status: ProcessStatusFlags,
    pub(crate) exe_path: Option<String>,
    pub(crate) command_line: Option<String>,
}

impl MockProcSpec {
    pub(crate) fn new(pid: u32, create_time: u64, parent_pid: u32, name: &str) -> Self {
        Self {
            key: ProcessKey::new(pid, create_time),
            parent_pid,
            session_id: 1,
            name: name.to_owned(),
            window_title: String::new(),
            category: ProcessCategory::Background,
            cpu_time_100ns: 1_000_000,
            cpu_delta_per_tick: 0,
            disk_bytes: 0,
            disk_delta_per_tick: 0,
            net_bytes: 0,
            net_delta_per_tick: 0,
            private_mem_bytes: 32 * 1024 * 1024,
            handle_count: 64,
            status: ProcessStatusFlags::empty(),
            exe_path: None,
            command_line: None,
        }
    }
}

#[derive(Default)]
pub(crate) struct MockMgmtState {
    pub(crate) list_services_calls: AtomicUsize,
    pub(crate) list_startup_calls: AtomicUsize,
    pub(crate) list_sessions_calls: AtomicUsize,
    pub(crate) read_history_calls: AtomicUsize,
    pub(crate) save_history_calls: AtomicUsize,
    pub(crate) saved_history_snapshots: Mutex<Vec<(u64, Vec<AppHistoryRecord>)>>,
    pub(crate) open_console_calls: AtomicUsize,
    pub(crate) services: Mutex<Vec<ServiceRecord>>,
    pub(crate) startup_bios_ms: Mutex<Option<u64>>,
    pub(crate) startup_entries: Mutex<Vec<StartupEntry>>,
    pub(crate) sessions: Mutex<Vec<UserSessionRecord>>,
    pub(crate) history_since: Mutex<Option<u64>>,
    pub(crate) history_items: Mutex<Vec<AppHistoryRecord>>,
    pub(crate) service_ops_require_elevation: AtomicBool,
    pub(crate) startup_ops_require_elevation: AtomicBool,
    pub(crate) session_ops_require_elevation: AtomicBool,
}

pub(crate) struct MockState {
    pub(crate) procs: Vec<MockProcSpec>,
    pub(crate) dynamic_mem_override: Mutex<HashMap<ProcessKey, u64>>,
    pub(crate) current_ts: u64,
    pub(crate) ts_step_100ns: u64,
    pub(crate) extra_ts_jump_100ns: Mutex<u64>,
    pub(crate) logical_cpus: u32,
    pub(crate) mem_total_bytes: u64,
    pub(crate) sample_proc_calls: AtomicUsize,
    pub(crate) sample_perf_calls: AtomicUsize,
    pub(crate) fail_next_perf_sample: AtomicBool,
    pub(crate) gpu_d3_asleep: AtomicBool,
    pub(crate) core_kinds: Mutex<Vec<CpuCoreKind>>,
    pub(crate) per_proc_net_transitions: Mutex<Vec<bool>>,
    pub(crate) per_proc_gpu_transitions: Mutex<Vec<bool>>,
    pub(crate) probed_proc_order: Mutex<Vec<ProcessKey>>,
    pub(crate) list_ports_calls: AtomicUsize,
    pub(crate) listening_ports: Mutex<Vec<ProcessSocketRecord>>,
    pub(crate) probe_proc_calls: AtomicUsize,
    pub(crate) probe_asset_calls: AtomicUsize,
    pub(crate) probe_asset_slow_calls: AtomicUsize,
    pub(crate) probe_hw_calls: AtomicUsize,
    pub(crate) probe_disk_calls: AtomicUsize,
    pub(crate) disk_probe_sleep: Duration,
    pub(crate) exe_asset_slow_prefix: Option<String>,
    pub(crate) exe_asset_sleep: Duration,
    pub(crate) slow_probe_block_pid: Option<u32>,
    pub(crate) slow_probe_sleep: Duration,
    pub(crate) slow_probe_release: Option<Receiver<()>>,
    pub(crate) recorded_tree_kills: Mutex<Vec<Vec<ProcessKey>>>,
    pub(crate) suspended_keys: Mutex<Vec<ProcessKey>>,
    pub(crate) resumed_keys: Mutex<Vec<ProcessKey>>,
    pub(crate) suspend_error_elevation: AtomicBool,
    pub(crate) restart_explorer_calls: AtomicUsize,
    pub(crate) file_locks_result: Mutex<Vec<FileLockHolder>>,
    pub(crate) priorities: Mutex<HashMap<ProcessKey, ProcessPriority>>,
    pub(crate) browse_result: Mutex<Option<PathBuf>>,
    pub(crate) terminate_error_elevation: AtomicBool,
    pub(crate) uac_error_elevation: AtomicBool,
    pub(crate) debugger_error_elevation: AtomicBool,
    pub(crate) debugger_attached_keys: Mutex<Vec<ProcessKey>>,
    pub(crate) wait_chain_nodes: Mutex<Vec<WaitChainNode>>,
    pub(crate) minidump_sleep: Duration,
    pub(crate) is_elevated: AtomicBool,
    pub(crate) mgmt: MockMgmtState,
}

impl MockState {
    pub(crate) fn new(procs: Vec<MockProcSpec>) -> Arc<Self> {
        Arc::new(Self::new_raw(procs))
    }

    pub(crate) fn new_raw(procs: Vec<MockProcSpec>) -> Self {
        Self {
            procs,
            dynamic_mem_override: Mutex::new(HashMap::new()),
            current_ts: 100_000_000,
            ts_step_100ns: 2_500_000, // 250 ms per tick
            extra_ts_jump_100ns: Mutex::new(0),
            logical_cpus: 4,
            mem_total_bytes: 16 * 1024 * 1024 * 1024,
            sample_proc_calls: AtomicUsize::new(0),
            sample_perf_calls: AtomicUsize::new(0),
            fail_next_perf_sample: AtomicBool::new(false),
            gpu_d3_asleep: AtomicBool::new(false),
            core_kinds: Mutex::new(Vec::new()),
            per_proc_net_transitions: Mutex::new(Vec::new()),
            per_proc_gpu_transitions: Mutex::new(Vec::new()),
            probed_proc_order: Mutex::new(Vec::new()),
            list_ports_calls: AtomicUsize::new(0),
            listening_ports: Mutex::new(Vec::new()),
            probe_proc_calls: AtomicUsize::new(0),
            probe_asset_calls: AtomicUsize::new(0),
            probe_asset_slow_calls: AtomicUsize::new(0),
            probe_hw_calls: AtomicUsize::new(0),
            probe_disk_calls: AtomicUsize::new(0),
            disk_probe_sleep: Duration::ZERO,
            exe_asset_slow_prefix: None,
            exe_asset_sleep: Duration::ZERO,
            slow_probe_block_pid: None,
            slow_probe_sleep: Duration::ZERO,
            slow_probe_release: None,
            recorded_tree_kills: Mutex::new(Vec::new()),
            suspended_keys: Mutex::new(Vec::new()),
            resumed_keys: Mutex::new(Vec::new()),
            suspend_error_elevation: AtomicBool::new(false),
            restart_explorer_calls: AtomicUsize::new(0),
            file_locks_result: Mutex::new(Vec::new()),
            priorities: Mutex::new(HashMap::new()),
            browse_result: Mutex::new(None),
            terminate_error_elevation: AtomicBool::new(false),
            uac_error_elevation: AtomicBool::new(false),
            debugger_error_elevation: AtomicBool::new(false),
            debugger_attached_keys: Mutex::new(Vec::new()),
            wait_chain_nodes: Mutex::new(Vec::new()),
            minidump_sleep: Duration::ZERO,
            is_elevated: AtomicBool::new(false),
            mgmt: MockMgmtState::default(),
        }
    }
}

pub(crate) struct MockBackend {
    pub(crate) state: Arc<MockState>,
    pub(crate) services: MockServices,
    pub(crate) startup: MockStartup,
    pub(crate) sessions: MockSessions,
    pub(crate) history: MockHistory,
    pub(crate) operator: MockOperator,
}

impl MockBackend {
    pub(crate) fn from_state(state: Arc<MockState>) -> Self {
        let services = MockServices {
            state: Arc::clone(&state),
        };
        let startup = MockStartup {
            state: Arc::clone(&state),
        };
        let sessions = MockSessions {
            state: Arc::clone(&state),
        };
        let history = MockHistory {
            state: Arc::clone(&state),
        };
        let operator = MockOperator {
            state: Arc::clone(&state),
        };
        Self {
            state,
            services,
            startup,
            sessions,
            history,
            operator,
        }
    }
}

pub(crate) struct MockFastSampler {
    state: Arc<MockState>,
    tick: u64,
}

impl FastSystemSampler for MockFastSampler {
    type Ext = ();

    fn sample_processes(
        &mut self,
        scratch: &mut ProcessSampleScratch<Self::Ext>,
    ) -> Result<Timestamp100Ns, PalError> {
        let tick = self.tick;
        self.tick += 1;
        self.state.sample_proc_calls.fetch_add(1, Ordering::AcqRel);

        let extra_jump = *self.state.extra_ts_jump_100ns.lock();
        let ts = Timestamp100Ns::new(
            self.state.current_ts + tick * self.state.ts_step_100ns + extra_jump,
        );
        scratch.perf = SystemPerfSample {
            timestamp: ts,
            logical_cpu_count: self.state.logical_cpus,
            physical_core_count: self.state.logical_cpus / 2,
            process_count: self.state.procs.len() as u32,
            mem_total_bytes: self.state.mem_total_bytes,
            mem_used_bytes: self.state.mem_total_bytes / 4,
            ..SystemPerfSample::default()
        };

        let mem_overrides = self.state.dynamic_mem_override.lock().clone();
        for spec in &self.state.procs {
            let (name_offset, name_len) = scratch.push_str(&spec.name);
            let (window_title_offset, window_title_len) = scratch.push_str(&spec.window_title);
            let cpu = spec.cpu_time_100ns + tick * spec.cpu_delta_per_tick;
            let disk = spec.disk_bytes + tick * spec.disk_delta_per_tick;
            let net = spec.net_bytes + tick * spec.net_delta_per_tick;
            let private_mem = mem_overrides
                .get(&spec.key)
                .copied()
                .unwrap_or(spec.private_mem_bytes);
            let priority = self
                .state
                .priorities
                .lock()
                .get(&spec.key)
                .copied()
                .unwrap_or(ProcessPriority::Normal);
            scratch.samples.push(ProcessHotSample {
                key: spec.key,
                parent_pid: spec.parent_pid,
                session_id: spec.session_id,
                thread_count: 4,
                handle_count: spec.handle_count,
                kernel_time_100ns: cpu / 2,
                user_time_100ns: cpu - cpu / 2,
                private_working_set_bytes: private_mem,
                working_set_bytes: private_mem,
                commit_bytes: private_mem,
                disk_read_bytes: disk,
                net_recv_bytes: net,
                category: spec.category,
                priority,
                status: spec.status,
                name_offset,
                name_len,
                window_title_offset,
                window_title_len,
                ..ProcessHotSample::default()
            });
        }

        Ok(ts)
    }

    fn sample_performance(
        &mut self,
        scratch: &mut PerfSampleScratch,
    ) -> Result<Timestamp100Ns, PalError> {
        let calls = self.state.sample_perf_calls.fetch_add(1, Ordering::AcqRel) as u64;
        if self
            .state
            .fail_next_perf_sample
            .swap(false, Ordering::AcqRel)
        {
            return Err(PalError::OsError {
                code: -1,
                message: "injected perf sampling error".into(),
            });
        }
        let ts = Timestamp100Ns::new(self.state.current_ts + calls * self.state.ts_step_100ns);
        scratch.sample = SystemPerfSample {
            timestamp: ts,
            cpu_total_permille: 320,
            cpu_kernel_permille: 80,
            cpu_freq_mhz: 4200,
            cpu_base_freq_mhz: 3600,
            logical_cpu_count: self.state.logical_cpus,
            mem_total_bytes: self.state.mem_total_bytes,
            mem_used_bytes: self.state.mem_total_bytes / 2,
            net_recv_bytes_per_sec: 125_000 + calls * 1000,
            net_sent_bytes_per_sec: 42_000 + calls * 500,
            gpu_util_permille: 240,
            gpu_dedicated_used_bytes: 2 * 1024 * 1024 * 1024,
            gpu_dedicated_total_bytes: 8 * 1024 * 1024 * 1024,
            gpu_shared_used_bytes: 512 * 1024 * 1024,
            ..SystemPerfSample::default()
        };
        scratch.per_core_permille.clear();
        scratch
            .per_core_permille
            .extend((0..self.state.logical_cpus).map(|i| 100 + (i as u16) * 50));
        scratch.networks.clear();
        scratch.networks.push(NetAdapterSample {
            luid: 0x1001,
            name: Arc::from("Ethernet"),
            description: Arc::from("Mock Gigabit Adapter"),
            connection_type: NetConnectionType::Ethernet,
            link_speed_bps: 1_000_000_000,
            ipv4: Some(Arc::from("192.168.1.50")),
            ipv6: Some(Arc::from("fe80::1")),
            dns_suffix: Some(Arc::from("local")),
            recv_bytes_per_sec: 125_000 + calls * 1000,
            sent_bytes_per_sec: 42_000 + calls * 500,
        });
        scratch.gpus.clear();
        scratch.gpus.push(GpuAdapterSample {
            index: 0,
            luid: 0x2001,
            name: Arc::from("Mock RTX GPU"),
            driver_version: Some(Arc::from("32.0.15.6000")),
            directx_version: Some(Arc::from("12 (WDDM 3.1)")),
            pci_location: Some(Arc::from("PCI bus 1, device 0, function 0")),
            temperature_celsius: Some(48),
            power_draw_mw: Some(42_500),
            fan_speed_percent: Some(38),
            core_clock_mhz: Some(2130),
            memory_clock_mhz: Some(10501),
            util_permille: 240,
            dedicated_used_bytes: 2 * 1024 * 1024 * 1024,
            dedicated_total_bytes: 8 * 1024 * 1024 * 1024,
            shared_used_bytes: 512 * 1024 * 1024,
            shared_total_bytes: 8 * 1024 * 1024 * 1024,
            d3_asleep: self.state.gpu_d3_asleep.load(Ordering::Acquire),
            engines: vec![
                GpuEngineSample {
                    name: Arc::from("3D"),
                    util_permille: 240,
                },
                GpuEngineSample {
                    name: Arc::from("Copy"),
                    util_permille: 45,
                },
            ],
        });
        Ok(ts)
    }

    fn set_per_process_network_enabled(&mut self, enabled: bool) {
        self.state.per_proc_net_transitions.lock().push(enabled);
    }

    fn set_per_process_gpu_enabled(&mut self, enabled: bool) {
        self.state.per_proc_gpu_transitions.lock().push(enabled);
    }
}

pub(crate) struct MockSlowProber {
    state: Arc<MockState>,
    by_key: HashMap<ProcessKey, (Option<String>, Option<String>)>,
}

impl SlowMetadataProber for MockSlowProber {
    fn probe_process(&self, key: ProcessKey) -> Result<ProcessColdMeta, PalError> {
        self.state.probe_proc_calls.fetch_add(1, Ordering::AcqRel);
        self.state.probed_proc_order.lock().push(key);
        if self.state.slow_probe_block_pid == Some(key.pid) {
            if let Some(ref rx) = self.state.slow_probe_release {
                let _ = rx.recv_timeout(self.state.slow_probe_sleep);
            } else if !self.state.slow_probe_sleep.is_zero() {
                std::thread::sleep(self.state.slow_probe_sleep);
            }
        }
        let (exe_opt, cmd_opt) = self.by_key.get(&key).cloned().unwrap_or((None, None));
        let exe_path = exe_opt.map(Arc::<str>::from);
        let command_line = Some(Arc::<str>::from(cmd_opt.as_deref().unwrap_or("--mock-arg")));

        Ok(ProcessColdMeta {
            key,
            exe_path,
            command_line,
            user_name: Some(Arc::from("TEST\\User")),
            arch: ProcessArch::X64,
            uac_virtualization: UacVirtualization::NotAllowed,
            efficiency_mode: false,
            is_elevated: self.state.is_elevated.load(Ordering::Acquire),
            app_group_id: None,
            asset: None,
        })
    }

    fn probe_exe_asset(&self, exe_path: &str) -> Result<ExeStaticAsset, PalError> {
        self.state.probe_asset_calls.fetch_add(1, Ordering::AcqRel);
        if let Some(ref prefix) = self.state.exe_asset_slow_prefix
            && exe_path.starts_with(prefix.as_str())
        {
            self.state
                .probe_asset_slow_calls
                .fetch_add(1, Ordering::AcqRel);
            if !self.state.exe_asset_sleep.is_zero() {
                std::thread::sleep(self.state.exe_asset_sleep);
            }
        }
        Ok(ExeStaticAsset {
            normalized_path: Arc::from(exe_path),
            publisher: Some(Arc::from("Mock Publisher")),
            file_description: Some(Arc::from("Mock Browser")),
            icon: Some(ExeIconRgba {
                width: 20,
                height: 20,
                rgba: Arc::from(vec![255u8; 20 * 20 * 4]),
            }),
            is_network_path: false,
        })
    }

    fn probe_hardware_static_info(&self) -> Result<HardwareStaticInfo, PalError> {
        self.state.probe_hw_calls.fetch_add(1, Ordering::AcqRel);
        Ok(HardwareStaticInfo {
            cpu: CpuStaticInfo {
                brand_name: Arc::from("Mock Ryzen 9 9950X"),
                base_freq_mhz: 3600,
                sockets: 1,
                physical_cores: self.state.logical_cpus / 2,
                logical_processors: self.state.logical_cpus,
                virtualization_enabled: Some(true),
                l1_cache_bytes: 512 * 1024,
                l2_cache_bytes: 4 * 1024 * 1024,
                l3_cache_bytes: 32 * 1024 * 1024,
                core_kinds: self.state.core_kinds.lock().clone(),
            },
            memory: MemoryHardwareInfo {
                speed_mt_s: Some(6000),
                slots_used: Some(2),
                slots_total: Some(4),
                form_factor: Some(Arc::from("DIMM")),
                hardware_reserved_bytes: 128 * 1024 * 1024,
            },
        })
    }

    fn probe_disk_performance(&self) -> Result<Vec<DiskDeviceSample>, PalError> {
        let n = self.state.probe_disk_calls.fetch_add(1, Ordering::AcqRel) as u64;
        if !self.state.disk_probe_sleep.is_zero() {
            std::thread::sleep(self.state.disk_probe_sleep);
        }
        Ok(vec![DiskDeviceSample {
            index: 0,
            model: Arc::from("Mock NVMe 1TB"),
            media_type: DiskMediaType::Nvme,
            drive_letters: Arc::from("C:"),
            capacity_bytes: 1024 * 1024 * 1024 * 1024,
            active_time_permille: 150,
            read_bytes_per_sec: 10_000_000 + n * 1_000,
            write_bytes_per_sec: 5_000_000 + n * 500,
            avg_response_us: 350,
            circuit_open: false,
        }])
    }

    fn list_listening_ports(&self) -> Result<Vec<ProcessSocketRecord>, PalError> {
        self.state.list_ports_calls.fetch_add(1, Ordering::AcqRel);
        Ok(self.state.listening_ports.lock().clone())
    }
}

pub(crate) struct MockServices {
    state: Arc<MockState>,
}
impl ServiceManager for MockServices {
    fn list_services(&self) -> Result<Vec<ServiceRecord>, PalError> {
        self.state
            .mgmt
            .list_services_calls
            .fetch_add(1, Ordering::AcqRel);
        Ok(self.state.mgmt.services.lock().clone())
    }
    fn start_service(&self, name: &str) -> Result<(), PalError> {
        if self
            .state
            .mgmt
            .service_ops_require_elevation
            .load(Ordering::Acquire)
        {
            return Err(PalError::ElevationRequired);
        }
        let mut guard = self.state.mgmt.services.lock();
        if let Some(svc) = guard.iter_mut().find(|s| s.name.as_ref() == name) {
            svc.state = ServiceState::Running;
            svc.pid = Some(4242);
        }
        Ok(())
    }
    fn stop_service(&self, name: &str) -> Result<(), PalError> {
        if self
            .state
            .mgmt
            .service_ops_require_elevation
            .load(Ordering::Acquire)
        {
            return Err(PalError::ElevationRequired);
        }
        let mut guard = self.state.mgmt.services.lock();
        if let Some(svc) = guard.iter_mut().find(|s| s.name.as_ref() == name) {
            svc.state = ServiceState::Stopped;
            svc.pid = None;
        }
        Ok(())
    }
    fn restart_service(&self, name: &str) -> Result<(), PalError> {
        if self
            .state
            .mgmt
            .service_ops_require_elevation
            .load(Ordering::Acquire)
        {
            return Err(PalError::ElevationRequired);
        }
        let mut guard = self.state.mgmt.services.lock();
        if let Some(svc) = guard.iter_mut().find(|s| s.name.as_ref() == name) {
            svc.state = ServiceState::Running;
            svc.pid = Some(svc.pid.unwrap_or(1000) + 1);
        }
        Ok(())
    }
    fn open_services_console(&self) -> Result<(), PalError> {
        self.state
            .mgmt
            .open_console_calls
            .fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

pub(crate) struct MockStartup {
    state: Arc<MockState>,
}
impl StartupManager for MockStartup {
    fn list_startup_entries(&self) -> Result<(Option<u64>, Vec<StartupEntry>), PalError> {
        self.state
            .mgmt
            .list_startup_calls
            .fetch_add(1, Ordering::AcqRel);
        let bios = *self.state.mgmt.startup_bios_ms.lock();
        let items = self.state.mgmt.startup_entries.lock().clone();
        Ok((bios.or(Some(2_100)), items))
    }
    fn set_startup_enabled(&self, id: &str, enabled: bool) -> Result<(), PalError> {
        if self
            .state
            .mgmt
            .startup_ops_require_elevation
            .load(Ordering::Acquire)
        {
            return Err(PalError::ElevationRequired);
        }
        let mut guard = self.state.mgmt.startup_entries.lock();
        if let Some(entry) = guard.iter_mut().find(|e| e.id.as_ref() == id) {
            entry.enabled = enabled;
            entry.impact = if enabled {
                StartupImpact::NotMeasured
            } else {
                StartupImpact::None
            };
        }
        Ok(())
    }
}

pub(crate) struct MockSessions {
    state: Arc<MockState>,
}
impl SessionManager for MockSessions {
    fn list_sessions(&self) -> Result<Vec<UserSessionRecord>, PalError> {
        self.state
            .mgmt
            .list_sessions_calls
            .fetch_add(1, Ordering::AcqRel);
        Ok(self.state.mgmt.sessions.lock().clone())
    }
    fn disconnect_session(&self, session_id: u32) -> Result<(), PalError> {
        if self
            .state
            .mgmt
            .session_ops_require_elevation
            .load(Ordering::Acquire)
        {
            return Err(PalError::ElevationRequired);
        }
        let mut guard = self.state.mgmt.sessions.lock();
        if let Some(s) = guard.iter_mut().find(|s| s.session_id == session_id) {
            s.state = UserSessionState::Disconnected;
        }
        Ok(())
    }
}

pub(crate) struct MockHistory {
    state: Arc<MockState>,
}
impl AppHistoryReader for MockHistory {
    fn read_history(&self) -> Result<(Option<u64>, Vec<AppHistoryRecord>), PalError> {
        self.state
            .mgmt
            .read_history_calls
            .fetch_add(1, Ordering::AcqRel);
        let since = *self.state.mgmt.history_since.lock();
        let items = self.state.mgmt.history_items.lock().clone();
        Ok((since, items))
    }
    fn clear_history(&self) -> Result<(), PalError> {
        *self.state.mgmt.history_since.lock() = Some(1_750_000_000);
        self.state.mgmt.history_items.lock().clear();
        Ok(())
    }
    fn save_history(
        &self,
        since_epoch_secs: u64,
        records: &[AppHistoryRecord],
    ) -> Result<(), PalError> {
        self.state
            .mgmt
            .save_history_calls
            .fetch_add(1, Ordering::AcqRel);
        *self.state.mgmt.history_since.lock() = Some(since_epoch_secs);
        *self.state.mgmt.history_items.lock() = records.to_vec();
        self.state
            .mgmt
            .saved_history_snapshots
            .lock()
            .push((since_epoch_secs, records.to_vec()));
        Ok(())
    }
}

pub(crate) struct MockOperator {
    state: Arc<MockState>,
}

impl SystemOperator for MockOperator {
    fn run_new_task(&self, _req: &RunTaskRequest) -> Result<Option<u32>, PalError> {
        Ok(Some(9999))
    }

    fn terminate_process(&self, _key: ProcessKey) -> Result<(), PalError> {
        if self.state.terminate_error_elevation.load(Ordering::Acquire) {
            return Err(PalError::ElevationRequired);
        }
        Ok(())
    }

    fn terminate_process_tree(&self, targets: &[ProcessKey]) -> Result<TreeKillReport, PalError> {
        self.state.recorded_tree_kills.lock().push(targets.to_vec());
        let root = targets.last().copied().unwrap_or_default();
        let items = targets
            .iter()
            .map(|&key| TreeKillItem {
                key,
                name: Arc::from(format!("pid-{}", key.pid)),
                terminated: true,
                error: None,
            })
            .collect();
        Ok(TreeKillReport { root, items })
    }

    fn set_efficiency_mode(&self, _key: ProcessKey, _enabled: bool) -> Result<(), PalError> {
        Ok(())
    }
    fn set_priority(&self, key: ProcessKey, priority: ProcessPriority) -> Result<(), PalError> {
        self.state.priorities.lock().insert(key, priority);
        Ok(())
    }
    fn get_affinity(&self, _key: ProcessKey) -> Result<CpuAffinityMask, PalError> {
        Ok(CpuAffinityMask::all(self.state.logical_cpus))
    }
    fn set_affinity(&self, _key: ProcessKey, _mask: &CpuAffinityMask) -> Result<(), PalError> {
        Ok(())
    }
    fn set_uac_virtualization(&self, _key: ProcessKey, _enabled: bool) -> Result<(), PalError> {
        if self.state.uac_error_elevation.load(Ordering::Acquire) {
            return Err(PalError::ElevationRequired);
        }
        Ok(())
    }
    fn analyze_wait_chain(&self, _key: ProcessKey) -> Result<Vec<WaitChainNode>, PalError> {
        Ok(self.state.wait_chain_nodes.lock().clone())
    }
    fn write_minidump(
        &self,
        _key: ProcessKey,
        dest: &Path,
        cancel: &AtomicBool,
    ) -> Result<(), PalError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(PalError::Cancelled);
        }
        if !self.state.minidump_sleep.is_zero() {
            std::thread::sleep(self.state.minidump_sleep);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(PalError::Cancelled);
        }
        let _ = dest;
        Ok(())
    }
    fn attach_debugger(&self, key: ProcessKey) -> Result<(), PalError> {
        if self.state.debugger_error_elevation.load(Ordering::Acquire) {
            return Err(PalError::ElevationRequired);
        }
        self.state.debugger_attached_keys.lock().push(key);
        Ok(())
    }
    fn suspend_process(&self, key: ProcessKey) -> Result<(), PalError> {
        if self.state.suspend_error_elevation.load(Ordering::Acquire) {
            return Err(PalError::ElevationRequired);
        }
        self.state.suspended_keys.lock().push(key);
        Ok(())
    }
    fn resume_process(&self, key: ProcessKey) -> Result<(), PalError> {
        self.state.resumed_keys.lock().push(key);
        Ok(())
    }
    fn restart_explorer(&self) -> Result<(), PalError> {
        self.state
            .restart_explorer_calls
            .fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    fn find_file_locks(&self, _path: &Path) -> Result<Vec<FileLockHolder>, PalError> {
        Ok(self.state.file_locks_result.lock().clone())
    }
    fn relaunch_self_elevated(&self, _restore_route: &str) -> Result<(), PalError> {
        Ok(())
    }
    fn open_file_location(&self, _path: &Path) -> Result<(), PalError> {
        Ok(())
    }
    fn show_file_properties(&self, _path: &Path) -> Result<(), PalError> {
        Ok(())
    }
    fn search_online(&self, _query: &str) -> Result<(), PalError> {
        Ok(())
    }
    fn browse_executable(&self) -> Result<Option<PathBuf>, PalError> {
        Ok(self.state.browse_result.lock().clone())
    }
    fn is_elevated(&self) -> bool {
        self.state.is_elevated.load(Ordering::Acquire)
    }
    fn logical_cpu_count(&self) -> u32 {
        self.state.logical_cpus
    }
}

impl PlatformBackend for MockBackend {
    type ProcessExt = ();
    type FastSampler = MockFastSampler;
    type SlowProber = MockSlowProber;
    type ServiceMgr = MockServices;
    type StartupMgr = MockStartup;
    type SessionMgr = MockSessions;
    type HistoryReader = MockHistory;
    type Operator = MockOperator;

    const PLATFORM: PlatformProfile = PlatformProfile::UNKNOWN;

    const CAPABILITIES: CapabilityFlags = CapabilityFlags::all();

    fn boost_self_priority() {}

    fn initialize() -> Result<Self, PalError> {
        Ok(Self::from_state(MockState::new(Vec::new())))
    }

    fn create_fast_sampler(&self) -> Result<Self::FastSampler, PalError> {
        Ok(MockFastSampler {
            state: Arc::clone(&self.state),
            tick: 0,
        })
    }

    fn create_slow_prober(&self) -> Result<Self::SlowProber, PalError> {
        let by_key = self
            .state
            .procs
            .iter()
            .map(|p| (p.key, (p.exe_path.clone(), p.command_line.clone())))
            .collect();
        Ok(MockSlowProber {
            state: Arc::clone(&self.state),
            by_key,
        })
    }

    fn service_manager(&self) -> &Self::ServiceMgr {
        &self.services
    }
    fn startup_manager(&self) -> &Self::StartupMgr {
        &self.startup
    }
    fn session_manager(&self) -> &Self::SessionMgr {
        &self.sessions
    }
    fn history_reader(&self) -> &Self::HistoryReader {
        &self.history
    }
    fn operator(&self) -> &Self::Operator {
        &self.operator
    }
}

pub(crate) fn wait_until<F>(timeout: Duration, mut cond: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    cond()
}
