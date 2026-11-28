#![allow(unused_imports)]
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

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

use crate::mock_backend::{
    MockBackend, MockFastSampler, MockHistory, MockOperator, MockProcSpec, MockServices,
    MockSessions, MockSlowProber, MockStartup, MockState, wait_until,
};

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
        slow_probe_block_pid: Some(500),
        slow_probe_sleep: Duration::from_secs(5),
        slow_probe_release: Some(release_rx),
        ..MockState::new_raw(vec![slow_proc, fast_proc])
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
fn slow_disk_probe_400ms_never_blocks_fast_lane_and_deduplicates_polls() {
    let state = Arc::new(MockState {
        disk_probe_sleep: Duration::from_millis(400),
        ..MockState::new_raw(vec![MockProcSpec::new(20, 100, 0, "worker.exe")])
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
fn startup_tick0_defers_per_process_gpu_and_prioritizes_app_slow_probes() {
    let mut sys_proc = MockProcSpec::new(100, 1, 0, "svchost.exe");
    sys_proc.category = ProcessCategory::System;
    sys_proc.private_mem_bytes = 200 * 1024 * 1024;

    let mut bg_proc = MockProcSpec::new(200, 2, 0, "worker.exe");
    bg_proc.category = ProcessCategory::Background;
    bg_proc.private_mem_bytes = 150 * 1024 * 1024;

    let mut small_app = MockProcSpec::new(300, 3, 0, "notepad.exe");
    small_app.category = ProcessCategory::App;
    small_app.private_mem_bytes = 24 * 1024 * 1024;

    let mut big_app = MockProcSpec::new(400, 4, 0, "chrome.exe");
    big_app.category = ProcessCategory::App;
    big_app.private_mem_bytes = 512 * 1024 * 1024;

    let state = MockState::new(vec![
        sys_proc.clone(),
        bg_proc.clone(),
        small_app.clone(),
        big_app.clone(),
    ]);
    let backend = MockBackend::from_state(Arc::clone(&state));
    let config = EngineConfig {
        slow_workers: 1,
        startup_calibration_delay: Duration::from_millis(20),
        ..EngineConfig::default()
    };
    let coord = EngineCoordinator::with_config(backend, AppSettings::default(), config).unwrap();

    // Wait for all 4 processes to be probed in order on the single slow worker.
    assert!(wait_until(Duration::from_secs(2), || {
        state.probed_proc_order.lock().len() >= 4
            && state.per_proc_gpu_transitions.lock().len() >= 2
    }));

    let gpu_transitions = state.per_proc_gpu_transitions.lock().clone();
    assert_eq!(
        &gpu_transitions[..2],
        &[false, true],
        "Tick 0 must disable per-process GPU queries and re-enable immediately after Tick 0"
    );

    let probed = state.probed_proc_order.lock().clone();
    assert_eq!(
        probed[0], big_app.key,
        "heaviest App process must be probed first"
    );
    assert_eq!(
        probed[1], small_app.key,
        "remaining App process must be probed before non-App processes"
    );
    assert_eq!(
        probed[2], sys_proc.key,
        "non-App processes must preserve their deterministic relative order"
    );
    assert_eq!(
        probed[3], bg_proc.key,
        "non-App processes must preserve their deterministic relative order"
    );

    coord.shutdown();
}
