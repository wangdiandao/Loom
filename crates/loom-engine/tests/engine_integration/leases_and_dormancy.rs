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

use crate::mock_backend::{MockBackend, MockProcSpec, MockState, wait_until};

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

#[test]
fn snapshot_wake_signal_fires_on_publish_and_command_events_and_closes_on_shutdown() {
    let p1 = MockProcSpec::new(100, 10, 0, "worker.exe");
    let state = MockState::new(vec![p1]);
    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };
    let coord = EngineCoordinator::with_config(backend, settings, EngineConfig::default()).unwrap();

    let wake_rx = coord.subscribe_snapshot_wake();
    let cb_hits = Arc::new(AtomicUsize::new(0));
    let cb_counter = Arc::clone(&cb_hits);
    coord.set_snapshot_waker(Arc::new(move || {
        cb_counter.fetch_add(1, Ordering::AcqRel);
    }));
    // Drain any initial pending signal from Tick 0.
    while wake_rx.try_recv().is_ok() {}
    let hits_before = cb_hits.load(Ordering::Acquire);

    // ForceRefresh publishes a new snapshot -> must wake both channel & callback immediately.
    coord
        .send_command_with_reply(EngineCommand::ForceRefresh)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert!(
        wake_rx.recv_timeout(Duration::from_secs(1)).is_ok(),
        "snapshot publication must notify subscribe_snapshot_wake receiver"
    );
    assert!(cb_hits.load(Ordering::Acquire) > hits_before);

    // CommandEvent emission (even without snapshot publish) must also trigger wake signal.
    while wake_rx.try_recv().is_ok() {}
    let hits_before_ev = cb_hits.load(Ordering::Acquire);
    let ev = coord
        .send_command_with_reply(EngineCommand::TerminateProcess(ProcessKey::new(100, 10)))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert!(ev.is_ok());
    assert!(
        wake_rx.recv_timeout(Duration::from_secs(1)).is_ok(),
        "CommandEvent emission must trigger snapshot wake signal"
    );
    assert!(cb_hits.load(Ordering::Acquire) > hits_before_ev);

    // Shutdown closes the wake channel cleanly.
    coord.shutdown();
    while wake_rx.try_recv().is_ok() {}
    assert!(wake_rx.recv_timeout(Duration::from_secs(1)).is_err());
}

#[test]
fn order_freeze_temp_freeze_and_force_refresh_work_without_clobbering_refresh_speed() {
    let mut p1 = MockProcSpec::new(101, 10, 0, "alpha.exe");
    p1.private_mem_bytes = 500 * 1024 * 1024;
    let mut p2 = MockProcSpec::new(102, 10, 0, "beta.exe");
    p2.private_mem_bytes = 100 * 1024 * 1024;
    let k1 = p1.key;
    let k2 = p2.key;

    let state = MockState::new(vec![p1, p2]);
    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };
    let coord = EngineCoordinator::with_config(backend, settings, EngineConfig::default()).unwrap();
    let _details_lease = coord.acquire_lease(MonitorDomain::Details);

    coord
        .send_command_with_reply(EngineCommand::SetProcessSort {
            column: ProcessSortColumn::Memory,
            direction: SortDirection::Descending,
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    coord
        .send_command_with_reply(EngineCommand::SetDetailsSort {
            column: DetailsSortColumn::Memory,
            direction: SortDirection::Descending,
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();

    // Freeze both Processes & Details row order while alpha > beta.
    coord
        .send_command_with_reply(EngineCommand::SetProcessOrderFreeze(true))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    coord
        .send_command_with_reply(EngineCommand::SetDetailsOrderFreeze(true))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();

    // Flip live memory so beta (900 MiB) > alpha (50 MiB) and ForceRefresh while Paused.
    {
        let mut ov = state.dynamic_mem_override.lock();
        ov.insert(k1, 50 * 1024 * 1024);
        ov.insert(k2, 900 * 1024 * 1024);
    }
    coord
        .send_command_with_reply(EngineCommand::ForceRefresh)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();

    let snap = coord.snapshot();
    let proc_rows: Vec<(ProcessKey, u64)> = snap
        .processes
        .rows
        .iter()
        .filter(|r| r.kind == ProcessRowKind::Process)
        .map(|r| (r.key, r.memory_bytes))
        .collect();
    assert_eq!(
        proc_rows,
        vec![(k1, 50 * 1024 * 1024), (k2, 900 * 1024 * 1024)],
        "frozen Processes order must stay k1 -> k2 while live metrics update"
    );
    let det_rows: Vec<(ProcessKey, u64)> = snap
        .details
        .rows
        .iter()
        .map(|r| (r.key, r.private_working_set_bytes))
        .collect();
    assert_eq!(
        det_rows,
        vec![(k1, 50 * 1024 * 1024), (k2, 900 * 1024 * 1024)],
        "frozen Details order must stay k1 -> k2 while live metrics update"
    );

    // Unfreezing immediately re-sorts k2 ahead of k1.
    coord
        .send_command_with_reply(EngineCommand::SetProcessOrderFreeze(false))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    coord
        .send_command_with_reply(EngineCommand::SetDetailsOrderFreeze(false))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let unfrozen = coord.snapshot();
    let proc_keys: Vec<ProcessKey> = unfrozen
        .processes
        .rows
        .iter()
        .filter(|r| r.kind == ProcessRowKind::Process)
        .map(|r| r.key)
        .collect();
    assert_eq!(proc_keys, vec![k2, k1]);
    assert_eq!(
        unfrozen
            .details
            .rows
            .iter()
            .map(|r| r.key)
            .collect::<Vec<_>>(),
        vec![k2, k1]
    );

    // SetTempFreeze(true) -> SetTempFreeze(false) while RefreshSpeed::Paused must STAY paused!
    let calls_before = state.sample_proc_calls.load(Ordering::Acquire);
    coord
        .send_command_with_reply(EngineCommand::SetTempFreeze(true))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    coord
        .send_command_with_reply(EngineCommand::SetTempFreeze(false))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    std::thread::sleep(Duration::from_millis(120));
    assert_eq!(
        state.sample_proc_calls.load(Ordering::Acquire),
        calls_before,
        "releasing SetTempFreeze must not resume sampling when RefreshSpeed::Paused"
    );

    coord.shutdown();
}

#[test]
fn processes_domain_lease_gates_projection_while_details_stays_independent() {
    let p1 = MockProcSpec::new(100, 10, 0, "app.exe");
    let p2 = MockProcSpec::new(200, 20, 0, "worker.exe");
    let state = MockState::new(vec![p1, p2]);
    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };
    let config = EngineConfig {
        gate_processes_by_lease: true,
        project_processes_on_boot: false,
        cold_meta_coalesce_window: Duration::from_millis(25),
        ..EngineConfig::default()
    };
    let coord = EngineCoordinator::with_config(backend, settings, config).unwrap();

    // Without a Processes lease and with project_processes_on_boot = false,
    // Processes projection stays skipped even across explicit ForceRefresh ticks.
    coord
        .send_command_with_reply(EngineCommand::ForceRefresh)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert_eq!(coord.snapshot().revisions.processes, 0);
    assert!(coord.snapshot().processes.rows.is_empty());

    // Details lease updates Details independently without waking Processes projection.
    let _details_lease = coord.acquire_lease(MonitorDomain::Details);
    assert!(wait_until(Duration::from_secs(2), || {
        coord.snapshot().revisions.details >= 1 && coord.snapshot().details.rows.len() == 2
    }));
    assert_eq!(
        coord.snapshot().revisions.processes,
        0,
        "unleased Processes must stay skipped while Details is active"
    );

    // Acquiring MonitorDomain::Processes immediately triggers lease-wakeup projection.
    let proc_lease = coord.acquire_lease(MonitorDomain::Processes);
    assert!(wait_until(Duration::from_secs(2), || {
        coord.snapshot().revisions.processes >= 1 && !coord.snapshot().processes.rows.is_empty()
    }));
    let rev_while_leased = coord.snapshot().revisions.processes;

    // Dropping the Processes lease halts Processes re-projection on subsequent ticks.
    drop(proc_lease);
    // Give the lease-drop wakeup a brief moment to settle, then force a fresh tick.
    std::thread::sleep(Duration::from_millis(30));
    coord
        .send_command_with_reply(EngineCommand::ForceRefresh)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert_eq!(
        coord.snapshot().revisions.processes,
        rev_while_leased,
        "releasing MonitorDomain::Processes must skip subsequent Processes projections"
    );
    assert!(
        coord.snapshot().revisions.details >= 2,
        "Details must continue refreshing while Processes is unleased"
    );

    // Re-acquiring MonitorDomain::Processes immediately catches up with the latest tick.
    let _proc_lease_again = coord.acquire_lease(MonitorDomain::Processes);
    assert!(wait_until(Duration::from_secs(2), || {
        coord.snapshot().revisions.processes > rev_while_leased
    }));

    coord.shutdown();
}

#[test]
fn unchanged_mgmt_polls_and_noop_freeze_do_not_bump_revisions() {
    let p1 = MockProcSpec::new(100, 10, 0, "app.exe");
    let state = MockState::new(vec![p1]);
    state.mgmt.services.lock().push(ServiceRecord {
        name: Arc::from("Spooler"),
        display_name: Arc::from("Print Spooler"),
        description: Some(Arc::from("Print spooler service")),
        pid: Some(100),
        state: ServiceState::Running,
        group: Some(Arc::from("netsvcs")),
    });
    state.mgmt.startup_entries.lock().push(StartupEntry {
        id: Arc::from("hkcu:Loom"),
        name: Arc::from("Loom"),
        publisher: Some(Arc::from("Loom")),
        command: None,
        file_path: None,
        icon: None,
        enabled: true,
        impact: StartupImpact::Low,
    });
    *state.mgmt.history_since.lock() = Some(1_700_000_000);
    state.mgmt.history_items.lock().push(AppHistoryRecord {
        app_id: Arc::from("app.one"),
        name: Arc::from("App One"),
        cpu_time_100ns: 100_000,
        network_bytes: 0,
        metered_network_bytes: 0,
    });

    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };
    let coord = EngineCoordinator::new(backend, settings).unwrap();
    let _svc_lease = coord.acquire_lease(MonitorDomain::Services);
    let _st_lease = coord.acquire_lease(MonitorDomain::Startup);
    let _hist_lease = coord.acquire_lease(MonitorDomain::AppHistory);
    let _det_lease = coord.acquire_lease(MonitorDomain::Details);

    assert!(wait_until(Duration::from_secs(2), || {
        let r = coord.snapshot().revisions;
        r.services >= 1 && r.startup >= 1 && r.app_history >= 1 && r.details >= 1
    }));

    let rev_initial = coord.snapshot().revisions;
    let svc_polls_before = state.mgmt.list_services_calls.load(Ordering::Acquire);
    let st_polls_before = state.mgmt.list_startup_calls.load(Ordering::Acquire);
    let hist_polls_before = state.mgmt.read_history_calls.load(Ordering::Acquire);

    // ForceRefresh polls Services/Startup/AppHistory again with identical data & zero deltas.
    coord
        .send_command_with_reply(EngineCommand::ForceRefresh)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert!(wait_until(Duration::from_secs(2), || {
        state.mgmt.list_services_calls.load(Ordering::Acquire) > svc_polls_before
            && state.mgmt.list_startup_calls.load(Ordering::Acquire) > st_polls_before
            && state.mgmt.read_history_calls.load(Ordering::Acquire) > hist_polls_before
    }));
    std::thread::sleep(Duration::from_millis(30));

    let rev_after_same_poll = coord.snapshot().revisions;
    assert_eq!(
        rev_after_same_poll.services, rev_initial.services,
        "unchanged Services poll must not spuriously bump services revision"
    );
    assert_eq!(
        rev_after_same_poll.startup, rev_initial.startup,
        "unchanged Startup poll must not spuriously bump startup revision"
    );
    assert_eq!(
        rev_after_same_poll.app_history, rev_initial.app_history,
        "unchanged AppHistory poll with zero live delta must not spuriously bump revision"
    );

    // No-op SetProcessOrderFreeze(false) / SetDetailsOrderFreeze(false) when already unfrozen
    // must not spuriously bump process/details revisions.
    let rev_before_noop = coord.snapshot().revisions;
    coord
        .send_command_with_reply(EngineCommand::SetProcessOrderFreeze(false))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    coord
        .send_command_with_reply(EngineCommand::SetDetailsOrderFreeze(false))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let rev_after_noop = coord.snapshot().revisions;
    assert_eq!(rev_after_noop.processes, rev_before_noop.processes);
    assert_eq!(rev_after_noop.details, rev_before_noop.details);

    coord.shutdown();
}
