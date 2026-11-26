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
    AppHistoryRecord, CapabilityFlags, CpuAffinityMask, DetailsViewMode, ExeIconRgba,
    ExeStaticAsset, MonitorDomain, PlatformProfile, ProcessArch, ProcessCategory, ProcessColdMeta,
    ProcessHotSample, ProcessKey, ProcessPriority, ProcessRowKind, ProcessSortColumn,
    ProcessStatusFlags, ProcessViewMode, RefreshSpeed, RunTaskRequest, ServiceRecord,
    SortDirection, StartupEntry, SystemPerfSample, Timestamp100Ns, TreeKillItem, TreeKillReport,
    UacVirtualization, UserSessionRecord, WaitChainNode,
};
use parking_lot::Mutex;

#[derive(Debug, Clone)]
struct MockProcSpec {
    key: ProcessKey,
    parent_pid: u32,
    name: String,
    window_title: String,
    category: ProcessCategory,
    cpu_time_100ns: u64,
    cpu_delta_per_tick: u64,
    disk_bytes: u64,
    disk_delta_per_tick: u64,
    private_mem_bytes: u64,
    exe_path: Option<String>,
}

impl MockProcSpec {
    fn new(pid: u32, create_time: u64, parent_pid: u32, name: &str) -> Self {
        Self {
            key: ProcessKey::new(pid, create_time),
            parent_pid,
            name: name.to_owned(),
            window_title: String::new(),
            category: ProcessCategory::Background,
            cpu_time_100ns: 1_000_000,
            cpu_delta_per_tick: 0,
            disk_bytes: 0,
            disk_delta_per_tick: 0,
            private_mem_bytes: 32 * 1024 * 1024,
            exe_path: None,
        }
    }
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
    slow_probe_block_pid: Option<u32>,
    slow_probe_sleep: Duration,
    slow_probe_release: Option<Receiver<()>>,
    recorded_tree_kills: Mutex<Vec<Vec<ProcessKey>>>,
    priorities: Mutex<HashMap<ProcessKey, ProcessPriority>>,
    browse_result: Mutex<Option<PathBuf>>,
    terminate_error_elevation: AtomicBool,
    is_elevated: AtomicBool,
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
            slow_probe_block_pid: None,
            slow_probe_sleep: Duration::ZERO,
            slow_probe_release: None,
            recorded_tree_kills: Mutex::new(Vec::new()),
            priorities: Mutex::new(HashMap::new()),
            browse_result: Mutex::new(None),
            terminate_error_elevation: AtomicBool::new(false),
            is_elevated: AtomicBool::new(false),
        })
    }
}

struct MockBackend {
    state: Arc<MockState>,
    services: MockNoopServices,
    startup: MockNoopStartup,
    sessions: MockNoopSessions,
    history: MockNoopHistory,
    operator: MockOperator,
}

impl MockBackend {
    fn from_state(state: Arc<MockState>) -> Self {
        let operator = MockOperator {
            state: Arc::clone(&state),
        };
        Self {
            state,
            services: MockNoopServices,
            startup: MockNoopStartup,
            sessions: MockNoopSessions,
            history: MockNoopHistory,
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
                session_id: 1,
                thread_count: 4,
                handle_count: 64,
                kernel_time_100ns: cpu / 2,
                user_time_100ns: cpu - cpu / 2,
                private_working_set_bytes: spec.private_mem_bytes,
                working_set_bytes: spec.private_mem_bytes,
                commit_bytes: spec.private_mem_bytes,
                disk_read_bytes: disk,
                category: spec.category,
                priority,
                status: ProcessStatusFlags::empty(),
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
            logical_cpu_count: self.state.logical_cpus,
            mem_total_bytes: self.state.mem_total_bytes,
            mem_used_bytes: self.state.mem_total_bytes / 2,
            ..SystemPerfSample::default()
        };
        scratch.per_core_permille.clear();
        scratch
            .per_core_permille
            .extend((0..self.state.logical_cpus).map(|i| 100 + (i as u16) * 50));
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
}

struct MockNoopServices;
impl ServiceManager for MockNoopServices {
    fn list_services(&self) -> Result<Vec<ServiceRecord>, PalError> {
        Ok(Vec::new())
    }
    fn start_service(&self, _name: &str) -> Result<(), PalError> {
        Ok(())
    }
    fn stop_service(&self, _name: &str) -> Result<(), PalError> {
        Ok(())
    }
    fn restart_service(&self, _name: &str) -> Result<(), PalError> {
        Ok(())
    }
    fn open_services_console(&self) -> Result<(), PalError> {
        Ok(())
    }
}

struct MockNoopStartup;
impl StartupManager for MockNoopStartup {
    fn list_startup_entries(&self) -> Result<(Option<u64>, Vec<StartupEntry>), PalError> {
        Ok((Some(2_100), Vec::new()))
    }
    fn set_startup_enabled(&self, _id: &str, _enabled: bool) -> Result<(), PalError> {
        Ok(())
    }
}

struct MockNoopSessions;
impl SessionManager for MockNoopSessions {
    fn list_sessions(&self) -> Result<Vec<UserSessionRecord>, PalError> {
        Ok(Vec::new())
    }
    fn disconnect_session(&self, _session_id: u32) -> Result<(), PalError> {
        Ok(())
    }
}

struct MockNoopHistory;
impl AppHistoryReader for MockNoopHistory {
    fn read_history(&self) -> Result<(Option<u64>, Vec<AppHistoryRecord>), PalError> {
        Ok((None, Vec::new()))
    }
    fn clear_history(&self) -> Result<(), PalError> {
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
        Ok(())
    }
    fn analyze_wait_chain(&self, _key: ProcessKey) -> Result<Vec<WaitChainNode>, PalError> {
        Ok(Vec::new())
    }
    fn write_minidump(
        &self,
        _key: ProcessKey,
        _dest: &Path,
        _cancel: &AtomicBool,
    ) -> Result<(), PalError> {
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
    type ServiceMgr = MockNoopServices;
    type StartupMgr = MockNoopStartup;
    type SessionMgr = MockNoopSessions;
    type HistoryReader = MockNoopHistory;
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
        slow_probe_block_pid: Some(500),
        slow_probe_sleep: Duration::from_secs(5),
        slow_probe_release: Some(release_rx),
        recorded_tree_kills: Mutex::new(Vec::new()),
        priorities: Mutex::new(HashMap::new()),
        browse_result: Mutex::new(None),
        terminate_error_elevation: AtomicBool::new(false),
        is_elevated: AtomicBool::new(false),
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

    // Acquiring Performance + PerCoreCpu lease triggers sample_performance and populates per-core history.
    let _perf_lease = coord.acquire_lease(MonitorDomain::Performance);
    let _core_lease = coord.acquire_lease(MonitorDomain::PerCoreCpu);
    assert!(wait_until(Duration::from_millis(300), || {
        state.sample_perf_calls.load(Ordering::Acquire) >= 1
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
        type ServiceMgr = MockNoopServices;
        type StartupMgr = MockNoopStartup;
        type SessionMgr = MockNoopSessions;
        type HistoryReader = MockNoopHistory;
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
