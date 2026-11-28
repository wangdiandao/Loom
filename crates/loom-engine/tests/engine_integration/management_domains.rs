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
        icon: None,
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
        icon: None,
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
fn offloaded_domain_sort_filter_expand_and_search_commands_are_lease_gated() {
    let mut p_alice = MockProcSpec::new(101, 10, 0, "code.exe");
    p_alice.session_id = 1;
    p_alice.cpu_delta_per_tick = 1_000_000;
    p_alice.private_mem_bytes = 256 * 1024 * 1024;
    let mut p_bob = MockProcSpec::new(201, 20, 0, "notepad.exe");
    p_bob.session_id = 2;
    p_bob.cpu_delta_per_tick = 200_000;
    p_bob.private_mem_bytes = 64 * 1024 * 1024;

    let state = MockState::new(vec![p_alice, p_bob]);
    *state.mgmt.services.lock() = vec![
        ServiceRecord {
            name: Arc::from("Spooler"),
            display_name: Arc::from("Print Spooler"),
            description: Some(Arc::from("打印后台处理程序")),
            pid: Some(2040),
            state: ServiceState::Running,
            group: Some(Arc::from("SpoolerGroup")),
        },
        ServiceRecord {
            name: Arc::from("Dhcp"),
            display_name: Arc::from("DHCP Client"),
            description: Some(Arc::from("网络配置服务")),
            pid: Some(1200),
            state: ServiceState::Stopped,
            group: Some(Arc::from("netsvcs")),
        },
    ];
    *state.mgmt.startup_entries.lock() = vec![
        StartupEntry {
            id: Arc::from("id:z"),
            name: Arc::from("ZetaApp"),
            publisher: Some(Arc::from("Beta Corp")),
            command: None,
            file_path: None,
            icon: None,
            enabled: false,
            impact: StartupImpact::Low,
        },
        StartupEntry {
            id: Arc::from("id:a"),
            name: Arc::from("AlphaApp"),
            publisher: Some(Arc::from("Acme Corp")),
            command: None,
            file_path: None,
            icon: None,
            enabled: true,
            impact: StartupImpact::High,
        },
    ];
    *state.mgmt.sessions.lock() = vec![
        UserSessionRecord {
            session_id: 1,
            user_name: Arc::from("Alice"),
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
            user_name: Arc::from("Bob"),
            state: UserSessionState::Disconnected,
            process_count: 0,
            cpu_permille: 0,
            memory_bytes: 0,
            disk_bytes_per_sec: 0,
            net_bytes_per_sec: 0,
            processes: Vec::new(),
        },
    ];
    *state.mgmt.history_since.lock() = Some(1_700_000_000);
    *state.mgmt.history_items.lock() = vec![
        AppHistoryRecord {
            app_id: Arc::from("app.one"),
            name: Arc::from("AlphaBrowser"),
            cpu_time_100ns: 100_000_000,
            network_bytes: 5_000_000,
            metered_network_bytes: 1_000,
        },
        AppHistoryRecord {
            app_id: Arc::from("app.two"),
            name: Arc::from("BetaEditor"),
            cpu_time_100ns: 900_000_000,
            network_bytes: 100_000,
            metered_network_bytes: 50_000,
        },
    ];

    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };
    let coord = EngineCoordinator::new(backend, settings).unwrap();
    assert!(wait_until(Duration::from_secs(2), || {
        coord.snapshot().revisions.processes >= 1
    }));

    // 1. Sending sort/filter/expand commands while domains are UNLEASED must NOT project or poll them.
    for cmd in [
        EngineCommand::SetServicesSort {
            column: ServicesSortColumn::Pid,
            direction: SortDirection::Descending,
        },
        EngineCommand::SetServicesFilterPid(None),
        EngineCommand::SetStartupSort {
            column: StartupSortColumn::Impact,
            direction: SortDirection::Descending,
        },
        EngineCommand::SetUsersSort {
            column: UsersSortColumn::User,
            direction: SortDirection::Ascending,
        },
        EngineCommand::ToggleUserSessionExpanded(1),
        EngineCommand::SetAppHistorySort {
            column: AppHistorySortColumn::Network,
            direction: SortDirection::Descending,
        },
    ] {
        let ev = coord
            .send_command_with_reply(cmd)
            .unwrap()
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(ev.is_ok());
    }

    let snap_unleased = coord.snapshot();
    assert_eq!(snap_unleased.revisions.services, 0);
    assert_eq!(snap_unleased.revisions.startup, 0);
    assert_eq!(snap_unleased.revisions.users, 0);
    assert_eq!(snap_unleased.revisions.app_history, 0);
    assert_eq!(state.mgmt.list_services_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.mgmt.list_startup_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.mgmt.list_sessions_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.mgmt.read_history_calls.load(Ordering::Acquire), 0);

    // 2. Acquire all 4 leases -> initial projections immediately honor the configured sort & expand state!
    let _svc_lease = coord.acquire_lease(MonitorDomain::Services);
    let _st_lease = coord.acquire_lease(MonitorDomain::Startup);
    let _usr_lease = coord.acquire_lease(MonitorDomain::Users);
    let _hist_lease = coord.acquire_lease(MonitorDomain::AppHistory);

    assert!(wait_until(Duration::from_secs(2), || {
        let r = coord.snapshot().revisions;
        r.services >= 1 && r.startup >= 1 && r.users >= 1 && r.app_history >= 1
    }));

    let snap = coord.snapshot();
    assert_eq!(snap.services.sort_column, ServicesSortColumn::Pid);
    assert_eq!(snap.services.visible_indices, vec![0, 1]);
    assert_eq!(snap.startup.sort_column, StartupSortColumn::Impact);
    assert_eq!(snap.startup.visible_indices, vec![1, 0]);
    assert_eq!(snap.users.sort_column, UsersSortColumn::User);
    assert_eq!(snap.users.rows.len(), 3); // Alice (expanded) + code.exe + Bob
    assert!(matches!(
        snap.users.rows[0],
        UsersRowProjection::Session {
            session_id: 1,
            is_expanded: true,
            ..
        }
    ));
    assert_eq!(snap.app_history.sort_column, AppHistorySortColumn::Network);
    assert_eq!(
        snap.app_history.items[snap.app_history.visible_indices[0]]
            .app_id
            .as_ref(),
        "app.one"
    );

    // 3. Record slow poll counts, then issue live sort/filter/search commands while leased (RefreshSpeed::Paused).
    // All 4 domains MUST re-project immediately on the fast lane with ZERO additional slow polls!
    let svc_polls = state.mgmt.list_services_calls.load(Ordering::Acquire);
    let st_polls = state.mgmt.list_startup_calls.load(Ordering::Acquire);
    let usr_polls = state.mgmt.list_sessions_calls.load(Ordering::Acquire);
    let hist_polls = state.mgmt.read_history_calls.load(Ordering::Acquire);

    let rev_before = coord.snapshot().revisions;
    coord
        .send_command_with_reply(EngineCommand::SetServicesFilterPid(Some(1200)))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let s_pid = coord.snapshot();
    assert!(s_pid.revisions.services > rev_before.services);
    assert_eq!(s_pid.services.filter_pid, Some(1200));
    assert_eq!(s_pid.services.visible_indices, vec![1]);

    // Clear PID filter, collapse session 1, then trigger SetSearchQuery across all leased domains!
    coord
        .send_command_with_reply(EngineCommand::SetServicesFilterPid(None))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    coord
        .send_command_with_reply(EngineCommand::ToggleUserSessionExpanded(1))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert_eq!(coord.snapshot().users.rows.len(), 2);

    let rev_pre_search = coord.snapshot().revisions;
    coord
        .send_command_with_reply(EngineCommand::SetSearchQuery("alpha".to_owned()))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let s_search = coord.snapshot();
    assert!(s_search.revisions.services > rev_pre_search.services);
    assert!(s_search.revisions.startup > rev_pre_search.startup);
    assert!(s_search.revisions.users > rev_pre_search.users);
    assert!(s_search.revisions.app_history > rev_pre_search.app_history);
    assert_eq!(s_search.startup.visible_indices, vec![1]); // AlphaApp
    assert_eq!(s_search.app_history.visible_indices.len(), 1);
    assert_eq!(
        s_search.app_history.items[s_search.app_history.visible_indices[0]]
            .app_id
            .as_ref(),
        "app.one"
    );

    // Search matching a child process auto-expands collapsed session 2 (Bob -> notepad.exe).
    coord
        .send_command_with_reply(EngineCommand::SetSearchQuery("notepad".to_owned()))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let s_note = coord.snapshot();
    assert_eq!(s_note.users.rows.len(), 2);
    assert!(matches!(
        s_note.users.rows[0],
        UsersRowProjection::Session {
            session_id: 2,
            is_expanded: true,
            ..
        }
    ));

    // Confirm zero extra slow-lane polls occurred during fast-lane sort/filter/search re-projections!
    assert_eq!(
        state.mgmt.list_services_calls.load(Ordering::Acquire),
        svc_polls
    );
    assert_eq!(
        state.mgmt.list_startup_calls.load(Ordering::Acquire),
        st_polls
    );
    assert_eq!(
        state.mgmt.list_sessions_calls.load(Ordering::Acquire),
        usr_polls
    );
    assert_eq!(
        state.mgmt.read_history_calls.load(Ordering::Acquire),
        hist_polls
    );

    coord.shutdown();
}
