//! Headless integration and fault-injection tests for `loom-engine`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use loom_config::AppSettings;
use loom_engine::{
    CommandKind, CommandSuccess, EngineCommand, EngineConfig, EngineCoordinator, ProjectionEngine,
    ViewProjectionConfig,
};
use loom_pal_api::{
    AppHistoryReader, FastSystemSampler, PalError, PerfSampleScratch, PlatformBackend,
    ProcessSampleScratch, ServiceManager, SessionManager, SlowMetadataProber, StartupManager,
    SystemOperator,
};
use loom_types::{
    AppHistoryRecord, CapabilityFlags, CpuAffinityMask, CpuStaticInfo, DetailsViewMode,
    DiskDeviceSample, DiskMediaType, ExeIconRgba, ExeStaticAsset, GpuAdapterSample,
    GpuEngineSample, HardwareStaticInfo, MemoryHardwareInfo, MonitorDomain, NetAdapterSample,
    NetConnectionType, PlatformProfile, ProcessArch, ProcessCategory, ProcessColdMeta,
    ProcessHotSample, ProcessKey, ProcessPriority, ProcessRowKind, ProcessSortColumn,
    ProcessStatusFlags, ProcessViewMode, RefreshSpeed, RunTaskRequest, ServiceRecord, ServiceState,
    SortDirection, StartupEntry, StartupImpact, SystemPerfSample, Timestamp100Ns, TreeKillItem,
    TreeKillReport, UacVirtualization, UserSessionRecord, UserSessionState, WaitChainNode,
};
use parking_lot::Mutex;

#[derive(Debug, Clone)]
struct MockProcSpec {
    key: ProcessKey,
    parent_pid: u32,
    session_id: u32,
    name: String,
    window_title: String,
    category: ProcessCategory,
    cpu_time_100ns: u64,
    cpu_delta_per_tick: u64,
    disk_bytes: u64,
    disk_delta_per_tick: u64,
    net_bytes: u64,
    net_delta_per_tick: u64,
    private_mem_bytes: u64,
    status: ProcessStatusFlags,
    exe_path: Option<String>,
}

impl MockProcSpec {
    fn new(pid: u32, create_time: u64, parent_pid: u32, name: &str) -> Self {
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
            status: ProcessStatusFlags::empty(),
            exe_path: None,
        }
    }
}

#[derive(Default)]
struct MockMgmtState {
    list_services_calls: AtomicUsize,
    list_startup_calls: AtomicUsize,
    list_sessions_calls: AtomicUsize,
    read_history_calls: AtomicUsize,
    open_console_calls: AtomicUsize,
    services: Mutex<Vec<ServiceRecord>>,
    startup_bios_ms: Mutex<Option<u64>>,
    startup_entries: Mutex<Vec<StartupEntry>>,
    sessions: Mutex<Vec<UserSessionRecord>>,
    history_since: Mutex<Option<u64>>,
    history_items: Mutex<Vec<AppHistoryRecord>>,
    service_ops_require_elevation: AtomicBool,
    startup_ops_require_elevation: AtomicBool,
    session_ops_require_elevation: AtomicBool,
}

struct MockState {
    procs: Vec<MockProcSpec>,
    current_ts: u64,
    ts_step_100ns: u64,
    logical_cpus: u32,
    mem_total_bytes: u64,
    sample_proc_calls: AtomicUsize,
    sample_perf_calls: AtomicUsize,
    probe_proc_calls: AtomicUsize,
    probe_asset_calls: AtomicUsize,
    probe_asset_slow_calls: AtomicUsize,
    probe_hw_calls: AtomicUsize,
    probe_disk_calls: AtomicUsize,
    disk_probe_sleep: Duration,
    exe_asset_slow_prefix: Option<String>,
    exe_asset_sleep: Duration,
    slow_probe_block_pid: Option<u32>,
    slow_probe_sleep: Duration,
    slow_probe_release: Option<Receiver<()>>,
    recorded_tree_kills: Mutex<Vec<Vec<ProcessKey>>>,
    priorities: Mutex<HashMap<ProcessKey, ProcessPriority>>,
    browse_result: Mutex<Option<PathBuf>>,
    terminate_error_elevation: AtomicBool,
    uac_error_elevation: AtomicBool,
    debugger_error_elevation: AtomicBool,
    debugger_attached_keys: Mutex<Vec<ProcessKey>>,
    wait_chain_nodes: Mutex<Vec<WaitChainNode>>,
    minidump_sleep: Duration,
    is_elevated: AtomicBool,
    mgmt: MockMgmtState,
}

impl MockState {
    fn new(procs: Vec<MockProcSpec>) -> Arc<Self> {
        Arc::new(Self {
            procs,
            current_ts: 100_000_000,
            ts_step_100ns: 2_500_000, // 250 ms per tick
            logical_cpus: 4,
            mem_total_bytes: 16 * 1024 * 1024 * 1024,
            sample_proc_calls: AtomicUsize::new(0),
            sample_perf_calls: AtomicUsize::new(0),
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
        })
    }
}

struct MockBackend {
    state: Arc<MockState>,
    services: MockServices,
    startup: MockStartup,
    sessions: MockSessions,
    history: MockHistory,
    operator: MockOperator,
}

impl MockBackend {
    fn from_state(state: Arc<MockState>) -> Self {
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

struct MockFastSampler {
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

        let ts = Timestamp100Ns::new(self.state.current_ts + tick * self.state.ts_step_100ns);
        scratch.perf = SystemPerfSample {
            timestamp: ts,
            logical_cpu_count: self.state.logical_cpus,
            physical_core_count: self.state.logical_cpus / 2,
            process_count: self.state.procs.len() as u32,
            mem_total_bytes: self.state.mem_total_bytes,
            mem_used_bytes: self.state.mem_total_bytes / 4,
            ..SystemPerfSample::default()
        };

        for spec in &self.state.procs {
            let (name_offset, name_len) = scratch.push_str(&spec.name);
            let (window_title_offset, window_title_len) = scratch.push_str(&spec.window_title);
            let cpu = spec.cpu_time_100ns + tick * spec.cpu_delta_per_tick;
            let disk = spec.disk_bytes + tick * spec.disk_delta_per_tick;
            let net = spec.net_bytes + tick * spec.net_delta_per_tick;
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
                handle_count: 64,
                kernel_time_100ns: cpu / 2,
                user_time_100ns: cpu - cpu / 2,
                private_working_set_bytes: spec.private_mem_bytes,
                working_set_bytes: spec.private_mem_bytes,
                commit_bytes: spec.private_mem_bytes,
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
            util_permille: 240,
            dedicated_used_bytes: 2 * 1024 * 1024 * 1024,
            dedicated_total_bytes: 8 * 1024 * 1024 * 1024,
            shared_used_bytes: 512 * 1024 * 1024,
            shared_total_bytes: 8 * 1024 * 1024 * 1024,
            d3_asleep: false,
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
}

struct MockSlowProber {
    state: Arc<MockState>,
    by_key: HashMap<ProcessKey, Option<String>>,
}

impl SlowMetadataProber for MockSlowProber {
    fn probe_process(&self, key: ProcessKey) -> Result<ProcessColdMeta, PalError> {
        self.state.probe_proc_calls.fetch_add(1, Ordering::AcqRel);
        if self.state.slow_probe_block_pid == Some(key.pid) {
            if let Some(ref rx) = self.state.slow_probe_release {
                let _ = rx.recv_timeout(self.state.slow_probe_sleep);
            } else if !self.state.slow_probe_sleep.is_zero() {
                std::thread::sleep(self.state.slow_probe_sleep);
            }
        }
        let exe_path = self
            .by_key
            .get(&key)
            .cloned()
            .flatten()
            .map(Arc::<str>::from);

        Ok(ProcessColdMeta {
            key,
            exe_path,
            command_line: Some(Arc::from("--mock-arg")),
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
                core_kinds: Vec::new(),
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
}

struct MockServices {
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

struct MockStartup {
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

struct MockSessions {
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

struct MockHistory {
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
}

struct MockOperator {
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
            .map(|p| (p.key, p.exe_path.clone()))
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

fn wait_until<F>(timeout: Duration, mut cond: F) -> bool
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

#[test]
fn startup_calibration_and_400_process_scan_latency() {
    // 1. Verify startup calibration tick produces accurate non-zero CPU% & Disk B/s.
    let mut busy = MockProcSpec::new(101, 1_000, 0, "busy.exe");
    busy.category = ProcessCategory::App;
    // 250ms tick (2_500_000 ticks) on 4 logical CPUs => 10_000_000 ticks == 1000 permille.
    // Give 2_500_000 CPU ticks/tick => 250 permille (25.0%) and 1_000_000 bytes/250ms => 4_000_000 B/s.
    busy.cpu_delta_per_tick = 2_500_000;
    busy.disk_delta_per_tick = 1_000_000;

    let state = MockState::new(vec![busy]);
    let backend = MockBackend::from_state(Arc::clone(&state));
    let config = EngineConfig {
        startup_calibration_delay: Duration::from_millis(30),
        ..EngineConfig::default()
    };

    let started = Instant::now();
    let coord = EngineCoordinator::with_config(backend, AppSettings::default(), config).unwrap();

    // First frame published rapidly.
    assert!(wait_until(Duration::from_millis(200), || {
        coord.snapshot().revisions.processes >= 1
    }));
    let first_frame_elapsed = started.elapsed();
    assert!(
        first_frame_elapsed < Duration::from_millis(200),
        "first frame took too long: {first_frame_elapsed:?}"
    );

    // Wait for calibration Tick 1 (~30 ms) to compute CPU% and Disk B/s deltas.
    assert!(wait_until(Duration::from_millis(400), || {
        let snap = coord.snapshot();
        snap.processes
            .rows
            .iter()
            .any(|r| r.key.pid == 101 && r.cpu_permille == 250 && r.disk_bytes_per_sec == 4_000_000)
    }));

    // 2. Verify steady-state scan + projection latency with 400 synthetic processes (< 2 ms).
    let mut scratch = ProcessSampleScratch::<()>::with_capacity(400, 16 * 1024);
    let mut proj = ProjectionEngine::new();
    let cold = loom_engine::ColdMetaCache::new();
    let view_cfg = ViewProjectionConfig::default();

    let fill_400 = |s: &mut ProcessSampleScratch<()>, tick: u64| {
        s.clear();
        s.perf.logical_cpu_count = 16;
        s.perf.mem_total_bytes = 32 * 1024 * 1024 * 1024;
        for i in 1..=400u32 {
            let (name_offset, name_len) = s.push_str(match i % 4 {
                0 => "chrome.exe",
                1 => "svchost.exe",
                2 => "code.exe",
                _ => "worker.exe",
            });
            s.samples.push(ProcessHotSample {
                key: ProcessKey::new(i * 4, u64::from(i) * 100),
                parent_pid: if i > 10 { (i % 10 + 1) * 4 } else { 0 },
                kernel_time_100ns: tick * u64::from(i) * 1_000,
                user_time_100ns: tick * u64::from(i) * 2_000,
                private_working_set_bytes: u64::from(i) * 1024 * 1024,
                disk_read_bytes: tick * u64::from(i) * 512,
                category: match i % 3 {
                    0 => ProcessCategory::App,
                    1 => ProcessCategory::Background,
                    _ => ProcessCategory::System,
                },
                name_offset,
                name_len,
                ..ProcessHotSample::default()
            });
        }
    };

    // Warm up caches & string interner (Tick 0).
    fill_400(&mut scratch, 0);
    proj.ingest_process_sample(Timestamp100Ns::new(100_000_000), &scratch, 16);
    let _ = proj.project_processes(&view_cfg, &cold, 1);

    // Measure 20 steady-state ticks and check best/median per-tick latency.
    let mut durations = Vec::with_capacity(20);
    for tick in 1..=20u64 {
        fill_400(&mut scratch, tick);
        let t = Instant::now();
        proj.ingest_process_sample(
            Timestamp100Ns::new(100_000_000 + tick * 10_000_000),
            &scratch,
            16,
        );
        let slice = proj.project_processes(&view_cfg, &cold, tick + 1);
        let elapsed = t.elapsed();
        assert_eq!(slice.total_process_count, 400);
        durations.push(elapsed);
    }
    durations.sort();
    let median = durations[durations.len() / 2];
    assert!(
        median < Duration::from_millis(2),
        "400-process ingest+projection median latency {median:?} exceeded 2 ms budget (all: {durations:?})"
    );
}

#[test]
fn pid_reuse_and_mutual_cycle_never_loop_and_promote_orphan_to_root() {
    // PID 100 was recycled at t=5000, AFTER child PID 200 was created at t=1000.
    let recycled_pid100 = MockProcSpec::new(100, 5_000, 0, "recycled.exe");
    let orphan_child200 = MockProcSpec::new(200, 1_000, 100, "orphan_child.exe");
    // Mutual cycle A <-> B created at the exact same timestamp t=2000.
    let cycle_a = MockProcSpec::new(301, 2_000, 302, "cycle_a.exe");
    let cycle_b = MockProcSpec::new(302, 2_000, 301, "cycle_b.exe");

    let state = MockState::new(vec![recycled_pid100, orphan_child200, cycle_a, cycle_b]);
    let backend = MockBackend::from_state(state);
    let settings = AppSettings {
        process_view_mode: ProcessViewMode::Tree,
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };

    let coord = EngineCoordinator::new(backend, settings).unwrap();
    let _details_lease = coord.acquire_lease(MonitorDomain::Details);
    coord
        .send_command_with_reply(EngineCommand::SetDetailsViewMode(DetailsViewMode::Tree))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();

    // Expand all nodes so any cycle would immediately manifest if unguarded.
    for pid_key in [
        ProcessKey::new(100, 5_000),
        ProcessKey::new(200, 1_000),
        ProcessKey::new(301, 2_000),
        ProcessKey::new(302, 2_000),
    ] {
        coord
            .send_command_with_reply(EngineCommand::ToggleProcessExpanded(pid_key))
            .unwrap()
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        coord
            .send_command_with_reply(EngineCommand::ToggleDetailsExpanded(pid_key))
            .unwrap()
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
    }

    let snap = coord.snapshot();
    assert_eq!(snap.processes.rows.len(), 4);
    assert_eq!(snap.details.rows.len(), 4);

    let orphan_row = snap
        .processes
        .rows
        .iter()
        .find(|r| r.key == ProcessKey::new(200, 1_000))
        .expect("orphan child must be present");
    assert_eq!(
        orphan_row.depth, 0,
        "child pointing to recycled newer parent PID must be promoted to root"
    );

    let orphan_details = snap
        .details
        .rows
        .iter()
        .find(|r| r.key == ProcessKey::new(200, 1_000))
        .expect("orphan child must be present in details");
    assert_eq!(orphan_details.parent_key, None);
    assert_eq!(orphan_details.depth, 0);

    // Both cycle participants appear once with zero infinite recursion.
    assert_eq!(
        snap.processes
            .rows
            .iter()
            .filter(|r| r.key.pid == 301 || r.key.pid == 302)
            .count(),
        2
    );
}

#[test]
fn terminate_process_tree_executes_bottom_up_leaves_before_parents() {
    let root = MockProcSpec::new(10, 100, 0, "root_watchdog.exe");
    let mid = MockProcSpec::new(20, 200, 10, "mid_supervisor.exe");
    let leaf1 = MockProcSpec::new(30, 300, 20, "leaf1.exe");
    let leaf2 = MockProcSpec::new(31, 301, 20, "leaf2.exe");

    let root_key = root.key;
    let mid_key = mid.key;
    let leaf1_key = leaf1.key;
    let leaf2_key = leaf2.key;

    let state = MockState::new(vec![root, mid, leaf1, leaf2]);
    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };

    let coord = EngineCoordinator::new(backend, settings).unwrap();
    let reply_rx = coord
        .send_command_with_reply(EngineCommand::TerminateProcessTree(root_key))
        .unwrap();
    let event = reply_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    assert_eq!(event.kind, CommandKind::TerminateProcessTree);
    assert!(event.is_ok());
    let Some(CommandSuccess::ProcessTreeTerminated(report)) = event.outcome.as_ok() else {
        panic!("expected ProcessTreeTerminated report");
    };
    assert_eq!(report.root, root_key);
    assert!(report.all_succeeded());

    let recorded = state.recorded_tree_kills.lock();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0], vec![leaf1_key, leaf2_key, mid_key, root_key]);
}

#[test]
fn slow_disk_5s_fault_injection_never_blocks_fast_lane_or_shutdown() {
    let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);

    let mut slow_proc = MockProcSpec::new(500, 1_000, 0, "slow_disk_app.exe");
    slow_proc.exe_path = Some(r"Z:\SlowShare\slow_disk_app.exe".to_owned());
    let fast_proc = MockProcSpec::new(501, 1_001, 0, "fast_app.exe");

    let state = Arc::new(MockState {
        procs: vec![slow_proc, fast_proc],
        current_ts: 100_000_000,
        ts_step_100ns: 2_500_000,
        logical_cpus: 4,
        mem_total_bytes: 16 * 1024 * 1024 * 1024,
        sample_proc_calls: AtomicUsize::new(0),
        sample_perf_calls: AtomicUsize::new(0),
        probe_proc_calls: AtomicUsize::new(0),
        probe_asset_calls: AtomicUsize::new(0),
        probe_asset_slow_calls: AtomicUsize::new(0),
        probe_hw_calls: AtomicUsize::new(0),
        probe_disk_calls: AtomicUsize::new(0),
        disk_probe_sleep: Duration::ZERO,
        exe_asset_slow_prefix: None,
        exe_asset_sleep: Duration::ZERO,
        slow_probe_block_pid: Some(500),
        slow_probe_sleep: Duration::from_secs(5),
        slow_probe_release: Some(release_rx),
        recorded_tree_kills: Mutex::new(Vec::new()),
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
    });

    let backend = MockBackend::from_state(Arc::clone(&state));
    let config = EngineConfig {
        startup_calibration_delay: Duration::from_millis(20),
        slow_shutdown_timeout: Duration::from_millis(30),
        ..EngineConfig::default()
    };
    let coord = EngineCoordinator::with_config(backend, AppSettings::default(), config).unwrap();

    // Ensure the slow worker has entered the 5-second blocking probe.
    assert!(wait_until(Duration::from_millis(500), || {
        state.probe_proc_calls.load(Ordering::Acquire) >= 1
    }));

    // While slow worker is blocked for 5 seconds, fast lane keeps publishing fresh snapshots rapidly!
    let rev_before = coord.snapshot().revisions.processes;
    let mut scan_latencies = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        coord
            .send_command_with_reply(EngineCommand::ForceRefresh)
            .unwrap()
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        scan_latencies.push(t.elapsed());
    }
    let rev_after = coord.snapshot().revisions.processes;
    assert!(
        rev_after >= rev_before + 5,
        "fast lane must stay unblocked while slow probe stalls"
    );
    scan_latencies.sort();
    let median = scan_latencies[scan_latencies.len() / 2];
    assert!(
        median < Duration::from_millis(10),
        "fast lane ForceRefresh roundtrip should stay fast even while slow worker blocks (median: {median:?})"
    );

    // Shutdown must complete quickly (bounded by slow_shutdown_timeout) instead of hanging 5 seconds!
    let shutdown_start = Instant::now();
    coord.shutdown();
    assert!(
        shutdown_start.elapsed() < Duration::from_millis(500),
        "shutdown should detach hung slow probe instead of waiting 5s"
    );

    // Cleanly unblock the detached mock worker so the test process exits tidy.
    let _ = release_tx.try_send(());
}

#[test]
fn monitor_lease_raii_gates_details_and_performance_domains() {
    let state = MockState::new(vec![MockProcSpec::new(10, 100, 0, "app.exe")]);
    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };

    let coord = EngineCoordinator::new(backend, settings).unwrap();

    // Trigger several fast-lane ticks with NO Details or Performance lease held.
    for _ in 0..3 {
        coord
            .send_command_with_reply(EngineCommand::ForceRefresh)
            .unwrap()
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
    }

    let snap_unleased = coord.snapshot();
    assert_eq!(snap_unleased.revisions.details, 0);
    assert!(snap_unleased.details.rows.is_empty());
    assert_eq!(
        state.sample_perf_calls.load(Ordering::Acquire),
        0,
        "full sample_performance should not run without Performance/PerCoreCpu lease"
    );

    // Acquire Details lease -> wakes coordinator immediately and projects DetailsTableSlice!
    let details_lease = coord.acquire_lease(MonitorDomain::Details);
    assert!(wait_until(Duration::from_millis(300), || {
        coord.snapshot().revisions.details >= 1
    }));
    let snap_leased_1 = coord.snapshot();
    assert_eq!(snap_leased_1.details.rows.len(), 1);
    let rev_leased_1 = snap_leased_1.revisions.details;

    // Subsequent tick while leased continues incrementing details revision.
    coord
        .send_command_with_reply(EngineCommand::ForceRefresh)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let snap_leased_2 = coord.snapshot();
    assert!(snap_leased_2.revisions.details > rev_leased_1);

    // Drop Details lease -> subsequent ticks preserve unchanged Arc<DetailsTableSlice> and revision!
    drop(details_lease);
    let frozen_snap = coord.snapshot();
    let frozen_rev = frozen_snap.revisions.details;

    for _ in 0..3 {
        coord
            .send_command_with_reply(EngineCommand::ForceRefresh)
            .unwrap()
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
    }
    let after_drop_snap = coord.snapshot();
    assert_eq!(after_drop_snap.revisions.details, frozen_rev);
    assert!(Arc::ptr_eq(&frozen_snap.details, &after_drop_snap.details));

    // Acquiring Performance first (and waiting for its wakeup sample to land) and then
    // acquiring PerCoreCpu on the same paused tick must still sample and publish per-core data.
    let _perf_lease = coord.acquire_lease(MonitorDomain::Performance);
    assert!(wait_until(Duration::from_millis(300), || {
        state.sample_perf_calls.load(Ordering::Acquire) >= 1
    }));
    assert!(
        coord.snapshot().performance.per_core_permille.is_empty(),
        "per-core data must stay empty while only Performance lease is held"
    );

    let _core_lease = coord.acquire_lease(MonitorDomain::PerCoreCpu);
    assert!(wait_until(Duration::from_millis(300), || {
        state.sample_perf_calls.load(Ordering::Acquire) >= 2
            && coord.snapshot().performance.per_core_permille.len() == 4
    }));
}

#[test]
fn window_dormancy_suspends_and_resumes_periodic_sampling() {
    let state = MockState::new(vec![MockProcSpec::new(10, 100, 0, "app.exe")]);
    let backend = MockBackend::from_state(Arc::clone(&state));
    let config = EngineConfig {
        startup_calibration_delay: Duration::from_millis(20),
        ..EngineConfig::default()
    };
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::High500Ms,
        ..AppSettings::default()
    };
    let coord = EngineCoordinator::with_config(backend, settings, config).unwrap();

    // Wait for initial tick + calibration tick.
    assert!(wait_until(Duration::from_millis(300), || {
        state.sample_proc_calls.load(Ordering::Acquire) >= 2
    }));

    // Enter dormancy: periodic sampling must stop completely.
    coord
        .send_command_with_reply(EngineCommand::SetWindowDormant(true))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let samples_when_dormant = state.sample_proc_calls.load(Ordering::Acquire);
    let rev_when_dormant = coord.snapshot().revisions.processes;

    // Wait longer than High500Ms interval -> zero new samples!
    std::thread::sleep(Duration::from_millis(650));
    assert_eq!(
        state.sample_proc_calls.load(Ordering::Acquire),
        samples_when_dormant
    );
    assert_eq!(coord.snapshot().revisions.processes, rev_when_dormant);

    // Exit dormancy: immediately triggers a fresh sample tick!
    coord
        .send_command_with_reply(EngineCommand::SetWindowDormant(false))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert!(state.sample_proc_calls.load(Ordering::Acquire) > samples_when_dormant);
    assert!(coord.snapshot().revisions.processes > rev_when_dormant);
}

#[test]
fn elevation_required_error_propagates_to_event_subscribers() {
    let target = ProcessKey::new(4, 10);
    let state = MockState::new(vec![MockProcSpec::new(4, 10, 0, "System")]);
    state
        .terminate_error_elevation
        .store(true, Ordering::Release);

    let backend = MockBackend::from_state(state);
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };
    let coord = EngineCoordinator::new(backend, settings).unwrap();
    let events_rx = coord.subscribe_events();

    coord
        .send_command(EngineCommand::TerminateProcess(target))
        .unwrap();

    let event = events_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(event.kind, CommandKind::TerminateProcess);
    assert_eq!(event.target, Some(target));
    assert!(!event.is_ok());
    assert!(event.is_elevation_required());
    assert!(matches!(event.error(), Some(PalError::ElevationRequired)));
}

#[test]
fn slow_lane_deduplicates_exe_asset_across_30_chrome_instances() {
    let procs: Vec<MockProcSpec> = (1..=30u32)
        .map(|i| {
            let mut p = MockProcSpec::new(1000 + i, 500 + u64::from(i), 0, "chrome.exe");
            p.category = ProcessCategory::App;
            p.exe_path = Some(r"C:\Program Files\Google\Chrome\Application\chrome.exe".to_owned());
            p
        })
        .collect();

    let state = MockState::new(procs);
    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };

    let coord = EngineCoordinator::new(backend, settings).unwrap();

    // Wait for all 30 processes to receive their shared publisher & icon metadata.
    assert!(wait_until(Duration::from_secs(2), || {
        let snap = coord.snapshot();
        let populated = snap
            .processes
            .rows
            .iter()
            .filter(|r| {
                r.kind == ProcessRowKind::Process
                    && r.publisher.as_deref() == Some("Mock Publisher")
                    && r.icon.is_some()
            })
            .count();
        populated == 30
    }));

    assert_eq!(
        state.probe_asset_calls.load(Ordering::Acquire),
        1,
        "30 instances of the same executable must share a single probe_exe_asset call"
    );

    // Also verify Grouped mode section header & sorting commands.
    coord
        .send_command_with_reply(EngineCommand::SetProcessSort {
            column: ProcessSortColumn::Name,
            direction: SortDirection::Ascending,
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();

    let snap = coord.snapshot();
    assert!(matches!(
        snap.processes.rows.first().map(|r| r.kind),
        Some(ProcessRowKind::SectionHeader {
            category: ProcessCategory::App,
            count: 30
        })
    ));
}

#[test]
fn efficiency_mode_priority_affinity_browse_and_cold_projection_fields() {
    let mut app = MockProcSpec::new(2001, 50_000, 0, "editor.exe");
    app.category = ProcessCategory::App;
    app.exe_path = Some(r"C:\Apps\Editor\editor.exe".to_owned());
    app.disk_delta_per_tick = 750_000;

    let mut helper = MockProcSpec::new(2002, 50_010, 0, "helper.exe");
    helper.category = ProcessCategory::Background;
    helper.disk_delta_per_tick = 250_000;

    let app_key = app.key;
    let state = MockState::new(vec![app, helper]);
    state.is_elevated.store(true, Ordering::Release);
    *state.browse_result.lock() = Some(PathBuf::from(r"C:\Windows\System32\notepad.exe"));

    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };
    let coord = EngineCoordinator::new(backend, settings).unwrap();
    let _details_lease = coord.acquire_lease(MonitorDomain::Details);

    // Wait for slow-lane ColdMeta & ExeStaticAsset to populate both Processes and Details projections.
    assert!(wait_until(Duration::from_secs(2), || {
        let snap = coord.snapshot();
        let proc_ready = snap.processes.rows.iter().any(|r| {
            r.key == app_key
                && r.exe_path.as_deref() == Some(r"C:\Apps\Editor\editor.exe")
                && r.status.contains(ProcessStatusFlags::ELEVATED)
        });
        let details_ready = snap.details.rows.iter().any(|r| {
            r.key == app_key
                && r.publisher.as_deref() == Some("Mock Publisher")
                && r.status.contains(ProcessStatusFlags::ELEVATED)
        });
        proc_ready && details_ready
    }));

    // 1. Trigger a ForceRefresh tick so disk throughput deltas & disk_permille are computed (75% vs 25%).
    coord
        .send_command_with_reply(EngineCommand::ForceRefresh)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let snap_disk = coord.snapshot();
    let app_row = snap_disk
        .processes
        .rows
        .iter()
        .find(|r| r.key == app_key)
        .expect("app row present");
    assert_eq!(app_row.disk_permille, 750);
    assert_eq!(app_row.net_permille, 0);

    // 2. SetEfficiencyMode(true) IMMEDIATELY sets EFFICIENCY_MODE in both Processes and Details
    //    without waiting for a slow-lane re-probe, and SetEfficiencyMode(false) clears it.
    let eco_on = coord
        .send_command_with_reply(EngineCommand::SetEfficiencyMode {
            key: app_key,
            enabled: true,
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert_eq!(eco_on.kind, CommandKind::SetEfficiencyMode);
    assert!(eco_on.is_ok());

    let snap_on = coord.snapshot();
    assert!(
        snap_on
            .processes
            .rows
            .iter()
            .find(|r| r.key == app_key)
            .unwrap()
            .status
            .contains(ProcessStatusFlags::EFFICIENCY_MODE)
    );
    assert!(
        snap_on
            .details
            .rows
            .iter()
            .find(|r| r.key == app_key)
            .unwrap()
            .status
            .contains(ProcessStatusFlags::EFFICIENCY_MODE)
    );

    let eco_off = coord
        .send_command_with_reply(EngineCommand::SetEfficiencyMode {
            key: app_key,
            enabled: false,
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert!(eco_off.is_ok());
    let snap_off = coord.snapshot();
    assert!(
        !snap_off
            .processes
            .rows
            .iter()
            .find(|r| r.key == app_key)
            .unwrap()
            .status
            .contains(ProcessStatusFlags::EFFICIENCY_MODE)
    );
    assert!(
        !snap_off
            .details
            .rows
            .iter()
            .find(|r| r.key == app_key)
            .unwrap()
            .status
            .contains(ProcessStatusFlags::EFFICIENCY_MODE)
    );

    // 3. SetPriority immediately triggers a fast refresh so Details reflects the updated priority.
    let prio_ev = coord
        .send_command_with_reply(EngineCommand::SetPriority {
            key: app_key,
            priority: ProcessPriority::High,
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert_eq!(prio_ev.kind, CommandKind::SetPriority);
    assert!(prio_ev.is_ok());
    assert_eq!(
        coord
            .snapshot()
            .details
            .rows
            .iter()
            .find(|r| r.key == app_key)
            .unwrap()
            .priority,
        ProcessPriority::High
    );

    // 4. GetAffinity returns CommandSuccess::Affinity matching host logical_cpus.
    let aff_ev = coord
        .send_command_with_reply(EngineCommand::GetAffinity(app_key))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert_eq!(aff_ev.kind, CommandKind::GetAffinity);
    assert_eq!(aff_ev.target, Some(app_key));
    assert_eq!(
        aff_ev.outcome.as_ok(),
        Some(&CommandSuccess::Affinity(CpuAffinityMask::all(4)))
    );

    // 5. BrowseExecutable runs on the slow lane and returns CommandSuccess::FilePicked.
    let browse_ev = coord
        .send_command_with_reply(EngineCommand::BrowseExecutable)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert_eq!(browse_ev.kind, CommandKind::BrowseExecutable);
    assert_eq!(
        browse_ev.outcome.as_ok(),
        Some(&CommandSuccess::FilePicked(Some(PathBuf::from(
            r"C:\Windows\System32\notepad.exe"
        ))))
    );
}

#[test]
fn engine_coordinator_publishes_backend_platform_profile_in_snapshot() {
    struct ProfiledMockBackend<const IS_LINUX: bool>(MockBackend);

    impl<const IS_LINUX: bool> PlatformBackend for ProfiledMockBackend<IS_LINUX> {
        type ProcessExt = ();
        type FastSampler = MockFastSampler;
        type SlowProber = MockSlowProber;
        type ServiceMgr = MockServices;
        type StartupMgr = MockStartup;
        type SessionMgr = MockSessions;
        type HistoryReader = MockHistory;
        type Operator = MockOperator;

        const PLATFORM: PlatformProfile = if IS_LINUX {
            PlatformProfile::LINUX
        } else {
            PlatformProfile::WINDOWS
        };

        const CAPABILITIES: CapabilityFlags = CapabilityFlags::all();

        fn boost_self_priority() {}

        fn initialize() -> Result<Self, PalError> {
            Ok(Self(MockBackend::initialize()?))
        }

        fn create_fast_sampler(&self) -> Result<Self::FastSampler, PalError> {
            self.0.create_fast_sampler()
        }

        fn create_slow_prober(&self) -> Result<Self::SlowProber, PalError> {
            self.0.create_slow_prober()
        }

        fn service_manager(&self) -> &Self::ServiceMgr {
            self.0.service_manager()
        }

        fn startup_manager(&self) -> &Self::StartupMgr {
            self.0.startup_manager()
        }

        fn session_manager(&self) -> &Self::SessionMgr {
            self.0.session_manager()
        }

        fn history_reader(&self) -> &Self::HistoryReader {
            self.0.history_reader()
        }

        fn operator(&self) -> &Self::Operator {
            self.0.operator()
        }
    }

    let win_coord = EngineCoordinator::new(
        ProfiledMockBackend::<false>(MockBackend::initialize().unwrap()),
        AppSettings::default(),
    )
    .unwrap();
    assert_eq!(win_coord.snapshot().platform, PlatformProfile::WINDOWS);

    let linux_coord = EngineCoordinator::new(
        ProfiledMockBackend::<true>(MockBackend::initialize().unwrap()),
        AppSettings::default(),
    )
    .unwrap();
    assert_eq!(linux_coord.snapshot().platform, PlatformProfile::LINUX);

    let unknown_coord =
        EngineCoordinator::new(MockBackend::initialize().unwrap(), AppSettings::default()).unwrap();
    assert_eq!(unknown_coord.snapshot().platform, PlatformProfile::UNKNOWN);
}

#[test]
fn performance_lease_gates_hardware_and_disk_polling_and_populates_60_point_histories() {
    let state = MockState::new(vec![MockProcSpec::new(10, 100, 0, "app.exe")]);
    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };

    let coord = EngineCoordinator::new(backend, settings).unwrap();

    // Run 3 fast-lane ticks without Performance or PerCoreCpu lease -> 0 disk / HW static probes.
    for _ in 0..3 {
        coord
            .send_command_with_reply(EngineCommand::ForceRefresh)
            .unwrap()
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
    }
    assert_eq!(
        state.probe_hw_calls.load(Ordering::Acquire),
        0,
        "hardware static info must not be probed when Performance is unleased"
    );
    assert_eq!(
        state.probe_disk_calls.load(Ordering::Acquire),
        0,
        "disk performance must not be polled when Performance is unleased"
    );
    assert_eq!(state.sample_perf_calls.load(Ordering::Acquire), 0);

    // Acquire both Performance and PerCoreCpu leases -> wakes coordinator immediately.
    let perf_lease = coord.acquire_lease(MonitorDomain::Performance);
    let core_lease = coord.acquire_lease(MonitorDomain::PerCoreCpu);

    assert!(
        wait_until(Duration::from_secs(2), || {
            let snap = coord.snapshot();
            !snap.performance.cpu_info.brand_name.is_empty()
                && !snap.performance.disks.is_empty()
                && !snap.performance.networks.is_empty()
                && !snap.performance.gpus.is_empty()
                && !snap.performance.per_core_history.is_empty()
        }),
        "Performance + PerCoreCpu lease must populate static hardware, disk, network, GPU, and per-core slices"
    );

    // Drive 65 ticks so every 60-point ring wraps cleanly at capacity 60.
    for _ in 0..65 {
        let prev_disk_polls = state.probe_disk_calls.load(Ordering::Acquire);
        coord
            .send_command_with_reply(EngineCommand::ForceRefresh)
            .unwrap()
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        // Wait for the slow-lane disk poll for this tick to settle so disk rings advance in lockstep.
        let _ = wait_until(Duration::from_millis(200), || {
            state.probe_disk_calls.load(Ordering::Acquire) > prev_disk_polls
        });
    }

    // Wait for final disk result to be applied to snapshot.
    assert!(wait_until(Duration::from_secs(1), || {
        let snap = coord.snapshot();
        snap.performance
            .disks
            .first()
            .is_some_and(|d| d.active_history.len() == 60)
    }));

    let snap = coord.snapshot();
    let perf = &snap.performance;

    // Hardware static info probed exactly ONCE.
    assert_eq!(state.probe_hw_calls.load(Ordering::Acquire), 1);
    assert_eq!(perf.cpu_info.brand_name.as_ref(), "Mock Ryzen 9 9950X");
    assert_eq!(perf.memory_info.speed_mt_s, Some(6000));
    assert_eq!(perf.memory_info.slots_used, Some(2));
    assert_eq!(perf.memory_info.slots_total, Some(4));
    assert_eq!(perf.memory_info.form_factor.as_deref(), Some("DIMM"));

    // 60-point histories populated for CPU, Memory, Per-Core CPU, Disk, Network, and GPU + engines.
    assert_eq!(perf.cpu_history.len(), 60);
    assert_eq!(perf.cpu_kernel_history.len(), 60);
    assert_eq!(perf.mem_history.len(), 60);
    assert_eq!(perf.per_core_history.len(), 4);
    assert!(perf.per_core_history.iter().all(|r| r.len() == 60));

    assert_eq!(perf.disks.len(), 1);
    assert_eq!(perf.disks[0].active_history.len(), 60);
    assert_eq!(perf.disks[0].read_history.len(), 60);
    assert_eq!(perf.disks[0].write_history.len(), 60);
    assert!(perf.latest.disk_read_bytes_per_sec >= 10_000_000);
    assert!(perf.latest.disk_write_bytes_per_sec >= 5_000_000);

    assert_eq!(perf.networks.len(), 1);
    assert_eq!(perf.networks[0].recv_history.len(), 60);
    assert_eq!(perf.networks[0].sent_history.len(), 60);
    assert!(perf.networks[0].recv_history.max_value() >= 125_000);

    assert_eq!(perf.gpus.len(), 1);
    assert_eq!(perf.gpus[0].util_history.len(), 60);
    assert_eq!(perf.gpus[0].dedicated_mem_history.len(), 60);
    assert_eq!(perf.gpus[0].shared_mem_history.len(), 60);
    assert_eq!(perf.gpus[0].engines.len(), 2);
    assert!(perf.gpus[0].engines.iter().all(|e| e.history.len() == 60));

    // Drop leases -> subsequent ticks stop disk and performance sampling.
    drop(perf_lease);
    drop(core_lease);
    let disk_before = state.probe_disk_calls.load(Ordering::Acquire);
    let perf_before = state.sample_perf_calls.load(Ordering::Acquire);
    for _ in 0..3 {
        coord
            .send_command_with_reply(EngineCommand::ForceRefresh)
            .unwrap()
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
    }
    assert_eq!(state.probe_disk_calls.load(Ordering::Acquire), disk_before);
    assert_eq!(state.sample_perf_calls.load(Ordering::Acquire), perf_before);
}

#[test]
fn slow_disk_probe_400ms_never_blocks_fast_lane_and_deduplicates_polls() {
    let state = Arc::new(MockState {
        procs: vec![MockProcSpec::new(20, 100, 0, "worker.exe")],
        current_ts: 100_000_000,
        ts_step_100ns: 2_500_000,
        logical_cpus: 4,
        mem_total_bytes: 16 * 1024 * 1024 * 1024,
        sample_proc_calls: AtomicUsize::new(0),
        sample_perf_calls: AtomicUsize::new(0),
        probe_proc_calls: AtomicUsize::new(0),
        probe_asset_calls: AtomicUsize::new(0),
        probe_asset_slow_calls: AtomicUsize::new(0),
        probe_hw_calls: AtomicUsize::new(0),
        probe_disk_calls: AtomicUsize::new(0),
        disk_probe_sleep: Duration::from_millis(400),
        exe_asset_slow_prefix: None,
        exe_asset_sleep: Duration::ZERO,
        slow_probe_block_pid: None,
        slow_probe_sleep: Duration::ZERO,
        slow_probe_release: None,
        recorded_tree_kills: Mutex::new(Vec::new()),
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
    });

    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };
    let config = EngineConfig {
        slow_shutdown_timeout: Duration::from_millis(50),
        ..EngineConfig::default()
    };
    let coord = EngineCoordinator::with_config(backend, settings, config).unwrap();
    let _perf_lease = coord.acquire_lease(MonitorDomain::Performance);

    // Wait for the slow worker to start executing the 400ms disk probe.
    assert!(wait_until(Duration::from_millis(500), || {
        state.probe_disk_calls.load(Ordering::Acquire) >= 1
    }));

    // While the 400ms disk probe is in flight, run 5 fast-lane ForceRefresh ticks rapidly.
    let rev_before = coord.snapshot().revisions.processes;
    let started = Instant::now();
    for _ in 0..5 {
        coord
            .send_command_with_reply(EngineCommand::ForceRefresh)
            .unwrap()
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
    }
    let fast_elapsed = started.elapsed();
    let rev_after = coord.snapshot().revisions.processes;

    assert!(
        rev_after >= rev_before + 5,
        "fast-lane ticks must keep advancing while 400ms disk probe runs"
    );
    assert!(
        fast_elapsed < Duration::from_millis(200),
        "5 fast-lane ticks should complete far faster than 400ms disk probe (took {fast_elapsed:?})"
    );
    // Deduplication via `pending_disk_perf`: still only 1 in-flight disk poll during the 400ms window!
    assert_eq!(
        state.probe_disk_calls.load(Ordering::Acquire),
        1,
        "pending_disk_perf must prevent queueing duplicate disk polls while one is in flight"
    );

    // Wait for the 400ms disk poll to finish and publish into PerformanceSlice.
    assert!(wait_until(Duration::from_secs(2), || {
        !coord.snapshot().performance.disks.is_empty()
    }));
}

#[test]
fn system_mgmt_domains_enforce_strict_raii_lease_gating() {
    let state = MockState::new(vec![MockProcSpec::new(10, 100, 0, "app.exe")]);
    state.mgmt.services.lock().push(ServiceRecord {
        name: Arc::from("Spooler"),
        display_name: Arc::from("Print Spooler"),
        description: Some(Arc::from("Loads files to memory for later printing")),
        pid: Some(10),
        state: ServiceState::Running,
        group: Some(Arc::from("netsvcs")),
    });
    state.mgmt.startup_entries.lock().push(StartupEntry {
        id: Arc::from("hkcu-run:Loom"),
        name: Arc::from("Loom"),
        publisher: Some(Arc::from("Loom")),
        command: Some(Arc::from(r"C:\Loom\loom.exe")),
        file_path: Some(Arc::from(r"C:\Loom\loom.exe")),
        enabled: true,
        impact: StartupImpact::NotMeasured,
    });
    state.mgmt.sessions.lock().push(UserSessionRecord {
        session_id: 1,
        user_name: Arc::from("HOST\\Alice"),
        state: UserSessionState::Active,
        process_count: 0,
        cpu_permille: 0,
        memory_bytes: 0,
        disk_bytes_per_sec: 0,
        net_bytes_per_sec: 0,
        processes: Vec::new(),
    });
    *state.mgmt.history_since.lock() = Some(1_700_000_000);

    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };
    let coord = EngineCoordinator::new(backend, settings).unwrap();

    // Run multiple ticks with NO leases active -> ZERO polls on any of the 4 domains!
    for _ in 0..3 {
        coord
            .send_command_with_reply(EngineCommand::ForceRefresh)
            .unwrap()
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
    }
    std::thread::sleep(Duration::from_millis(30));

    assert_eq!(state.mgmt.list_services_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.mgmt.list_startup_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.mgmt.list_sessions_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.mgmt.read_history_calls.load(Ordering::Acquire), 0);

    // Acquire each lease one by one and confirm ONLY that domain polls and populates snapshot.
    let svc_lease = coord.acquire_lease(MonitorDomain::Services);
    assert!(wait_until(Duration::from_secs(1), || {
        coord.snapshot().revisions.services >= 1
    }));
    assert_eq!(coord.snapshot().services.items.len(), 1);
    assert_eq!(state.mgmt.list_startup_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.mgmt.list_sessions_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.mgmt.read_history_calls.load(Ordering::Acquire), 0);
    drop(svc_lease);

    let startup_lease = coord.acquire_lease(MonitorDomain::Startup);
    assert!(wait_until(Duration::from_secs(1), || {
        coord.snapshot().revisions.startup >= 1
    }));
    assert_eq!(coord.snapshot().startup.items.len(), 1);
    assert_eq!(coord.snapshot().startup.last_bios_time_ms, Some(2_100));
    drop(startup_lease);

    let users_lease = coord.acquire_lease(MonitorDomain::Users);
    assert!(wait_until(Duration::from_secs(1), || {
        coord.snapshot().revisions.users >= 1
    }));
    assert_eq!(coord.snapshot().users.sessions.len(), 1);
    drop(users_lease);

    let hist_lease = coord.acquire_lease(MonitorDomain::AppHistory);
    assert!(wait_until(Duration::from_secs(1), || {
        coord.snapshot().revisions.app_history >= 1
    }));
    assert_eq!(
        coord.snapshot().app_history.since_epoch_secs,
        Some(1_700_000_000)
    );
    drop(hist_lease);

    // After all leases are dropped, further refreshes trigger ZERO polls on all 4 domains.
    let counts_before = (
        state.mgmt.list_services_calls.load(Ordering::Acquire),
        state.mgmt.list_startup_calls.load(Ordering::Acquire),
        state.mgmt.list_sessions_calls.load(Ordering::Acquire),
        state.mgmt.read_history_calls.load(Ordering::Acquire),
    );
    for _ in 0..3 {
        coord
            .send_command_with_reply(EngineCommand::ForceRefresh)
            .unwrap()
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
    }
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(
        (
            state.mgmt.list_services_calls.load(Ordering::Acquire),
            state.mgmt.list_startup_calls.load(Ordering::Acquire),
            state.mgmt.list_sessions_calls.load(Ordering::Acquire),
            state.mgmt.read_history_calls.load(Ordering::Acquire),
        ),
        counts_before
    );
}

#[test]
fn users_session_enrichment_aggregates_live_processes_and_sorts_descending() {
    // Session 1 has 2 processes (busy_high + mem_heavy); Session 2 has 1 process (remote_app).
    let mut p1 = MockProcSpec::new(101, 1_000, 0, "busy_high.exe");
    p1.session_id = 1;
    p1.category = ProcessCategory::App;
    p1.cpu_delta_per_tick = 4_000_000; // 400 permille on 4 CPUs / 250ms tick
    p1.disk_delta_per_tick = 250_000; // 1_000_000 B/s
    p1.net_delta_per_tick = 100_000; // 400_000 B/s
    p1.private_mem_bytes = 64 * 1024 * 1024;

    let mut p2 = MockProcSpec::new(102, 1_001, 0, "mem_heavy.exe");
    p2.session_id = 1;
    p2.category = ProcessCategory::App;
    p2.cpu_delta_per_tick = 1_000_000; // 100 permille
    p2.disk_delta_per_tick = 500_000; // 2_000_000 B/s
    p2.net_delta_per_tick = 250_000; // 1_000_000 B/s
    p2.private_mem_bytes = 256 * 1024 * 1024;

    let mut p3 = MockProcSpec::new(201, 2_000, 0, "remote_app.exe");
    p3.session_id = 2;
    p3.category = ProcessCategory::App;
    p3.cpu_delta_per_tick = 2_000_000; // 200 permille
    p3.private_mem_bytes = 128 * 1024 * 1024;

    let state = MockState::new(vec![p1, p2, p3]);
    *state.mgmt.sessions.lock() = vec![
        UserSessionRecord {
            session_id: 1,
            user_name: Arc::from("HOST\\Alice"),
            state: UserSessionState::Active,
            process_count: 0,
            cpu_permille: 0,
            memory_bytes: 0,
            disk_bytes_per_sec: 0,
            net_bytes_per_sec: 0,
            processes: Vec::new(),
        },
        UserSessionRecord {
            session_id: 2,
            user_name: Arc::from("HOST\\Bob"),
            state: UserSessionState::Disconnected,
            process_count: 0,
            cpu_permille: 0,
            memory_bytes: 0,
            disk_bytes_per_sec: 0,
            net_bytes_per_sec: 0,
            processes: Vec::new(),
        },
    ];

    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };
    let coord = EngineCoordinator::new(backend, settings).unwrap();
    let _users_lease = coord.acquire_lease(MonitorDomain::Users);

    // Wait for initial session poll, then advance a tick so per-process deltas become non-zero.
    assert!(wait_until(Duration::from_secs(1), || {
        coord.snapshot().revisions.users >= 1
    }));
    coord
        .send_command_with_reply(EngineCommand::ForceRefresh)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();

    assert!(wait_until(Duration::from_secs(1), || {
        let snap = coord.snapshot();
        snap.users
            .sessions
            .iter()
            .any(|s| s.session_id == 1 && s.cpu_permille == 500)
    }));

    let snap = coord.snapshot();
    let s1 = snap
        .users
        .sessions
        .iter()
        .find(|s| s.session_id == 1)
        .expect("session 1");
    assert_eq!(s1.process_count, 2);
    assert_eq!(s1.cpu_permille, 500);
    assert_eq!(s1.memory_bytes, (64 + 256) * 1024 * 1024);
    assert_eq!(s1.disk_bytes_per_sec, 3_000_000);
    assert_eq!(s1.net_bytes_per_sec, 1_400_000);
    assert_eq!(s1.processes.len(), 2);
    // Sorted by CPU descending: busy_high (400) before mem_heavy (100).
    assert_eq!(s1.processes[0].name.as_ref(), "busy_high.exe");
    assert_eq!(s1.processes[0].cpu_permille, 400);
    assert_eq!(s1.processes[1].name.as_ref(), "mem_heavy.exe");
    assert_eq!(s1.processes[1].cpu_permille, 100);

    let s2 = snap
        .users
        .sessions
        .iter()
        .find(|s| s.session_id == 2)
        .expect("session 2");
    assert_eq!(s2.process_count, 1);
    assert_eq!(s2.cpu_permille, 200);
    assert_eq!(s2.memory_bytes, 128 * 1024 * 1024);
    assert_eq!(s2.processes.len(), 1);
    assert_eq!(s2.processes[0].name.as_ref(), "remote_app.exe");
}

#[test]
fn system_mgmt_commands_refresh_leased_snapshots_and_propagate_elevation_required() {
    let state = MockState::new(vec![MockProcSpec::new(10, 100, 0, "app.exe")]);
    state.mgmt.services.lock().push(ServiceRecord {
        name: Arc::from("wuauserv"),
        display_name: Arc::from("Windows Update"),
        description: None,
        pid: None,
        state: ServiceState::Stopped,
        group: Some(Arc::from("netsvcs")),
    });
    state.mgmt.startup_entries.lock().push(StartupEntry {
        id: Arc::from("hklm-run:SecurityHealth"),
        name: Arc::from("SecurityHealth"),
        publisher: Some(Arc::from("Microsoft Corporation")),
        command: Some(Arc::from(r"C:\Windows\System32\SecurityHealthSystray.exe")),
        file_path: Some(Arc::from(r"C:\Windows\System32\SecurityHealthSystray.exe")),
        enabled: true,
        impact: StartupImpact::NotMeasured,
    });
    state.mgmt.sessions.lock().push(UserSessionRecord {
        session_id: 2,
        user_name: Arc::from("HOST\\Guest"),
        state: UserSessionState::Active,
        process_count: 0,
        cpu_permille: 0,
        memory_bytes: 0,
        disk_bytes_per_sec: 0,
        net_bytes_per_sec: 0,
        processes: Vec::new(),
    });

    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };
    let coord = EngineCoordinator::new(backend, settings).unwrap();
    let _svc_lease = coord.acquire_lease(MonitorDomain::Services);
    let _startup_lease = coord.acquire_lease(MonitorDomain::Startup);
    let _users_lease = coord.acquire_lease(MonitorDomain::Users);

    assert!(wait_until(Duration::from_secs(1), || {
        let s = coord.snapshot();
        s.revisions.services >= 1 && s.revisions.startup >= 1 && s.revisions.users >= 1
    }));

    // 1. StartService -> immediately updates snapshot to Running!
    let ev = coord
        .send_command_with_reply(EngineCommand::StartService("wuauserv".into()))
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(ev.kind, CommandKind::StartService);
    assert!(ev.is_ok());
    assert_eq!(
        coord.snapshot().services.items[0].state,
        ServiceState::Running
    );
    let pid_after_start = coord.snapshot().services.items[0].pid;
    assert_eq!(pid_after_start, Some(4242));

    // 2. RestartService -> updates PID immediately!
    let ev = coord
        .send_command_with_reply(EngineCommand::RestartService("wuauserv".into()))
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(ev.kind, CommandKind::RestartService);
    assert!(ev.is_ok());
    assert_eq!(coord.snapshot().services.items[0].pid, Some(4243));

    // 3. StopService -> immediately updates snapshot to Stopped!
    let ev = coord
        .send_command_with_reply(EngineCommand::StopService("wuauserv".into()))
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(ev.kind, CommandKind::StopService);
    assert!(ev.is_ok());
    assert_eq!(
        coord.snapshot().services.items[0].state,
        ServiceState::Stopped
    );

    // 4. OpenServicesConsole -> invokes ServiceManager::open_services_console.
    let ev = coord
        .send_command_with_reply(EngineCommand::OpenServicesConsole)
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(ev.kind, CommandKind::OpenServicesConsole);
    assert!(ev.is_ok());
    assert_eq!(state.mgmt.open_console_calls.load(Ordering::Acquire), 1);

    // 5. SetStartupEnabled -> immediately reflects disabled state in StartupSlice!
    let ev = coord
        .send_command_with_reply(EngineCommand::SetStartupEnabled {
            id: "hklm-run:SecurityHealth".into(),
            enabled: false,
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(ev.kind, CommandKind::SetStartupEnabled);
    assert!(ev.is_ok());
    assert!(!coord.snapshot().startup.items[0].enabled);
    assert_eq!(
        coord.snapshot().startup.items[0].impact,
        StartupImpact::None
    );

    // 6. DisconnectSession -> immediately reflects Disconnected state in UsersSlice!
    let ev = coord
        .send_command_with_reply(EngineCommand::DisconnectSession(2))
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(ev.kind, CommandKind::DisconnectSession);
    assert!(ev.is_ok());
    assert_eq!(
        coord.snapshot().users.sessions[0].state,
        UserSessionState::Disconnected
    );

    // 7. Verify PalError::ElevationRequired propagation across service, startup, and session ops.
    state
        .mgmt
        .service_ops_require_elevation
        .store(true, Ordering::Release);
    state
        .mgmt
        .startup_ops_require_elevation
        .store(true, Ordering::Release);
    state
        .mgmt
        .session_ops_require_elevation
        .store(true, Ordering::Release);

    let svc_ev = coord
        .send_command_with_reply(EngineCommand::StartService("wuauserv".into()))
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert!(svc_ev.is_elevation_required());

    let startup_ev = coord
        .send_command_with_reply(EngineCommand::SetStartupEnabled {
            id: "hklm-run:SecurityHealth".into(),
            enabled: true,
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert!(startup_ev.is_elevation_required());

    let sess_ev = coord
        .send_command_with_reply(EngineCommand::DisconnectSession(2))
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert!(sess_ev.is_elevation_required());
}

#[test]
fn app_history_accumulates_live_deltas_merges_persisted_and_clears_atomically() {
    let mut app = MockProcSpec::new(501, 1_000, 0, "Browser.exe");
    app.category = ProcessCategory::App;
    app.cpu_delta_per_tick = 1_500_000;
    app.net_delta_per_tick = 200_000;

    let mut sys = MockProcSpec::new(4, 1, 0, "System");
    sys.category = ProcessCategory::System;
    sys.cpu_delta_per_tick = 9_000_000;

    let state = MockState::new(vec![app, sys]);
    *state.mgmt.history_since.lock() = Some(1_700_000_000);
    state.mgmt.history_items.lock().push(AppHistoryRecord {
        app_id: Arc::from("browser.exe"),
        name: Arc::from("Browser"),
        cpu_time_100ns: 10_000_000,
        network_bytes: 500_000,
        metered_network_bytes: 1_000,
    });

    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };
    let coord = EngineCoordinator::new(backend, settings).unwrap();

    // Advance 2 ticks BEFORE leasing AppHistory -> live deltas accumulate in memory with 0 reads!
    for _ in 0..2 {
        coord
            .send_command_with_reply(EngineCommand::ForceRefresh)
            .unwrap()
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
    }
    assert_eq!(state.mgmt.read_history_calls.load(Ordering::Acquire), 0);

    // Lease AppHistory -> persisted baseline + 2 ticks of live deltas merge seamlessly.
    let _hist_lease = coord.acquire_lease(MonitorDomain::AppHistory);
    assert!(wait_until(Duration::from_secs(1), || {
        coord.snapshot().revisions.app_history >= 1
    }));

    let snap = coord.snapshot();
    assert_eq!(snap.app_history.since_epoch_secs, Some(1_700_000_000));
    assert_eq!(
        snap.app_history.items.len(),
        1,
        "System process must be excluded and browser.exe merged with persisted baseline"
    );
    let rec = &snap.app_history.items[0];
    assert_eq!(rec.app_id.as_ref(), "browser.exe");
    assert_eq!(rec.cpu_time_100ns, 10_000_000 + 2 * 1_500_000);
    assert_eq!(rec.network_bytes, 500_000 + 2 * 200_000);
    assert_eq!(rec.metered_network_bytes, 1_000);

    // ClearAppHistory wipes both persisted store and live in-memory accumulator atomically!
    let ev = coord
        .send_command_with_reply(EngineCommand::ClearAppHistory)
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(ev.kind, CommandKind::ClearAppHistory);
    assert!(ev.is_ok());

    let cleared = coord.snapshot();
    assert_eq!(cleared.app_history.since_epoch_secs, Some(1_750_000_000));
    assert!(
        cleared.app_history.items.is_empty(),
        "ClearAppHistory must reset both persisted and live accumulated entries"
    );

    // One more tick starts accumulating cleanly from zero.
    coord
        .send_command_with_reply(EngineCommand::ForceRefresh)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert!(wait_until(Duration::from_secs(1), || {
        coord
            .snapshot()
            .app_history
            .items
            .iter()
            .any(|r| r.app_id.as_ref() == "browser.exe" && r.cpu_time_100ns == 1_500_000)
    }));
}

#[test]
fn diagnostics_commands_support_wait_chain_minidump_cancel_uac_and_debugger_attach() {
    let target_key = ProcessKey::new(777, 10_000);
    let blocker_key = ProcessKey::new(888, 20_000);

    let state = MockState::new(vec![
        MockProcSpec::new(777, 10_000, 0, "legacy32.exe"),
        MockProcSpec::new(888, 20_000, 0, "blocker.exe"),
    ]);
    *state.wait_chain_nodes.lock() = vec![WaitChainNode {
        thread_id: 1001,
        process_key: Some(target_key),
        process_name: Some(Arc::from("legacy32.exe")),
        object_type: Arc::from("Thread"),
        object_name: None,
        is_deadlocked: true,
        children: vec![WaitChainNode {
            thread_id: 0,
            process_key: None,
            process_name: None,
            object_type: Arc::from("SendMessage"),
            object_name: None,
            is_deadlocked: true,
            children: vec![WaitChainNode {
                thread_id: 2002,
                process_key: Some(blocker_key),
                process_name: Some(Arc::from("blocker.exe")),
                object_type: Arc::from("Thread"),
                object_name: None,
                is_deadlocked: true,
                children: Vec::new(),
            }],
        }],
    }];

    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };
    let coord = EngineCoordinator::new(backend, settings).unwrap();
    let _details_lease = coord.acquire_lease(MonitorDomain::Details);
    assert!(wait_until(Duration::from_secs(1), || {
        coord.snapshot().revisions.details >= 1
    }));

    // 1. AnalyzeWaitChain returns structured WaitChain tree on slow lane.
    let wct_ev = coord
        .send_command_with_reply(EngineCommand::AnalyzeWaitChain(target_key))
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(wct_ev.kind, CommandKind::AnalyzeWaitChain);
    assert_eq!(wct_ev.target, Some(target_key));
    match wct_ev.outcome.as_ok() {
        Some(CommandSuccess::WaitChain(nodes)) => {
            assert_eq!(nodes.len(), 1);
            assert!(nodes[0].is_deadlocked);
            assert_eq!(nodes[0].thread_id, 1001);
            assert_eq!(
                nodes[0].children[0].children[0].process_key,
                Some(blocker_key)
            );
        }
        other => panic!("expected WaitChain success, got {other:?}"),
    }

    // 2. WriteMiniDump succeeds when not cancelled, and returns Cancelled when cancel is set.
    let dump_dest = PathBuf::from(r"C:\Temp\legacy32_777.DMP");
    let ok_ev = coord
        .send_command_with_reply(EngineCommand::WriteMiniDump {
            key: target_key,
            dest: dump_dest.clone(),
            cancel: Arc::new(AtomicBool::new(false)),
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(ok_ev.kind, CommandKind::WriteMiniDump);
    assert_eq!(ok_ev.target, Some(target_key));
    assert_eq!(
        ok_ev.outcome.as_ok(),
        Some(&CommandSuccess::MiniDumpWritten(dump_dest.clone()))
    );

    let cancel_ev = coord
        .send_command_with_reply(EngineCommand::WriteMiniDump {
            key: target_key,
            dest: dump_dest,
            cancel: Arc::new(AtomicBool::new(true)),
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(cancel_ev.kind, CommandKind::WriteMiniDump);
    assert!(matches!(cancel_ev.error(), Some(PalError::Cancelled)));

    // 3. SetUacVirtualization updates ColdMetaCache and reprojects Details, and propagates ElevationRequired.
    let uac_on = coord
        .send_command_with_reply(EngineCommand::SetUacVirtualization {
            key: target_key,
            enabled: true,
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(uac_on.kind, CommandKind::SetUacVirtualization);
    assert!(uac_on.is_ok());
    assert!(wait_until(Duration::from_secs(1), || {
        coord
            .snapshot()
            .details
            .rows
            .iter()
            .any(|r| r.key == target_key && r.uac_virtualization == UacVirtualization::Enabled)
    }));

    let uac_off = coord
        .send_command_with_reply(EngineCommand::SetUacVirtualization {
            key: target_key,
            enabled: false,
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert!(uac_off.is_ok());
    assert!(wait_until(Duration::from_secs(1), || {
        coord
            .snapshot()
            .details
            .rows
            .iter()
            .any(|r| r.key == target_key && r.uac_virtualization == UacVirtualization::Disabled)
    }));

    state.uac_error_elevation.store(true, Ordering::Release);
    let uac_denied = coord
        .send_command_with_reply(EngineCommand::SetUacVirtualization {
            key: target_key,
            enabled: true,
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert!(uac_denied.is_elevation_required());

    // 4. AttachDebugger succeeds on slow lane and propagates ElevationRequired when denied.
    let dbg_ok = coord
        .send_command_with_reply(EngineCommand::AttachDebugger(target_key))
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(dbg_ok.kind, CommandKind::AttachDebugger);
    assert_eq!(dbg_ok.target, Some(target_key));
    assert!(dbg_ok.is_ok());
    assert_eq!(
        state.debugger_attached_keys.lock().as_slice(),
        &[target_key]
    );

    state
        .debugger_error_elevation
        .store(true, Ordering::Release);
    let dbg_denied = coord
        .send_command_with_reply(EngineCommand::AttachDebugger(target_key))
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert!(dbg_denied.is_elevation_required());
}

#[test]
fn high_load_500_processes_hung_windows_and_slow_disk_stress_test() {
    use std::collections::HashSet;

    // Build 512 synthetic processes spanning:
    // - Index 0: slow-drive trigger binary on Z:\slow_app_0.exe
    // - 16 multi-level (depth 0 -> 1 -> 2) App trees (48 procs), with hung NOT_RESPONDING windows
    // - 443 mixed Background / System processes sharing local C:\ binaries
    // - Trailing 20 distinct executables on Z:\slow_app_1.exe .. Z:\slow_app_20.exe
    let mut procs = Vec::with_capacity(512);
    let mut hung_pids = HashSet::new();

    // Index 0: first slow-drive process (trips CircuitBreaker for Z: after >= 300ms).
    let mut slow_trigger = MockProcSpec::new(100, 1_000, 0, "slow_app_0.exe");
    slow_trigger.category = ProcessCategory::App;
    slow_trigger.window_title = "Slow External Drive App".to_owned();
    slow_trigger.exe_path = Some(r"Z:\slow_app_0.exe".to_owned());
    slow_trigger.private_mem_bytes = 256 * 1024 * 1024;
    slow_trigger.cpu_delta_per_tick = 500_000;
    procs.push(slow_trigger);

    // Multi-level 3-tier App trees (16 roots * 3 = 48 processes).
    let mut tracked_root_key = ProcessKey::default();
    let mut tracked_child_key = ProcessKey::default();
    let mut tracked_grandchild_key = ProcessKey::default();
    for i in 0..16u32 {
        let root_pid = 200 + i * 10;
        let child_pid = root_pid + 1;
        let grand_pid = root_pid + 2;
        let is_hung = i % 2 == 0;

        let mut root = MockProcSpec::new(
            root_pid,
            10_000 + u64::from(root_pid),
            0,
            &format!("studio_{i}.exe"),
        );
        root.category = ProcessCategory::App;
        root.window_title = if is_hung {
            format!("Hung Studio Window {i}")
        } else {
            format!("Studio Window {i}")
        };
        if is_hung {
            root.status = ProcessStatusFlags::NOT_RESPONDING;
            hung_pids.insert(root_pid);
        }
        root.exe_path = Some(format!(r"C:\Apps\studio_{}.exe", i % 4));
        root.private_mem_bytes = u64::from(128 + i * 16) * 1024 * 1024;
        root.cpu_delta_per_tick = u64::from(100_000 + i * 50_000);

        let mut child = MockProcSpec::new(
            child_pid,
            10_000 + u64::from(child_pid),
            root_pid,
            &format!("studio_{i}_worker.exe"),
        );
        child.category = ProcessCategory::Background;
        child.exe_path = Some(r"C:\Apps\worker.exe".to_owned());
        child.private_mem_bytes = u64::from(48 + i * 4) * 1024 * 1024;
        child.cpu_delta_per_tick = u64::from(40_000 + i * 10_000);

        let mut grand = MockProcSpec::new(
            grand_pid,
            10_000 + u64::from(grand_pid),
            child_pid,
            &format!("studio_{i}_gpu.exe"),
        );
        grand.category = ProcessCategory::Background;
        grand.exe_path = Some(r"C:\Apps\gpu_helper.exe".to_owned());
        grand.private_mem_bytes = u64::from(32 + i * 2) * 1024 * 1024;
        grand.cpu_delta_per_tick = u64::from(20_000 + i * 5_000);

        if i == 0 {
            tracked_root_key = root.key;
            tracked_child_key = child.key;
            tracked_grandchild_key = grand.key;
        }

        procs.push(root);
        procs.push(child);
        procs.push(grand);
    }

    // 443 mixed Background / System processes on fast local C:\ drive.
    for idx in 0..443u32 {
        let pid = 1_000 + idx * 4;
        let is_system = idx % 5 == 0;
        let parent_pid = if idx > 16 { 1_000 + (idx % 16) * 4 } else { 0 };
        let mut p = MockProcSpec::new(
            pid,
            50_000 + u64::from(idx),
            parent_pid,
            if is_system {
                "svchost.exe"
            } else {
                "bg_service.exe"
            },
        );
        p.category = if is_system {
            ProcessCategory::System
        } else {
            ProcessCategory::Background
        };
        p.exe_path = Some(if is_system {
            r"C:\Windows\System32\svchost.exe".to_owned()
        } else {
            format!(r"C:\ProgramData\Svc\bg_{}.exe", idx % 8)
        });
        p.private_mem_bytes = u64::from(8 + (idx % 64)) * 1024 * 1024;
        p.cpu_delta_per_tick = u64::from((idx % 20) * 10_000);
        procs.push(p);
    }

    // Trailing 20 distinct executables on Z:\ (indices 492..512, beyond initial 256 slow-queue cap).
    for idx in 1..=20u32 {
        let pid = 9_000 + idx * 4;
        let mut p = MockProcSpec::new(
            pid,
            90_000 + u64::from(idx),
            0,
            &format!("slow_app_{idx}.exe"),
        );
        p.category = ProcessCategory::Background;
        p.exe_path = Some(format!(r"Z:\slow_app_{idx}.exe"));
        p.private_mem_bytes = 16 * 1024 * 1024;
        procs.push(p);
    }
    assert_eq!(procs.len(), 512);

    let mut state = Arc::try_unwrap(MockState::new(procs)).ok().unwrap();
    state.logical_cpus = 16;
    state.mem_total_bytes = 32 * 1024 * 1024 * 1024;
    state.disk_probe_sleep = Duration::from_millis(500);
    state.exe_asset_slow_prefix = Some(r"Z:\".to_owned());
    state.exe_asset_sleep = Duration::from_millis(410);
    let state = Arc::new(state);

    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };
    let config = EngineConfig {
        slow_workers: 2,
        slow_queue_capacity: 256,
        slow_probe_timeout: Duration::from_millis(300),
        slow_circuit_cooldown: Duration::from_secs(60),
        slow_shutdown_timeout: Duration::from_millis(100),
        ..EngineConfig::default()
    };
    let coord = EngineCoordinator::with_config(backend, settings, config).unwrap();

    // Wait for Tick 0 and lease both Performance (slow disk polling) and Details.
    assert!(wait_until(Duration::from_secs(1), || {
        coord.snapshot().revisions.processes >= 1
    }));
    let _perf_lease = coord.acquire_lease(MonitorDomain::Performance);
    let _details_lease = coord.acquire_lease(MonitorDomain::Details);

    // Wait until BOTH slow-lane workers are actively executing slow-disk & slow-asset stalls.
    assert!(
        wait_until(Duration::from_secs(2), || {
            state.probe_disk_calls.load(Ordering::Acquire) >= 1
                && state.probe_asset_slow_calls.load(Ordering::Acquire) >= 1
        }),
        "both slow disk and slow Z: asset probes should start on slow-lane workers"
    );

    // While both slow-lane workers are stalled in 410ms/500ms sleeps, fire 5 consecutive
    // fast-lane ForceRefresh ticks across all 512 processes and verify fast-lane responsiveness.
    let burst_start = Instant::now();
    let rev_before_burst = coord.snapshot().revisions.processes;
    for _ in 0..5 {
        let ev = coord
            .send_command_with_reply(EngineCommand::ForceRefresh)
            .unwrap()
            .recv_timeout(Duration::from_millis(200))
            .expect("fast lane must not block behind 410ms/500ms slow-lane stalls");
        assert!(ev.is_ok());
    }
    let burst_elapsed = burst_start.elapsed();
    assert!(
        burst_elapsed < Duration::from_millis(300),
        "5 fast-lane 512-process ticks should complete well under 410ms slow stall, took {burst_elapsed:?}"
    );
    assert!(coord.snapshot().revisions.processes >= rev_before_burst + 5);

    // Wait for slow-lane workers to finish the 410ms Z: probe (tripping CircuitBreaker for Z:)
    // and drain remaining ProcessMeta probes on subsequent ticks.
    assert!(
        wait_until(Duration::from_secs(3), || {
            if state.probe_proc_calls.load(Ordering::Acquire) < 512 {
                let _ = coord.send_command(EngineCommand::ForceRefresh);
                false
            } else {
                true
            }
        }),
        "all 512 process cold metadata probes should complete; got {}",
        state.probe_proc_calls.load(Ordering::Acquire)
    );
    // Give fast lane one more ForceRefresh barrier to apply any final drained slow results.
    coord
        .send_command_with_reply(EngineCommand::ForceRefresh)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();

    // CircuitBreaker must have tripped on Z: after Z:\slow_app_0.exe (>= 300ms) and bypassed
    // all 20 trailing Z:\slow_app_1..20.exe asset probes!
    let slow_z_asset_calls = state.probe_asset_slow_calls.load(Ordering::Acquire);
    assert_eq!(
        slow_z_asset_calls, 1,
        "CircuitBreaker for Z: must bypass trailing 20 slow-drive asset probes, got {slow_z_asset_calls}"
    );

    // Verify snapshot integrity across all 512 processes & hung windows.
    let snap = coord.snapshot();
    assert_eq!(snap.processes.total_process_count, 512);
    assert_eq!(snap.details.rows.len(), 512);

    let projected_hung_details: HashSet<u32> = snap
        .details
        .rows
        .iter()
        .filter(|r| r.status.contains(ProcessStatusFlags::NOT_RESPONDING))
        .map(|r| r.key.pid)
        .collect();
    assert_eq!(
        projected_hung_details, hung_pids,
        "all NOT_RESPONDING hung windows must be accurately projected in Details"
    );
    assert!(
        snap.processes
            .rows
            .iter()
            .filter(|r| r.kind == ProcessRowKind::Process
                && r.status.contains(ProcessStatusFlags::NOT_RESPONDING))
            .count()
            >= hung_pids.len(),
        "hung NOT_RESPONDING apps must be surfaced in Processes slice"
    );

    // Verify Process Tree mode (zero duplicates, multi-level depth preserved).
    coord
        .send_command_with_reply(EngineCommand::SetProcessViewMode(ProcessViewMode::Tree))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    coord
        .send_command_with_reply(EngineCommand::ToggleProcessExpanded(tracked_root_key))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    coord
        .send_command_with_reply(EngineCommand::ToggleProcessExpanded(tracked_child_key))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();

    let tree_snap = coord.snapshot();
    let mut seen_keys = HashSet::with_capacity(tree_snap.processes.rows.len());
    for row in &tree_snap.processes.rows {
        assert_eq!(row.kind, ProcessRowKind::Process);
        assert!(seen_keys.insert(row.key), "duplicate key in Tree mode");
    }
    let grand_row = tree_snap
        .processes
        .rows
        .iter()
        .find(|r| r.key == tracked_grandchild_key)
        .expect("expanded depth-2 grandchild must be visible in Tree mode");
    assert_eq!(grand_row.depth, 2);

    // Verify search & sort accuracy on 512 processes.
    coord
        .send_command_with_reply(EngineCommand::SetProcessViewMode(ProcessViewMode::Grouped))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    coord
        .send_command_with_reply(EngineCommand::SetProcessSort {
            column: ProcessSortColumn::Memory,
            direction: SortDirection::Descending,
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let sorted_snap = coord.snapshot();
    let app_mems: Vec<u64> = sorted_snap
        .processes
        .rows
        .iter()
        .skip_while(|r| {
            !matches!(
                r.kind,
                ProcessRowKind::SectionHeader {
                    category: ProcessCategory::App,
                    ..
                }
            )
        })
        .skip(1)
        .take_while(|r| r.kind == ProcessRowKind::Process)
        .filter(|r| r.depth == 0)
        .map(|r| r.memory_bytes)
        .collect();
    assert!(app_mems.len() >= 16);
    assert!(
        app_mems.windows(2).all(|w| w[0] >= w[1]),
        "Apps section must be sorted by Memory descending"
    );

    // Search for a specific hung window title and verify accurate narrowing.
    coord
        .send_command_with_reply(EngineCommand::SetSearchQuery("Hung Studio Window 0".into()))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let search_snap = coord.snapshot();
    assert!(
        search_snap
            .processes
            .rows
            .iter()
            .any(|r| r.key == tracked_root_key
                && r.status.contains(ProcessStatusFlags::NOT_RESPONDING)),
        "search must find hung studio_0 root process"
    );
    assert!(search_snap.processes.rows.len() < 10);

    // Verify bottom-up kill ordering on the 3-level subtree (grandchild -> child -> root).
    let kill_ev = coord
        .send_command_with_reply(EngineCommand::TerminateProcessTree(tracked_root_key))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert!(kill_ev.is_ok());
    let recorded = state.recorded_tree_kills.lock();
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0].as_slice(),
        &[tracked_grandchild_key, tracked_child_key, tracked_root_key],
        "TerminateProcessTree must kill grandchild -> child -> root strictly bottom-up"
    );
    drop(recorded);

    coord.shutdown();
}

#[test]
fn steady_state_dormancy_and_unleased_domains_zero_overhead_test() {
    let mut app = MockProcSpec::new(301, 2_000, 0, "app.exe");
    app.category = ProcessCategory::App;
    app.cpu_delta_per_tick = 1_000_000;

    let state = MockState::new(vec![app]);
    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::High500Ms,
        ..AppSettings::default()
    };
    let config = EngineConfig {
        startup_calibration_delay: Duration::from_millis(30),
        ..EngineConfig::default()
    };
    let coord = EngineCoordinator::with_config(backend, settings, config).unwrap();

    // Wait for Tick 0 + calibration Tick 1.
    assert!(wait_until(Duration::from_secs(1), || {
        state.sample_proc_calls.load(Ordering::Acquire) >= 2
    }));

    // Unleased domains must incur zero backend sampling/polling calls.
    assert_eq!(state.sample_perf_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.probe_hw_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.probe_disk_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.mgmt.list_services_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.mgmt.list_startup_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.mgmt.list_sessions_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.mgmt.read_history_calls.load(Ordering::Acquire), 0);
    assert_eq!(coord.snapshot().revisions.details, 0);

    // Enter window dormancy (e.g., minimized / closed to tray) and wait past a full 500 ms tick.
    let dormant_ack = coord
        .send_command_with_reply(EngineCommand::SetWindowDormant(true))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert!(dormant_ack.is_ok());

    let calls_at_dormant = state.sample_proc_calls.load(Ordering::Acquire);
    let rev_at_dormant = coord.snapshot().revisions.processes;

    // Even acquiring a lease wakeup while dormant must NOT trigger sampling.
    let dormant_details_lease = coord.acquire_lease(MonitorDomain::Details);
    std::thread::sleep(Duration::from_millis(650));

    assert_eq!(
        state.sample_proc_calls.load(Ordering::Acquire),
        calls_at_dormant,
        "dormant window must perform strictly 0 fast-lane sample ticks"
    );
    assert_eq!(
        coord.snapshot().revisions.processes,
        rev_at_dormant,
        "dormant window must not publish process snapshot updates"
    );
    assert_eq!(
        coord.snapshot().revisions.details,
        0,
        "dormant window must not project Details even on lease wakeup"
    );
    drop(dormant_details_lease);

    // Wake up from dormancy -> immediate sample tick executes before ack returns!
    let wake_ack = coord
        .send_command_with_reply(EngineCommand::SetWindowDormant(false))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert!(wake_ack.is_ok());

    let calls_after_wake = state.sample_proc_calls.load(Ordering::Acquire);
    assert!(
        calls_after_wake > calls_at_dormant,
        "waking from dormancy must run an immediate tick ({calls_after_wake} vs {calls_at_dormant})"
    );
    assert!(coord.snapshot().revisions.processes > rev_at_dormant);

    // Periodic ticks resume automatically after waking from dormancy.
    assert!(
        wait_until(Duration::from_millis(900), || {
            state.sample_proc_calls.load(Ordering::Acquire) > calls_after_wake
        }),
        "periodic fast-lane ticks must resume after SetWindowDormant(false)"
    );

    // Unleased domains remain completely untouched throughout dormancy & wakeup.
    assert_eq!(state.sample_perf_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.mgmt.list_services_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.mgmt.list_startup_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.mgmt.list_sessions_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.mgmt.read_history_calls.load(Ordering::Acquire), 0);

    coord.shutdown();
}
