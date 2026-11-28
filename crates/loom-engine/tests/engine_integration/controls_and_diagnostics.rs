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
fn structured_port_search_is_strictly_on_demand_and_hide_system_filter_works() {
    let mut node_proc = MockProcSpec::new(3000, 10, 0, "node.exe");
    node_proc.category = ProcessCategory::Background;
    let mut sys_proc = MockProcSpec::new(400, 10, 0, "svchost.exe");
    sys_proc.category = ProcessCategory::System;
    let mut shell_proc = MockProcSpec::new(401, 10, 0, "pwsh.exe");
    shell_proc.category = ProcessCategory::System;

    let state = MockState::new(vec![node_proc, sys_proc, shell_proc]);
    state.listening_ports.lock().push(ProcessSocketRecord {
        protocol: SocketProtocol::Tcp,
        local_addr: Arc::from("0.0.0.0"),
        local_port: 3000,
        remote_addr: None,
        remote_port: None,
        state: Arc::from("LISTEN"),
        pid: 3000,
    });

    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };
    let coord = EngineCoordinator::with_config(backend, settings, EngineConfig::default()).unwrap();

    // Normal search query MUST NOT trigger any listening port scans.
    coord
        .send_command_with_reply(EngineCommand::SetSearchQuery("node".into()))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    coord
        .send_command_with_reply(EngineCommand::ForceRefresh)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert_eq!(
        state.list_ports_calls.load(Ordering::Acquire),
        0,
        "non-port search must incur strictly 0 port scans"
    );

    // Switching to `:3000` dispatches slow-lane `list_listening_ports()` and matches PID 3000.
    coord
        .send_command_with_reply(EngineCommand::SetSearchQuery(":3000".into()))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(2), || {
            let snap = coord.snapshot();
            snap.processes
                .rows
                .iter()
                .any(|r| r.kind == ProcessRowKind::Process && r.key.pid == 3000)
        }),
        "on-demand `:3000` query must resolve and display owning process PID 3000"
    );
    assert_eq!(state.list_ports_calls.load(Ordering::Acquire), 1);

    // Clearing the port query stops future port scans and tests `SetHideSystemProcesses(true)`.
    coord
        .send_command_with_reply(EngineCommand::SetSearchQuery(String::new()))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    coord
        .send_command_with_reply(EngineCommand::SetHideSystemProcesses(true))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let snap = coord.snapshot();
    assert!(
        !snap
            .processes
            .rows
            .iter()
            .any(|r| r.name.as_ref() == "svchost.exe"),
        "svchost.exe must be hidden when hide_windows_system_processes is enabled"
    );
    assert!(
        snap.processes
            .rows
            .iter()
            .any(|r| r.name.as_ref() == "pwsh.exe"),
        "interactive shell pwsh.exe must remain visible"
    );

    coord.shutdown();
}

#[test]
fn performance_gap_sentinels_d3_asleep_and_core_kinds_propagate_accurately() {
    let p1 = MockProcSpec::new(100, 10, 0, "app.exe");
    let state = MockState::new(vec![p1]);
    state.gpu_d3_asleep.store(true, Ordering::Release);
    *state.core_kinds.lock() = vec![
        CpuCoreKind::Performance,
        CpuCoreKind::Performance,
        CpuCoreKind::Efficiency,
        CpuCoreKind::LowPowerEfficiency,
    ];

    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };
    let coord = EngineCoordinator::with_config(backend, settings, EngineConfig::default()).unwrap();
    let _perf_lease = coord.acquire_lease(MonitorDomain::Performance);

    // Wait for hardware static info and initial perf tick.
    assert!(wait_until(Duration::from_secs(2), || {
        let s = coord.snapshot();
        !s.performance.cpu_info.core_kinds.is_empty() && !s.performance.gpus.is_empty()
    }));

    let s0 = coord.snapshot();
    assert_eq!(
        s0.performance.cpu_info.core_kinds,
        vec![
            CpuCoreKind::Performance,
            CpuCoreKind::Performance,
            CpuCoreKind::Efficiency,
            CpuCoreKind::LowPowerEfficiency,
        ]
    );
    assert!(s0.performance.gpus[0].d3_asleep);
    assert!(
        !s0.performance
            .cpu_history
            .iter_ordered()
            .any(HistoryRing::<60>::is_gap),
        "initial samples must not contain spurious gap sentinels"
    );

    // 1. Resuming from RefreshSpeed::Paused inserts a gap sentinel on next tick.
    coord
        .send_command_with_reply(EngineCommand::SetRefreshSpeed(RefreshSpeed::Low2S))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    coord
        .send_command_with_reply(EngineCommand::ForceRefresh)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let gaps_after_resume = coord
        .snapshot()
        .performance
        .cpu_history
        .iter_ordered()
        .filter(|&v| HistoryRing::<60>::is_gap(v))
        .count();
    assert_eq!(
        gaps_after_resume, 1,
        "resuming from Paused must insert exactly one gap sentinel"
    );

    // 2. Normal refresh speed change (Low2S -> High500Ms) MUST NOT insert a false gap.
    coord
        .send_command_with_reply(EngineCommand::SetRefreshSpeed(RefreshSpeed::High500Ms))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    coord
        .send_command_with_reply(EngineCommand::ForceRefresh)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let gaps_after_speed_change = coord
        .snapshot()
        .performance
        .cpu_history
        .iter_ordered()
        .filter(|&v| HistoryRing::<60>::is_gap(v))
        .count();
    assert_eq!(
        gaps_after_speed_change, 1,
        "changing refresh speed between non-paused cadences must not insert a gap"
    );

    // 3. Sampling error inserts a gap sentinel.
    state.fail_next_perf_sample.store(true, Ordering::Release);
    coord
        .send_command_with_reply(EngineCommand::ForceRefresh)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let gaps_after_error = coord
        .snapshot()
        .performance
        .cpu_history
        .iter_ordered()
        .filter(|&v| HistoryRing::<60>::is_gap(v))
        .count();
    assert_eq!(
        gaps_after_error, 2,
        "performance sampling error must insert a gap sentinel"
    );

    // Recover with one normal tick before simulating a clock jump so gaps are separated by real data.
    coord
        .send_command_with_reply(EngineCommand::ForceRefresh)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();

    // 4. Timestamp jump > 5s (simulating system sleep / clock jump) inserts a gap sentinel.
    *state.extra_ts_jump_100ns.lock() = 100_000_000; // +10 seconds
    coord
        .send_command_with_reply(EngineCommand::ForceRefresh)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let gaps_after_jump = coord
        .snapshot()
        .performance
        .cpu_history
        .iter_ordered()
        .filter(|&v| HistoryRing::<60>::is_gap(v))
        .count();
    assert_eq!(
        gaps_after_jump, 3,
        "elapsed timestamp jump > 5s must insert a gap sentinel"
    );

    coord.shutdown();
}

#[test]
fn new_process_controls_file_locks_etw_lease_and_app_history_save_work_end_to_end() {
    let mut app = MockProcSpec::new(500, 10, 0, "app.exe");
    app.category = ProcessCategory::App;
    app.cpu_delta_per_tick = 2_000_000;
    app.handle_count = 100_500; // triggers immediate critical handle leak warning
    app.exe_path = Some(r"C:\Apps\app.exe".to_owned());

    let mut wv = MockProcSpec::new(501, 20, 500, "msedgewebview2.exe");
    wv.command_line =
        Some(r#"msedgewebview2.exe --type=gpu-process --webview-exe-name=app.exe"#.to_owned());

    let app_key = app.key;
    let wv_key = wv.key;
    let state = MockState::new(vec![app, wv]);
    state.file_locks_result.lock().push(FileLockHolder {
        pid: 500,
        app_name: Arc::from("Mock Editor"),
        service_name: None,
        app_type: Arc::from("MainWindow"),
        restartable: true,
    });

    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };
    let config = EngineConfig {
        app_history_flush_interval: Duration::from_millis(10),
        leak_detector: LeakDetectorConfig {
            check_interval: Duration::ZERO,
            sustain_window: Duration::from_secs(600),
        },
        ..EngineConfig::default()
    };
    let coord = EngineCoordinator::with_config(backend, settings, config).unwrap();

    // 1. ProcessNetwork lease gating calls set_per_process_network_enabled(true -> false -> true -> false).
    assert!(state.per_proc_net_transitions.lock().is_empty());
    let net_lease = coord.acquire_lease(MonitorDomain::ProcessNetwork);
    assert!(wait_until(Duration::from_secs(1), || {
        state.per_proc_net_transitions.lock().as_slice() == [true]
    }));
    // Dormancy must turn ETW off even while lease is held, then restore on wake.
    coord
        .send_command_with_reply(EngineCommand::SetWindowDormant(true))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert_eq!(
        state.per_proc_net_transitions.lock().as_slice(),
        &[true, false]
    );
    coord
        .send_command_with_reply(EngineCommand::SetWindowDormant(false))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert_eq!(
        state.per_proc_net_transitions.lock().as_slice(),
        &[true, false, true]
    );
    drop(net_lease);
    assert!(wait_until(Duration::from_secs(1), || {
        state.per_proc_net_transitions.lock().as_slice() == [true, false, true, false]
    }));

    // 2. SuspendProcess (with ElevationRequired propagation), ResumeProcess, RestartExplorer, FindFileLocks.
    state.suspend_error_elevation.store(true, Ordering::Release);
    let susp_denied = coord
        .send_command_with_reply(EngineCommand::SuspendProcess(app_key))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert_eq!(susp_denied.kind, CommandKind::SuspendProcess);
    assert!(susp_denied.is_elevation_required());

    state
        .suspend_error_elevation
        .store(false, Ordering::Release);
    let susp_ok = coord
        .send_command_with_reply(EngineCommand::SuspendProcess(app_key))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert!(susp_ok.is_ok());
    assert_eq!(state.suspended_keys.lock().as_slice(), &[app_key]);

    let res_ok = coord
        .send_command_with_reply(EngineCommand::ResumeProcess(app_key))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert!(res_ok.is_ok());
    assert_eq!(state.resumed_keys.lock().as_slice(), &[app_key]);

    let restart_ev = coord
        .send_command_with_reply(EngineCommand::RestartExplorer)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert_eq!(restart_ev.kind, CommandKind::RestartExplorer);
    assert!(restart_ev.is_ok());
    assert_eq!(state.restart_explorer_calls.load(Ordering::Acquire), 1);

    let locks_ev = coord
        .send_command_with_reply(EngineCommand::FindFileLocks {
            path: PathBuf::from(r"C:\Temp\locked.txt"),
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert_eq!(locks_ev.kind, CommandKind::FindFileLocks);
    let Some(CommandSuccess::FileLocks(holders)) = locks_ev.outcome.as_ok() else {
        panic!(
            "expected CommandSuccess::FileLocks, got {:?}",
            locks_ev.outcome
        );
    };
    assert_eq!(holders.len(), 1);
    assert_eq!(holders[0].pid, 500);
    assert_eq!(holders[0].app_name.as_ref(), "Mock Editor");

    // 3. Leak warnings, IgnoreLeakPath, SetShowLeakWarnings, parent_key, and WebView2 role.
    assert!(wait_until(Duration::from_secs(2), || {
        let snap = coord.snapshot();
        let has_leak = snap.processes.rows.iter().any(|r| {
            r.key == app_key
                && r.leak_warning
                    .as_ref()
                    .is_some_and(|w| w.kind == LeakKind::Handles)
        });
        let has_wv = snap.processes.rows.iter().any(|r| {
            r.key == wv_key
                && r.webview2_role == Some(WebView2Role::Gpu)
                && r.parent_key == Some(app_key)
        });
        has_leak && has_wv
    }));

    coord
        .send_command_with_reply(EngineCommand::SetShowLeakWarnings(false))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert!(
        coord
            .snapshot()
            .processes
            .rows
            .iter()
            .find(|r| r.key == app_key)
            .unwrap()
            .leak_warning
            .is_none()
    );
    coord
        .send_command_with_reply(EngineCommand::SetShowLeakWarnings(true))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert!(
        coord
            .snapshot()
            .processes
            .rows
            .iter()
            .find(|r| r.key == app_key)
            .unwrap()
            .leak_warning
            .is_some()
    );
    coord
        .send_command_with_reply(EngineCommand::IgnoreLeakPath("APP.EXE".into()))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert!(
        coord
            .snapshot()
            .processes
            .rows
            .iter()
            .find(|r| r.key == app_key)
            .unwrap()
            .leak_warning
            .is_none()
    );

    // 4. AppHistory periodic save_history while leased AND shutdown flush when dirty.
    let _hist_lease = coord.acquire_lease(MonitorDomain::AppHistory);
    std::thread::sleep(Duration::from_millis(20));
    coord
        .send_command_with_reply(EngineCommand::ForceRefresh)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(2), || {
            state.mgmt.save_history_calls.load(Ordering::Acquire) >= 1
        }),
        "periodic AppHistory save_history must trigger when leased and dirty"
    );
    let saves_before_shutdown = state.mgmt.save_history_calls.load(Ordering::Acquire);
    // Accumulate one more tick so shutdown has dirty deltas to flush cleanly.
    drop(_hist_lease);
    coord
        .send_command_with_reply(EngineCommand::ForceRefresh)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    coord.shutdown();
    assert!(
        state.mgmt.save_history_calls.load(Ordering::Acquire) > saves_before_shutdown,
        "shutdown must flush remaining dirty AppHistory via save_history"
    );
}

#[test]
fn details_working_set_delta_tracks_signed_growth_shrink_and_sorting() {
    const MIB: u64 = 1024 * 1024;
    let mut grow = MockProcSpec::new(201, 10, 0, "grow.exe");
    grow.private_mem_bytes = 100 * MIB;
    let mut shrink = MockProcSpec::new(202, 10, 0, "shrink.exe");
    shrink.private_mem_bytes = 200 * MIB;
    let mut steady = MockProcSpec::new(203, 10, 0, "steady.exe");
    steady.private_mem_bytes = 150 * MIB;

    let k_grow = grow.key;
    let k_shrink = shrink.key;
    let k_steady = steady.key;

    let state = MockState::new(vec![grow, shrink, steady]);
    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };
    let coord = EngineCoordinator::with_config(backend, settings, EngineConfig::default()).unwrap();
    let _details_lease = coord.acquire_lease(MonitorDomain::Details);

    // Ensure the initial tick and Details projection have completed before mutating overrides.
    coord
        .send_command_with_reply(EngineCommand::SetDetailsSort {
            column: DetailsSortColumn::WorkingSetDelta,
            direction: SortDirection::Descending,
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();

    // Tick 1: baseline observation for all processes -> delta must be 0.
    let snap1 = coord.snapshot();
    assert_eq!(snap1.details.rows.len(), 3);
    for row in &snap1.details.rows {
        assert_eq!(
            row.working_set_delta_bytes, 0,
            "initial sample must report 0 working set delta for {}",
            row.name
        );
    }

    // Tick 2: grow (+100 MiB), shrink (-64 MiB), steady (0).
    {
        let mut ov = state.dynamic_mem_override.lock();
        ov.insert(k_grow, 200 * MIB);
        ov.insert(k_shrink, 136 * MIB);
        ov.insert(k_steady, 150 * MIB);
    }
    coord
        .send_command_with_reply(EngineCommand::ForceRefresh)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();

    // Sort descending: largest positive growth first, largest shrink last.
    coord
        .send_command_with_reply(EngineCommand::SetDetailsSort {
            column: DetailsSortColumn::WorkingSetDelta,
            direction: SortDirection::Descending,
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let snap_desc = coord.snapshot();
    let desc_pairs: Vec<(ProcessKey, i64)> = snap_desc
        .details
        .rows
        .iter()
        .map(|r| (r.key, r.working_set_delta_bytes))
        .collect();
    assert_eq!(
        desc_pairs,
        vec![
            (k_grow, (100 * MIB) as i64),
            (k_steady, 0),
            (k_shrink, -((64 * MIB) as i64)),
        ]
    );

    // Sort ascending: largest negative shrink first, largest positive growth last.
    coord
        .send_command_with_reply(EngineCommand::SetDetailsSort {
            column: DetailsSortColumn::WorkingSetDelta,
            direction: SortDirection::Ascending,
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let snap_asc = coord.snapshot();
    let asc_pairs: Vec<(ProcessKey, i64)> = snap_asc
        .details
        .rows
        .iter()
        .map(|r| (r.key, r.working_set_delta_bytes))
        .collect();
    assert_eq!(
        asc_pairs,
        vec![
            (k_shrink, -((64 * MIB) as i64)),
            (k_steady, 0),
            (k_grow, (100 * MIB) as i64),
        ]
    );

    // Tick 3: working sets unchanged -> deltas return to 0.
    coord
        .send_command_with_reply(EngineCommand::ForceRefresh)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let snap3 = coord.snapshot();
    for row in &snap3.details.rows {
        assert_eq!(row.working_set_delta_bytes, 0);
    }

    coord.shutdown();
}
