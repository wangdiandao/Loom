use loom_types::{
    AppHistoryRecord, ProcessArch, ProcessHotSample, ProcessTableSlice, ServiceRecord,
    ServiceState, StartupEntry, StartupImpact, UacVirtualization, UserSessionProcess,
    UserSessionRecord, UserSessionState, UsersRowProjection,
};

use super::*;
use crate::query::ParsedQuery;

#[allow(clippy::too_many_arguments)]
fn make_sample(
    scratch: &mut ProcessSampleScratch<()>,
    pid: u32,
    create_time: u64,
    parent_pid: u32,
    name: &str,
    category: ProcessCategory,
    cpu_100ns: u64,
    disk_bytes: u64,
) {
    let (name_offset, name_len) = scratch.push_str(name);
    scratch.samples.push(ProcessHotSample {
        key: ProcessKey::new(pid, create_time),
        parent_pid,
        kernel_time_100ns: cpu_100ns / 2,
        user_time_100ns: cpu_100ns - cpu_100ns / 2,
        private_working_set_bytes: 100 * 1024 * 1024,
        disk_read_bytes: disk_bytes,
        category,
        name_offset,
        name_len,
        ..ProcessHotSample::default()
    });
}

#[test]
fn delta_calculator_computes_accurate_cpu_and_disk_rates() {
    let mut engine = ProjectionEngine::new();
    let mut scratch = ProcessSampleScratch::<()>::new();
    scratch.perf.logical_cpu_count = 4;
    scratch.perf.mem_total_bytes = 1_000 * 1024 * 1024;
    make_sample(
        &mut scratch,
        42,
        100,
        0,
        "worker.exe",
        ProcessCategory::App,
        10_000_000,
        1_000_000,
    );

    let t0 = Timestamp100Ns::new(100_000_000);
    engine.ingest_process_sample(t0, &scratch, 4);

    let cold = ColdMetaCache::new();
    let cfg = ViewProjectionConfig::default();
    let s0 = engine.project_processes(&cfg, &cold, 1);
    let row0 = s0
        .rows
        .iter()
        .find(|r| r.kind == ProcessRowKind::Process)
        .unwrap();
    assert_eq!(row0.cpu_permille, 0);
    assert_eq!(row0.disk_bytes_per_sec, 0);

    // +250 ms elapsed (2_500_000 ticks), 4 CPUs => 10_000_000 ticks = 100% total CPU.
    // Give worker.exe 2_500_000 CPU ticks => 25.0% (250 permille) and +500_000 disk bytes => 2_000_000 B/s.
    scratch.clear();
    scratch.perf.logical_cpu_count = 4;
    scratch.perf.mem_total_bytes = 1_000 * 1024 * 1024;
    make_sample(
        &mut scratch,
        42,
        100,
        0,
        "worker.exe",
        ProcessCategory::App,
        12_500_000,
        1_500_000,
    );
    let t1 = Timestamp100Ns::new(102_500_000);
    engine.ingest_process_sample(t1, &scratch, 4);

    let s1 = engine.project_processes(&cfg, &cold, 2);
    let row1 = s1
        .rows
        .iter()
        .find(|r| r.kind == ProcessRowKind::Process)
        .unwrap();
    assert_eq!(row1.cpu_permille, 250);
    assert_eq!(row1.disk_bytes_per_sec, 2_000_000);
    assert_eq!(row1.memory_permille, 100);
}

#[test]
fn search_in_tree_mode_auto_expands_ancestors() {
    let mut engine = ProjectionEngine::new();
    let mut scratch = ProcessSampleScratch::<()>::new();
    scratch.perf.logical_cpu_count = 4;
    make_sample(
        &mut scratch,
        1,
        10,
        0,
        "root.exe",
        ProcessCategory::System,
        0,
        0,
    );
    make_sample(
        &mut scratch,
        2,
        20,
        1,
        "mid.exe",
        ProcessCategory::Background,
        0,
        0,
    );
    make_sample(
        &mut scratch,
        3,
        30,
        2,
        "target_leaf.exe",
        ProcessCategory::App,
        0,
        0,
    );
    make_sample(
        &mut scratch,
        4,
        40,
        0,
        "unrelated.exe",
        ProcessCategory::App,
        0,
        0,
    );

    engine.ingest_process_sample(Timestamp100Ns::new(1_000_000), &scratch, 4);

    let cold = ColdMetaCache::new();
    let mut cfg = ViewProjectionConfig {
        process_view_mode: ProcessViewMode::Tree,
        search_query: "target_leaf".to_owned(),
        ..ViewProjectionConfig::default()
    };

    let slice = engine.project_processes(&cfg, &cold, 1);
    let names: Vec<&str> = slice.rows.iter().map(|r| r.name.as_ref()).collect();
    assert_eq!(names, vec!["root.exe", "mid.exe", "target_leaf.exe"]);
    assert_eq!(slice.rows[0].depth, 0);
    assert_eq!(slice.rows[1].depth, 1);
    assert_eq!(slice.rows[2].depth, 2);

    // Search by PID also matches and auto-expands ancestors.
    cfg.search_query = "3".to_owned();
    let by_pid = engine.project_processes(&cfg, &cold, 2);
    assert!(by_pid.rows.iter().any(|r| r.key.pid == 3));
}

#[allow(clippy::too_many_arguments)]
fn make_detailed_sample(
    scratch: &mut ProcessSampleScratch<()>,
    pid: u32,
    create_time: u64,
    parent_pid: u32,
    session_id: u32,
    name: &str,
    category: ProcessCategory,
    status: ProcessStatusFlags,
    cpu_100ns: u64,
    mem_bytes: u64,
    disk_bytes: u64,
) {
    let (name_offset, name_len) = scratch.push_str(name);
    scratch.samples.push(ProcessHotSample {
        key: ProcessKey::new(pid, create_time),
        parent_pid,
        session_id,
        kernel_time_100ns: cpu_100ns / 2,
        user_time_100ns: cpu_100ns - cpu_100ns / 2,
        private_working_set_bytes: mem_bytes,
        disk_read_bytes: disk_bytes,
        category,
        status,
        name_offset,
        name_len,
        ..ProcessHotSample::default()
    });
}

#[test]
fn windows_grouped_un_nests_apps_and_background_daemons_from_explorer() {
    let mut engine = ProjectionEngine::with_platform(PlatformProfile::WINDOWS);
    let mut scratch = ProcessSampleScratch::<()>::new();
    scratch.perf.logical_cpu_count = 4;
    scratch.perf.mem_total_bytes = 16 * 1024 * 1024 * 1024;

    // explorer.exe spawns independent GUI apps and a background tray daemon.
    make_sample(
        &mut scratch,
        100,
        10,
        0,
        "explorer.exe",
        ProcessCategory::App,
        0,
        0,
    );
    for (pid, name) in [
        (200, "Antigravity.exe"),
        (201, "chrome.exe"),
        (202, "loom-app.exe"),
        (203, "QQ.exe"),
        (204, "Weixin.exe"),
    ] {
        make_sample(
            &mut scratch,
            pid,
            20 + u64::from(pid),
            100,
            name,
            ProcessCategory::App,
            0,
            0,
        );
    }
    make_sample(
        &mut scratch,
        300,
        500,
        100,
        "OneDrive.exe",
        ProcessCategory::Background,
        0,
        0,
    );

    engine.ingest_process_sample(Timestamp100Ns::new(1_000_000), &scratch, 4);

    let cold = ColdMetaCache::new();
    let cfg = ViewProjectionConfig {
        process_view_mode: ProcessViewMode::Grouped,
        process_sort_column: ProcessSortColumn::Name,
        process_sort_direction: SortDirection::Ascending,
        ..ViewProjectionConfig::default()
    };

    let slice = engine.project_processes(&cfg, &cold, 1);

    // Apps section header reports all 6 top-level apps, including explorer.exe.
    assert_eq!(
        slice.rows[0].kind,
        ProcessRowKind::SectionHeader {
            category: ProcessCategory::App,
            count: 6
        }
    );

    let app_rows: Vec<&ProcessRowProjection> = slice.rows[1..7].iter().collect();
    for row in &app_rows {
        assert_eq!(row.kind, ProcessRowKind::Process);
        assert_eq!(row.depth, 0, "{} must be top-level (depth 0)", row.name);
        assert!(!row.has_children);
        assert_eq!(row.child_count, 0);
    }
    let explorer = app_rows
        .iter()
        .find(|r| r.name.eq_ignore_ascii_case("explorer.exe"))
        .expect("explorer.exe must appear in Apps");
    assert!(!explorer.has_children);
    assert_eq!(explorer.child_count, 0);

    // Background daemon spawned by explorer.exe remains in Background, not under explorer.exe.
    assert_eq!(
        slice.rows[7].kind,
        ProcessRowKind::SectionHeader {
            category: ProcessCategory::Background,
            count: 1
        }
    );
    assert_eq!(slice.rows[8].name.as_ref(), "OneDrive.exe");
    assert_eq!(slice.rows[8].depth, 0);
}

#[test]
fn windows_grouped_never_nests_under_windowless_explorer() {
    let mut engine = ProjectionEngine::with_platform(PlatformProfile::WINDOWS);
    let mut scratch = ProcessSampleScratch::<()>::new();
    scratch.perf.logical_cpu_count = 4;

    // When no File Explorer folder window is open, explorer.exe can be Background or System.
    // In either case it must NEVER adopt different-image child processes.
    make_sample(
        &mut scratch,
        100,
        10,
        0,
        "explorer.exe",
        ProcessCategory::Background,
        0,
        0,
    );
    make_sample(
        &mut scratch,
        200,
        20,
        100,
        "chrome.exe",
        ProcessCategory::Background,
        0,
        0,
    );
    make_sample(
        &mut scratch,
        201,
        30,
        200,
        "chrome.exe",
        ProcessCategory::Background,
        0,
        0,
    );
    make_sample(
        &mut scratch,
        300,
        40,
        100,
        "Weixin.exe",
        ProcessCategory::Background,
        0,
        0,
    );

    engine.ingest_process_sample(Timestamp100Ns::new(1_000_000), &scratch, 4);

    let cold = ColdMetaCache::new();
    let cfg = ViewProjectionConfig {
        process_view_mode: ProcessViewMode::Grouped,
        process_sort_column: ProcessSortColumn::Name,
        process_sort_direction: SortDirection::Ascending,
        ..ViewProjectionConfig::default()
    };

    let slice = engine.project_processes(&cfg, &cold, 1);
    assert_eq!(
        slice.rows[0].kind,
        ProcessRowKind::SectionHeader {
            category: ProcessCategory::Background,
            count: 3
        }
    );

    let names: Vec<(&str, u32, u64)> = slice.rows[1..]
        .iter()
        .map(|r| (r.name.as_ref(), r.child_count, r.memory_bytes))
        .collect();
    let mib = 1024 * 1024;
    assert_eq!(
        names,
        vec![
            ("chrome.exe", 2, 200 * mib),
            ("explorer.exe", 0, 100 * mib),
            ("Weixin.exe", 0, 100 * mib),
        ]
    );
}

#[test]
fn windows_grouped_aggregates_by_app_identity_and_excludes_different_exes() {
    let mut engine = ProjectionEngine::with_platform(PlatformProfile::WINDOWS);
    let mib = 1024 * 1024;
    let populate = |s: &mut ProcessSampleScratch<()>, tick: u64| {
        s.clear();
        s.perf.logical_cpu_count = 4;
        s.perf.mem_total_bytes = 2_000 * mib;

        // Shell launcher.
        make_detailed_sample(
            s,
            100,
            10,
            0,
            1,
            "explorer.exe",
            ProcessCategory::App,
            ProcessStatusFlags::empty(),
            0,
            50 * mib,
            0,
        );

        // QQ: 1 visible-window leader + 4 windowless QQ.exe helpers = QQ (5).
        // Plus a child crashpad_handler.exe that MUST stay in Background (not pulled into QQ).
        make_detailed_sample(
            s,
            200,
            100,
            100,
            1,
            "QQ.exe",
            ProcessCategory::App,
            ProcessStatusFlags::empty(),
            tick * 500_000,
            100 * mib,
            tick * 250_000,
        );
        for i in 1..=4u32 {
            let status = if i == 1 {
                ProcessStatusFlags::EFFICIENCY_MODE
            } else if i == 2 {
                ProcessStatusFlags::SUSPENDED
            } else {
                ProcessStatusFlags::empty()
            };
            make_detailed_sample(
                s,
                200 + i,
                100 + u64::from(i) * 10,
                if i == 4 { 9999 } else { 200 },
                1,
                "QQ.exe",
                ProcessCategory::Background,
                status,
                tick * 250_000,
                50 * mib,
                if i == 1 { tick * 250_000 } else { 0 },
            );
        }
        make_detailed_sample(
            s,
            250,
            160,
            200,
            1,
            "crashpad_handler.exe",
            ProcessCategory::Background,
            ProcessStatusFlags::empty(),
            0,
            12 * mib,
            0,
        );

        // Weixin: 2 visible-window Weixin.exe + 3 windowless Weixin.exe = single Weixin (5) group!
        // Plus 10x WeChatAppEx.exe and crashpad_handler.exe children that MUST stay in Background!
        make_detailed_sample(
            s,
            300,
            200,
            100,
            1,
            "Weixin.exe",
            ProcessCategory::App,
            ProcessStatusFlags::empty(),
            tick * 500_000,
            120 * mib,
            0,
        );
        make_detailed_sample(
            s,
            301,
            210,
            300,
            1,
            "Weixin.exe",
            ProcessCategory::App,
            ProcessStatusFlags::empty(),
            0,
            80 * mib,
            0,
        );
        for i in 2..=4u32 {
            make_detailed_sample(
                s,
                300 + i,
                200 + u64::from(i) * 10,
                300,
                1,
                "Weixin.exe",
                ProcessCategory::Background,
                ProcessStatusFlags::empty(),
                0,
                40 * mib,
                0,
            );
        }
        make_detailed_sample(
            s,
            350,
            260,
            300,
            1,
            "WeChatAppEx.exe",
            ProcessCategory::Background,
            ProcessStatusFlags::empty(),
            0,
            20 * mib,
            0,
        );
        for i in 1..10u32 {
            make_detailed_sample(
                s,
                350 + i,
                260 + u64::from(i),
                350,
                1,
                "WeChatAppEx.exe",
                ProcessCategory::Background,
                ProcessStatusFlags::empty(),
                0,
                10 * mib,
                0,
            );
        }

        // A fully suspended 2-process app -> group status MUST include SUSPENDED.
        make_detailed_sample(
            s,
            400,
            300,
            100,
            1,
            "SuspendedApp.exe",
            ProcessCategory::App,
            ProcessStatusFlags::SUSPENDED,
            0,
            30 * mib,
            0,
        );
        make_detailed_sample(
            s,
            401,
            310,
            400,
            1,
            "SuspendedApp.exe",
            ProcessCategory::Background,
            ProcessStatusFlags::SUSPENDED,
            0,
            10 * mib,
            0,
        );
    };

    let mut scratch = ProcessSampleScratch::<()>::new();
    populate(&mut scratch, 0);
    engine.ingest_process_sample(Timestamp100Ns::new(100_000_000), &scratch, 4);

    // +250 ms elapsed (2_500_000 ticks), 4 CPUs => 10_000_000 ticks = 1000 permille.
    populate(&mut scratch, 1);
    engine.ingest_process_sample(Timestamp100Ns::new(102_500_000), &scratch, 4);

    let cold = ColdMetaCache::new();
    let mut cfg = ViewProjectionConfig {
        process_view_mode: ProcessViewMode::Grouped,
        process_sort_column: ProcessSortColumn::Memory,
        process_sort_direction: SortDirection::Descending,
        ..ViewProjectionConfig::default()
    };

    let slice = engine.project_processes(&cfg, &cold, 2);

    // 4 top-level App groups: Weixin (320 MiB), QQ (300 MiB), explorer (50 MiB), SuspendedApp (40 MiB).
    assert_eq!(
        slice.rows[0].kind,
        ProcessRowKind::SectionHeader {
            category: ProcessCategory::App,
            count: 4
        }
    );
    let app_names: Vec<(&str, u32, u64)> = slice.rows[1..5]
        .iter()
        .map(|r| (r.name.as_ref(), r.child_count, r.memory_bytes))
        .collect();
    assert_eq!(
        app_names,
        vec![
            ("Weixin.exe", 5, 320 * mib),
            ("QQ.exe", 5, 300 * mib),
            ("explorer.exe", 0, 50 * mib),
            ("SuspendedApp.exe", 2, 40 * mib),
        ]
    );

    let qq = &slice.rows[2];
    assert_eq!(qq.key, ProcessKey::new(200, 100));
    assert!(qq.has_children);
    assert!(!qq.is_expanded);
    assert_eq!(qq.child_count, 5);
    assert_eq!(qq.cpu_permille, 150);
    assert_eq!(qq.memory_bytes, 300 * mib);
    assert_eq!(qq.memory_permille, 150);
    assert_eq!(qq.disk_bytes_per_sec, 2_000_000);
    assert!(qq.status.contains(ProcessStatusFlags::EFFICIENCY_MODE));
    assert!(!qq.status.contains(ProcessStatusFlags::SUSPENDED));

    let suspended_app = &slice.rows[4];
    assert!(suspended_app.status.contains(ProcessStatusFlags::SUSPENDED));

    // Background section MUST contain WeChatAppEx.exe (10) and crashpad_handler.exe (not inside Weixin/QQ!).
    assert_eq!(
        slice.rows[5].kind,
        ProcessRowKind::SectionHeader {
            category: ProcessCategory::Background,
            count: 2
        }
    );
    let bg_names: Vec<(&str, u32)> = slice.rows[6..]
        .iter()
        .map(|r| (r.name.as_ref(), r.child_count))
        .collect();
    assert_eq!(
        bg_names,
        vec![("WeChatAppEx.exe", 10), ("crashpad_handler.exe", 0)]
    );

    // Expanding QQ reveals its 4 other QQ.exe members at depth 1 with zero duplicate ProcessKeys.
    cfg.process_expanded.insert(ProcessKey::new(200, 100));
    let expanded = engine.project_processes(&cfg, &cold, 3);
    let mut seen_keys = HashSet::new();
    for row in expanded
        .rows
        .iter()
        .filter(|r| r.kind == ProcessRowKind::Process)
    {
        assert!(seen_keys.insert(row.key), "duplicate key {:?}", row.key);
    }
    let qq_idx = expanded
        .rows
        .iter()
        .position(|r| r.key == ProcessKey::new(200, 100))
        .unwrap();
    assert!(expanded.rows[qq_idx].is_expanded);
    let qq_children: Vec<&str> = expanded.rows[qq_idx + 1..qq_idx + 5]
        .iter()
        .map(|r| r.name.as_ref())
        .collect();
    assert_eq!(qq_children, vec!["QQ.exe", "QQ.exe", "QQ.exe", "QQ.exe"]);
    assert!(
        expanded.rows[qq_idx + 1..qq_idx + 5]
            .iter()
            .all(|r| r.depth == 1)
    );

    // Searching by a child PID inside QQ auto-expands QQ so the matching child is visible.
    cfg.process_expanded.clear();
    cfg.search_query = "204".to_owned();
    let searched = engine.project_processes(&cfg, &cold, 4);
    let searched_pids: Vec<(u32, u16)> = searched
        .rows
        .iter()
        .filter(|r| r.kind == ProcessRowKind::Process)
        .map(|r| (r.key.pid, r.depth))
        .collect();
    assert_eq!(searched_pids, vec![(200, 0), (204, 1)]);
    assert!(searched.rows[1].is_expanded);
}

#[test]
fn windows_grouped_packaged_apps_group_by_app_group_id() {
    use loom_types::ProcessColdMeta;

    let mut engine = ProjectionEngine::with_platform(PlatformProfile::WINDOWS);
    let mut scratch = ProcessSampleScratch::<()>::new();
    scratch.perf.logical_cpu_count = 4;

    make_detailed_sample(
        &mut scratch,
        500,
        100,
        100,
        1,
        "WindowsTerminal.exe",
        ProcessCategory::App,
        ProcessStatusFlags::empty(),
        0,
        80 * 1024 * 1024,
        0,
    );
    make_detailed_sample(
        &mut scratch,
        501,
        110,
        500,
        1,
        "OpenConsole.exe",
        ProcessCategory::Background,
        ProcessStatusFlags::EFFICIENCY_MODE,
        0,
        20 * 1024 * 1024,
        0,
    );
    make_detailed_sample(
        &mut scratch,
        502,
        120,
        500,
        1,
        "pwsh.exe",
        ProcessCategory::Background,
        ProcessStatusFlags::empty(),
        0,
        40 * 1024 * 1024,
        0,
    );
    engine.ingest_process_sample(Timestamp100Ns::new(1_000_000), &scratch, 4);

    let mut cold = ColdMetaCache::new();
    let pkg: Arc<str> = Arc::from("Microsoft.WindowsTerminal_8wekyb3d8bbwe");
    cold.insert_process(ProcessColdMeta {
        key: ProcessKey::new(500, 100),
        exe_path: Some(Arc::from(
            r"C:\Program Files\WindowsApps\WT\WindowsTerminal.exe",
        )),
        command_line: None,
        user_name: None,
        arch: ProcessArch::X64,
        uac_virtualization: UacVirtualization::NotAllowed,
        efficiency_mode: false,
        is_elevated: false,
        app_group_id: Some(Arc::clone(&pkg)),
        asset: None,
    });
    cold.insert_process(ProcessColdMeta {
        key: ProcessKey::new(501, 110),
        exe_path: Some(Arc::from(
            r"C:\Program Files\WindowsApps\WT\OpenConsole.exe",
        )),
        command_line: None,
        user_name: None,
        arch: ProcessArch::X64,
        uac_virtualization: UacVirtualization::NotAllowed,
        efficiency_mode: false,
        is_elevated: false,
        app_group_id: Some(pkg),
        asset: None,
    });

    let cfg = ViewProjectionConfig {
        process_view_mode: ProcessViewMode::Grouped,
        ..ViewProjectionConfig::default()
    };

    let slice = engine.project_processes(&cfg, &cold, 1);
    assert_eq!(
        slice.rows[0].kind,
        ProcessRowKind::SectionHeader {
            category: ProcessCategory::App,
            count: 1
        }
    );
    // WindowsTerminal.exe and OpenConsole.exe share app_group_id -> grouped as WindowsTerminal (2) (100 MiB).
    assert_eq!(slice.rows[1].name.as_ref(), "WindowsTerminal.exe");
    assert_eq!(slice.rows[1].child_count, 2);
    assert_eq!(slice.rows[1].memory_bytes, 100 * 1024 * 1024);
    assert!(
        slice.rows[1]
            .status
            .contains(ProcessStatusFlags::EFFICIENCY_MODE)
    );

    // Unpackaged pwsh.exe stays in Background!
    assert_eq!(
        slice.rows[2].kind,
        ProcessRowKind::SectionHeader {
            category: ProcessCategory::Background,
            count: 1
        }
    );
    assert_eq!(slice.rows[3].name.as_ref(), "pwsh.exe");
}

#[test]
fn windows_tree_mode_preserves_explorer_parent_child_edges() {
    let mut engine = ProjectionEngine::with_platform(PlatformProfile::WINDOWS);
    let mut scratch = ProcessSampleScratch::<()>::new();
    scratch.perf.logical_cpu_count = 4;

    make_sample(
        &mut scratch,
        100,
        10,
        0,
        "explorer.exe",
        ProcessCategory::App,
        0,
        0,
    );
    make_sample(
        &mut scratch,
        200,
        20,
        100,
        "chrome.exe",
        ProcessCategory::App,
        0,
        0,
    );
    make_sample(
        &mut scratch,
        201,
        30,
        200,
        "chrome.exe",
        ProcessCategory::Background,
        0,
        0,
    );
    engine.ingest_process_sample(Timestamp100Ns::new(1_000_000), &scratch, 4);

    let cold = ColdMetaCache::new();
    let mut cfg = ViewProjectionConfig {
        process_view_mode: ProcessViewMode::Tree,
        ..ViewProjectionConfig::default()
    };
    cfg.process_expanded.insert(ProcessKey::new(100, 10));
    cfg.process_expanded.insert(ProcessKey::new(200, 20));

    let slice = engine.project_processes(&cfg, &cold, 1);
    let tree_shape: Vec<(u32, u16)> = slice.rows.iter().map(|r| (r.key.pid, r.depth)).collect();
    assert_eq!(tree_shape, vec![(100, 0), (200, 1), (201, 2)]);
}

#[test]
fn linux_and_macos_preserve_standard_grouped_behavior() {
    for profile in [PlatformProfile::LINUX, PlatformProfile::MACOS] {
        let mut engine = ProjectionEngine::with_platform(profile);
        let mut scratch = ProcessSampleScratch::<()>::new();
        scratch.perf.logical_cpu_count = 4;

        make_sample(
            &mut scratch,
            100,
            10,
            0,
            "explorer.exe",
            ProcessCategory::App,
            0,
            0,
        );
        make_sample(
            &mut scratch,
            200,
            20,
            100,
            "chrome",
            ProcessCategory::App,
            0,
            0,
        );
        make_sample(
            &mut scratch,
            201,
            30,
            200,
            "chrome",
            ProcessCategory::Background,
            0,
            0,
        );
        engine.ingest_process_sample(Timestamp100Ns::new(1_000_000), &scratch, 4);

        let cold = ColdMetaCache::new();
        let cfg = ViewProjectionConfig {
            process_view_mode: ProcessViewMode::Grouped,
            ..ViewProjectionConfig::default()
        };

        let slice = engine.project_processes(&cfg, &cold, 1);
        // Non-Windows keeps category-local parent nesting (App child stays under App parent)
        // and keeps Background helpers in the Background section.
        assert_eq!(
            slice.rows[0].kind,
            ProcessRowKind::SectionHeader {
                category: ProcessCategory::App,
                count: 2
            },
            "failed for {profile:?}"
        );
        assert_eq!(slice.rows[1].key.pid, 100);
        assert!(slice.rows[1].has_children);
        assert_eq!(slice.rows[1].child_count, 1);
        assert_eq!(
            slice.rows[2].kind,
            ProcessRowKind::SectionHeader {
                category: ProcessCategory::Background,
                count: 1
            },
            "failed for {profile:?}"
        );
        assert_eq!(slice.rows[3].key.pid, 201);
    }
}

#[test]
fn windows_grouped_webview2_host_attribution_and_role_population() {
    use loom_types::{ProcessColdMeta, WebView2Role};

    let mut engine = ProjectionEngine::with_platform(PlatformProfile::WINDOWS);
    let mut scratch = ProcessSampleScratch::<()>::new();
    scratch.perf.logical_cpu_count = 4;

    // Host App: Teams.exe (PID 100) + two msedgewebview2.exe subprocesses (200 by flag, 201 by ancestor chain).
    make_detailed_sample(
        &mut scratch,
        100,
        10,
        1,
        1,
        "Teams.exe",
        ProcessCategory::App,
        ProcessStatusFlags::empty(),
        0,
        150 * 1024 * 1024,
        0,
    );
    make_detailed_sample(
        &mut scratch,
        200,
        20,
        999, // detached parent PID, matched via `--webview-exe-name=Teams.exe`
        1,
        "msedgewebview2.exe",
        ProcessCategory::Background,
        ProcessStatusFlags::empty(),
        0,
        80 * 1024 * 1024,
        0,
    );
    make_detailed_sample(
        &mut scratch,
        201,
        30,
        100, // child of Teams.exe, no `--webview-exe-name=` flag -> matched via ancestor chain
        1,
        "msedgewebview2.exe",
        ProcessCategory::Background,
        ProcessStatusFlags::empty(),
        0,
        60 * 1024 * 1024,
        0,
    );

    engine.ingest_process_sample(Timestamp100Ns::new(10_000_000), &scratch, 4);

    let mut cold = ColdMetaCache::new();
    cold.insert_process(ProcessColdMeta {
        key: ProcessKey::new(200, 20),
        exe_path: Some(Arc::from(r"C:\Program Files\WebView2\msedgewebview2.exe")),
        command_line: Some(Arc::from(
            r#"msedgewebview2.exe --type=renderer --webview-exe-name=Teams.exe"#,
        )),
        user_name: None,
        arch: ProcessArch::X64,
        uac_virtualization: UacVirtualization::NotAllowed,
        efficiency_mode: false,
        is_elevated: false,
        app_group_id: None,
        asset: None,
    });
    cold.insert_process(ProcessColdMeta {
        key: ProcessKey::new(201, 30),
        exe_path: Some(Arc::from(r"C:\Program Files\WebView2\msedgewebview2.exe")),
        command_line: Some(Arc::from(
            r#"msedgewebview2.exe --type=utility --utility-sub-type=network.mojom.NetworkService"#,
        )),
        user_name: None,
        arch: ProcessArch::X64,
        uac_virtualization: UacVirtualization::NotAllowed,
        efficiency_mode: false,
        is_elevated: false,
        app_group_id: None,
        asset: None,
    });

    let mut cfg = ViewProjectionConfig {
        process_view_mode: ProcessViewMode::Grouped,
        webview2_host_grouping: true,
        ..ViewProjectionConfig::default()
    };
    cfg.process_expanded.insert(ProcessKey::new(100, 10));

    let grouped = engine.project_processes(&cfg, &cold, 1);
    // Teams.exe absorbs both WebView2 processes -> total group count 3, zero Background section.
    assert_eq!(
        grouped.rows[0].kind,
        ProcessRowKind::SectionHeader {
            category: ProcessCategory::App,
            count: 1
        }
    );
    assert_eq!(grouped.rows[1].key, ProcessKey::new(100, 10));
    assert_eq!(grouped.rows[1].child_count, 3);
    assert_eq!(grouped.rows[1].memory_bytes, 290 * 1024 * 1024);
    assert_eq!(grouped.rows.len(), 4);

    let wv_renderer = grouped
        .rows
        .iter()
        .find(|r| r.key == ProcessKey::new(200, 20))
        .unwrap();
    assert_eq!(wv_renderer.depth, 1);
    assert_eq!(wv_renderer.webview2_role, Some(WebView2Role::Renderer));

    let wv_net = grouped
        .rows
        .iter()
        .find(|r| r.key == ProcessKey::new(201, 30))
        .unwrap();
    assert_eq!(wv_net.depth, 1);
    assert_eq!(wv_net.webview2_role, Some(WebView2Role::Network));
    assert_eq!(wv_net.parent_key, Some(ProcessKey::new(100, 10)));

    // Disabling `webview2_host_grouping` moves WebView2 processes back to Background
    // while still populating `webview2_role`.
    cfg.webview2_host_grouping = false;
    let ungrouped = engine.project_processes(&cfg, &cold, 2);
    let teams = ungrouped
        .rows
        .iter()
        .find(|r| r.key == ProcessKey::new(100, 10))
        .unwrap();
    assert_eq!(teams.child_count, 0);
    assert!(ungrouped.rows.iter().any(|r| matches!(
        r.kind,
        ProcessRowKind::SectionHeader {
            category: ProcessCategory::Background,
            ..
        }
    )));
    let details = engine.project_details(&cfg, &cold, 1);
    assert_eq!(
        details
            .rows
            .iter()
            .find(|r| r.key == ProcessKey::new(200, 20))
            .unwrap()
            .webview2_role,
        Some(WebView2Role::Renderer)
    );
}

#[test]
fn hide_windows_system_processes_filters_core_os_but_keeps_interactive_shells() {
    let mut engine = ProjectionEngine::with_platform(PlatformProfile::WINDOWS);
    let mut scratch = ProcessSampleScratch::<()>::new();
    scratch.perf.logical_cpu_count = 4;

    for (pid, name, cat) in [
        (4, "System", ProcessCategory::System),
        (100, "svchost.exe", ProcessCategory::System),
        (101, "csrss.exe", ProcessCategory::System),
        (102, "explorer.exe", ProcessCategory::System),
        (103, "cmd.exe", ProcessCategory::System),
        (104, "pwsh.exe", ProcessCategory::System),
        (105, "wt.exe", ProcessCategory::System),
        (200, "myapp.exe", ProcessCategory::App),
    ] {
        make_sample(&mut scratch, pid, 10, 4, name, cat, 0, 0);
    }
    engine.ingest_process_sample(Timestamp100Ns::new(10_000_000), &scratch, 4);

    let cold = ColdMetaCache::new();
    let cfg = ViewProjectionConfig {
        hide_windows_system_processes: true,
        ..ViewProjectionConfig::default()
    };
    let slice = engine.project_processes(&cfg, &cold, 1);
    let visible_names: Vec<&str> = slice
        .rows
        .iter()
        .filter(|r| r.kind == ProcessRowKind::Process)
        .map(|r| r.name.as_ref())
        .collect();
    assert!(visible_names.contains(&"myapp.exe"));
    assert!(visible_names.contains(&"explorer.exe"));
    assert!(visible_names.contains(&"cmd.exe"));
    assert!(visible_names.contains(&"pwsh.exe"));
    assert!(visible_names.contains(&"wt.exe"));
    assert!(!visible_names.contains(&"System"));
    assert!(!visible_names.contains(&"svchost.exe"));
    assert!(!visible_names.contains(&"csrss.exe"));

    // In Tree mode, visible children whose hidden parent (PID 4) is filtered out are promoted to roots.
    let tree_cfg = ViewProjectionConfig {
        process_view_mode: ProcessViewMode::Tree,
        hide_windows_system_processes: true,
        ..ViewProjectionConfig::default()
    };
    let tree_slice = engine.project_processes(&tree_cfg, &cold, 2);
    assert!(
        tree_slice
            .rows
            .iter()
            .any(|r| r.name.as_ref() == "myapp.exe" && r.depth == 0)
    );
    assert!(
        !tree_slice
            .rows
            .iter()
            .any(|r| r.name.as_ref() == "svchost.exe")
    );

    let details = engine.project_details(&cfg, &cold, 1);
    assert!(
        !details
            .rows
            .iter()
            .any(|r| r.name.as_ref() == "svchost.exe")
    );
    assert!(details.rows.iter().any(|r| r.name.as_ref() == "pwsh.exe"));
}

#[test]
fn order_freeze_locks_row_order_while_updating_metrics_and_pruning_exited_rows() {
    let mut engine = ProjectionEngine::new();
    let mut scratch = ProcessSampleScratch::<()>::new();
    scratch.perf.logical_cpu_count = 4;

    let k_a = ProcessKey::new(10, 100);
    let k_b = ProcessKey::new(20, 100);
    let k_c = ProcessKey::new(30, 100);
    make_detailed_sample(
        &mut scratch,
        10,
        100,
        0,
        1,
        "alpha.exe",
        ProcessCategory::Background,
        ProcessStatusFlags::empty(),
        0,
        300 * 1024 * 1024,
        0,
    );
    make_detailed_sample(
        &mut scratch,
        20,
        100,
        0,
        1,
        "beta.exe",
        ProcessCategory::Background,
        ProcessStatusFlags::empty(),
        0,
        200 * 1024 * 1024,
        0,
    );
    make_detailed_sample(
        &mut scratch,
        30,
        100,
        0,
        1,
        "gamma.exe",
        ProcessCategory::Background,
        ProcessStatusFlags::empty(),
        0,
        100 * 1024 * 1024,
        0,
    );
    engine.ingest_process_sample(Timestamp100Ns::new(10_000_000), &scratch, 4);

    let cold = ColdMetaCache::new();
    let mut cfg = ViewProjectionConfig {
        process_sort_column: ProcessSortColumn::Memory,
        process_sort_direction: SortDirection::Descending,
        ..ViewProjectionConfig::default()
    };

    let s1 = engine.project_processes(&cfg, &cold, 1);
    let order1: Vec<ProcessKey> = s1
        .rows
        .iter()
        .filter(|r| r.kind == ProcessRowKind::Process)
        .map(|r| r.key)
        .collect();
    assert_eq!(order1, vec![k_a, k_b, k_c]);

    // Freeze row order, then reverse memory usage (gamma > alpha) and remove exited beta.
    cfg.freeze_process_order(&s1.rows);
    scratch.clear();
    make_detailed_sample(
        &mut scratch,
        10,
        100,
        0,
        1,
        "alpha.exe",
        ProcessCategory::Background,
        ProcessStatusFlags::empty(),
        0,
        50 * 1024 * 1024,
        0,
    );
    make_detailed_sample(
        &mut scratch,
        30,
        100,
        0,
        1,
        "gamma.exe",
        ProcessCategory::Background,
        ProcessStatusFlags::empty(),
        0,
        900 * 1024 * 1024,
        0,
    );
    engine.ingest_process_sample(Timestamp100Ns::new(20_000_000), &scratch, 4);

    let s2 = engine.project_processes(&cfg, &cold, 2);
    let rows2: Vec<(ProcessKey, u64)> = s2
        .rows
        .iter()
        .filter(|r| r.kind == ProcessRowKind::Process)
        .map(|r| (r.key, r.memory_bytes))
        .collect();
    // Order stays alpha -> gamma (beta removed cleanly) while live memory updates in place!
    assert_eq!(
        rows2,
        vec![(k_a, 50 * 1024 * 1024), (k_c, 900 * 1024 * 1024)]
    );

    // Unfreezing immediately re-sorts gamma ahead of alpha.
    cfg.process_order_frozen = false;
    cfg.frozen_process_order.clear();
    let s3 = engine.project_processes(&cfg, &cold, 3);
    let order3: Vec<ProcessKey> = s3
        .rows
        .iter()
        .filter(|r| r.kind == ProcessRowKind::Process)
        .map(|r| r.key)
        .collect();
    assert_eq!(order3, vec![k_c, k_a]);
}

#[test]
fn sort_deadband_suppresses_small_jitter_reorders_on_threshold_and_resets_on_config_change() {
    let mut engine = ProjectionEngine::new();
    let mut scratch = ProcessSampleScratch::<()>::new();
    let cold = ColdMetaCache::new();
    let k_a = ProcessKey::new(10, 100);
    let k_b = ProcessKey::new(20, 100);

    // 1s tick (10_000_000 ticks), 1 logical CPU => 10_000 ticks = 1 permille (0.1%).
    let mut cpu_a: u64 = 0;
    let mut cpu_b: u64 = 0;
    let mut disk_a: u64 = 0;
    let mut disk_b: u64 = 0;
    let mut ts: u64 = 100_000_000;

    let mut ingest = |eng: &mut ProjectionEngine,
                      da_permille: u64,
                      db_permille: u64,
                      da_disk: u64,
                      db_disk: u64| {
        cpu_a += da_permille * 10_000;
        cpu_b += db_permille * 10_000;
        disk_a += da_disk;
        disk_b += db_disk;
        ts += 10_000_000;
        scratch.clear();
        scratch.perf.logical_cpu_count = 1;
        make_detailed_sample(
            &mut scratch,
            10,
            100,
            0,
            1,
            "alpha.exe",
            ProcessCategory::Background,
            ProcessStatusFlags::empty(),
            cpu_a,
            100 * 1024 * 1024,
            disk_a,
        );
        make_detailed_sample(
            &mut scratch,
            20,
            100,
            0,
            1,
            "beta.exe",
            ProcessCategory::Background,
            ProcessStatusFlags::empty(),
            cpu_b,
            100 * 1024 * 1024,
            disk_b,
        );
        eng.ingest_process_sample(Timestamp100Ns::new(ts), &scratch, 1);
    };

    // Tick 0 (baseline).
    ingest(&mut engine, 0, 0, 0, 0);
    let mut cfg = ViewProjectionConfig {
        process_sort_column: ProcessSortColumn::Cpu,
        process_sort_direction: SortDirection::Descending,
        details_sort_column: DetailsSortColumn::Cpu,
        details_sort_direction: SortDirection::Descending,
        ..ViewProjectionConfig::default()
    };
    assert!(cfg.sort_deadband_enabled);

    // Tick 1: both at 10 permille (1.0%) -> alphabetical tie-break [A, B].
    ingest(&mut engine, 10, 10, 0, 0);
    let s1 = engine.project_processes(&cfg, &cold, 1);
    let p_order = |s: &ProcessTableSlice| -> Vec<ProcessKey> {
        s.rows
            .iter()
            .filter(|r| r.kind == ProcessRowKind::Process)
            .map(|r| r.key)
            .collect()
    };
    assert_eq!(p_order(&s1), vec![k_a, k_b]);
    let d1 = engine.project_details(&cfg, &cold, 1);
    assert_eq!(
        d1.rows.iter().map(|r| r.key).collect::<Vec<_>>(),
        vec![k_a, k_b]
    );

    // Tick 2: B rises by 0.1% (11 vs 10 permille, abs_diff = 1 < 2) -> order MUST stay [A, B].
    ingest(&mut engine, 10, 11, 0, 0);
    let s2 = engine.project_processes(&cfg, &cold, 2);
    assert_eq!(p_order(&s2), vec![k_a, k_b]);
    let d2 = engine.project_details(&cfg, &cold, 2);
    assert_eq!(
        d2.rows.iter().map(|r| r.key).collect::<Vec<_>>(),
        vec![k_a, k_b]
    );

    // Tick 3: B jumps to 12 permille (abs_diff(12, 10) == 2 >= CPU_SORT_DEADBAND_PERMILLE) -> MUST reorder [B, A].
    ingest(&mut engine, 10, 12, 0, 0);
    let s3 = engine.project_processes(&cfg, &cold, 3);
    assert_eq!(p_order(&s3), vec![k_b, k_a]);
    let d3 = engine.project_details(&cfg, &cold, 3);
    assert_eq!(
        d3.rows.iter().map(|r| r.key).collect::<Vec<_>>(),
        vec![k_b, k_a]
    );

    // Tick 4: Small flip back (A=11, B=11 -> abs_diff from sticky (10, 12) is 1 < 2) -> stays [B, A].
    ingest(&mut engine, 11, 11, 0, 0);
    let s4 = engine.project_processes(&cfg, &cold, 4);
    assert_eq!(p_order(&s4), vec![k_b, k_a]);

    // Changing sort direction resets deadband immediately -> live tie (11, 11) breaks alphabetically [A, B].
    cfg.process_sort_direction = SortDirection::Ascending;
    let s5 = engine.project_processes(&cfg, &cold, 5);
    assert_eq!(p_order(&s5), vec![k_a, k_b]);

    // Test Disk rate deadband (16 KiB/s): 100 KiB/s vs 108 KiB/s stays stable; >= 16 KiB/s reorders.
    cfg.process_sort_column = ProcessSortColumn::Disk;
    cfg.process_sort_direction = SortDirection::Descending;
    ingest(&mut engine, 0, 0, 100 * 1024, 100 * 1024);
    let sd1 = engine.project_processes(&cfg, &cold, 6);
    assert_eq!(p_order(&sd1), vec![k_a, k_b]);

    ingest(&mut engine, 0, 0, 100 * 1024, 108 * 1024);
    let sd2 = engine.project_processes(&cfg, &cold, 7);
    assert_eq!(p_order(&sd2), vec![k_a, k_b]);

    ingest(&mut engine, 0, 0, 100 * 1024, 116 * 1024);
    let sd3 = engine.project_processes(&cfg, &cold, 8);
    assert_eq!(p_order(&sd3), vec![k_b, k_a]);

    // Disabling sort_deadband_enabled immediately sorts strictly by live metrics.
    cfg.sort_deadband_enabled = false;
    ingest(&mut engine, 0, 0, 101 * 1024, 100 * 1024);
    let sd4 = engine.project_processes(&cfg, &cold, 9);
    assert_eq!(p_order(&sd4), vec![k_a, k_b]);
}

#[test]
fn sort_deadband_randomized_stress_maintains_strict_weak_ordering() {
    let mut engine = ProjectionEngine::with_platform(PlatformProfile::WINDOWS);
    let mut scratch = ProcessSampleScratch::<()>::with_capacity(120, 4096);
    let cold = ColdMetaCache::new();
    let mut rng: u64 = 0x9e37_79b9_7f4a_7c15;
    let next_u64 = |state: &mut u64| -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    };

    let mut acc_cpu = vec![0u64; 120];
    let mut acc_disk = vec![0u64; 120];
    let mut cfg = ViewProjectionConfig {
        process_sort_column: ProcessSortColumn::Cpu,
        process_sort_direction: SortDirection::Descending,
        details_sort_column: DetailsSortColumn::Cpu,
        details_sort_direction: SortDirection::Descending,
        sort_deadband_enabled: true,
        ..ViewProjectionConfig::default()
    };

    for tick in 1..=80u64 {
        scratch.clear();
        scratch.perf.logical_cpu_count = 4;
        scratch.perf.mem_total_bytes = 16 * 1024 * 1024 * 1024;
        for i in 0..120usize {
            let jitter_permille = next_u64(&mut rng) % 6; // 0..=5 permille around deadband boundary
            let jitter_disk = (next_u64(&mut rng) % 40) * 1024; // 0..40 KiB/s
            acc_cpu[i] += jitter_permille * 40_000;
            acc_disk[i] += jitter_disk;
            let cat = match i % 3 {
                0 => ProcessCategory::App,
                1 => ProcessCategory::Background,
                _ => ProcessCategory::System,
            };
            let name = match i % 6 {
                0 => "app_a.exe",
                1 => "app_b.exe",
                2 => "worker.exe",
                3 => "svc.exe",
                4 => "helper.exe",
                _ => "daemon.exe",
            };
            make_detailed_sample(
                &mut scratch,
                100 + i as u32,
                1_000 + i as u64,
                0,
                1,
                name,
                cat,
                ProcessStatusFlags::empty(),
                acc_cpu[i],
                (i as u64 + 1) * 1024 * 1024,
                acc_disk[i],
            );
        }
        engine.ingest_process_sample(
            Timestamp100Ns::new(100_000_000 + tick * 10_000_000),
            &scratch,
            4,
        );

        // Cycle across deadband columns and view modes to exercise every comparator path.
        cfg.process_sort_column = match tick % 4 {
            0 => ProcessSortColumn::Cpu,
            1 => ProcessSortColumn::Disk,
            2 => ProcessSortColumn::Network,
            _ => ProcessSortColumn::Gpu,
        };
        cfg.process_view_mode = if tick % 2 == 0 {
            ProcessViewMode::Grouped
        } else {
            ProcessViewMode::Tree
        };
        let ps = engine.project_processes(&cfg, &cold, tick);
        assert_eq!(ps.total_process_count, 120);

        cfg.details_view_mode = if tick % 2 == 0 {
            DetailsViewMode::Flat
        } else {
            DetailsViewMode::Tree
        };
        let ds = engine.project_details(&cfg, &cold, tick);
        assert_eq!(ds.rows.len(), 120);
    }
}

#[test]
fn offloaded_services_startup_users_and_app_history_cjk_schwartzian_filtering_and_sorting() {
    let mut engine = ProjectionEngine::new();
    let cold = ColdMetaCache::new();
    let mut cfg = ViewProjectionConfig::default();

    // 1. Services: CJK descriptions, PID filter, and all sort columns.
    let services = vec![
        ServiceRecord {
            name: Arc::from("Spooler"),
            display_name: Arc::from("Print Spooler"),
            description: Some(Arc::from("打印后台处理程序服务")),
            pid: Some(2040),
            state: ServiceState::Running,
            group: Some(Arc::from("SpoolerGroup")),
        },
        ServiceRecord {
            name: Arc::from("Dhcp"),
            display_name: Arc::from("DHCP 客户端"),
            description: None,
            pid: Some(1200),
            state: ServiceState::Running,
            group: Some(Arc::from("LocalServiceNetworkRestricted")),
        },
        ServiceRecord {
            name: Arc::from("wuauserv"),
            display_name: Arc::from("Windows Update"),
            description: Some(Arc::from("启用 Windows 更新的检测、下载和安装")),
            pid: None,
            state: ServiceState::Stopped,
            group: Some(Arc::from("netsvcs")),
        },
    ];

    cfg.services_sort_column = ServicesSortColumn::Name;
    cfg.services_sort_direction = SortDirection::Ascending;
    let svc_name = engine.project_services(&services, &cfg, 1);
    assert_eq!(svc_name.visible_indices, vec![1, 0, 2]);

    cfg.services_sort_column = ServicesSortColumn::Pid;
    cfg.services_sort_direction = SortDirection::Descending;
    let svc_pid = engine.project_services(&services, &cfg, 2);
    assert_eq!(svc_pid.visible_indices, vec![0, 1, 2]);

    cfg.services_sort_column = ServicesSortColumn::Description;
    cfg.services_sort_direction = SortDirection::Ascending;
    let svc_desc = engine.project_services(&services, &cfg, 3);
    assert_eq!(svc_desc.visible_indices.len(), 3);
    // ASCII "DHCP..." sorts before CJK "启用..." and "打印..."
    assert_eq!(svc_desc.visible_indices[0], 1);

    // CJK search query matches Chinese description.
    cfg.search_query = "打印后台".to_owned();
    let svc_cjk = engine.project_services(&services, &cfg, 4);
    assert_eq!(svc_cjk.visible_indices, vec![0]);

    // Strict PID filter overrides non-matching services.
    cfg.search_query.clear();
    cfg.services_filter_pid = Some(1200);
    let svc_pid_filter = engine.project_services(&services, &cfg, 5);
    assert_eq!(svc_pid_filter.visible_indices, vec![1]);
    cfg.services_filter_pid = None;

    // 2. Startup: CJK + publisher/command search + Impact/Status sorting.
    let startup = vec![
        StartupEntry {
            id: Arc::from("hkcu:wechat"),
            name: Arc::from("微信 WeChat"),
            publisher: Some(Arc::from("Tencent")),
            command: Some(Arc::from(r"C:\Tencent\WeChat.exe --autorun")),
            file_path: None,
            icon: None,
            enabled: true,
            impact: StartupImpact::High,
        },
        StartupEntry {
            id: Arc::from("hkcu:discord"),
            name: Arc::from("Discord"),
            publisher: Some(Arc::from("Discord Inc.")),
            command: Some(Arc::from(r"C:\Discord\Update.exe --processStart")),
            file_path: None,
            icon: None,
            enabled: false,
            impact: StartupImpact::Low,
        },
        StartupEntry {
            id: Arc::from("hklm:security"),
            name: Arc::from("安全中心"),
            publisher: Some(Arc::from("Microsoft Corporation")),
            command: None,
            file_path: None,
            icon: None,
            enabled: true,
            impact: StartupImpact::NotMeasured,
        },
    ];

    cfg.startup_sort_column = StartupSortColumn::Impact;
    cfg.startup_sort_direction = SortDirection::Descending;
    let st_impact = engine.project_startup(Some(4_200), &startup, &cfg, 1);
    assert_eq!(st_impact.visible_indices, vec![0, 1, 2]);
    assert_eq!(st_impact.last_bios_time_ms, Some(4_200));

    cfg.search_query = "微信".to_owned();
    let st_cjk = engine.project_startup(Some(4_200), &startup, &cfg, 2);
    assert_eq!(st_cjk.visible_indices, vec![0]);

    // 3. Users: session sorting, explicit expansion, and search auto-expand.
    let sessions = vec![
        UserSessionRecord {
            session_id: 1,
            user_name: Arc::from("Alice"),
            state: UserSessionState::Active,
            process_count: 2,
            cpu_permille: 120,
            memory_bytes: 512 * 1024 * 1024,
            disk_bytes_per_sec: 1024,
            net_bytes_per_sec: 2048,
            processes: vec![
                UserSessionProcess {
                    key: ProcessKey::new(101, 100),
                    name: Arc::from("code.exe"),
                    display_name: Arc::from("Visual Studio Code"),
                    status: ProcessStatusFlags::empty(),
                    cpu_permille: 100,
                    memory_bytes: 400 * 1024 * 1024,
                    disk_bytes_per_sec: 0,
                    net_bytes_per_sec: 0,
                },
                UserSessionProcess {
                    key: ProcessKey::new(102, 200),
                    name: Arc::from("notepad.exe"),
                    display_name: Arc::from("记事本"),
                    status: ProcessStatusFlags::empty(),
                    cpu_permille: 20,
                    memory_bytes: 112 * 1024 * 1024,
                    disk_bytes_per_sec: 0,
                    net_bytes_per_sec: 0,
                },
            ],
        },
        UserSessionRecord {
            session_id: 2,
            user_name: Arc::from("Bob"),
            state: UserSessionState::Disconnected,
            process_count: 1,
            cpu_permille: 10,
            memory_bytes: 64 * 1024 * 1024,
            disk_bytes_per_sec: 0,
            net_bytes_per_sec: 0,
            processes: vec![UserSessionProcess {
                key: ProcessKey::new(201, 300),
                name: Arc::from("cmd.exe"),
                display_name: Arc::from("cmd.exe"),
                status: ProcessStatusFlags::empty(),
                cpu_permille: 10,
                memory_bytes: 64 * 1024 * 1024,
                disk_bytes_per_sec: 0,
                net_bytes_per_sec: 0,
            }],
        },
    ];

    cfg.search_query.clear();
    cfg.users_sort_column = UsersSortColumn::Cpu;
    cfg.users_sort_direction = SortDirection::Descending;
    let users_collapsed = engine.project_users(&sessions, &cfg, &cold, 1);
    assert_eq!(users_collapsed.rows.len(), 2);
    assert!(matches!(
        users_collapsed.rows[0],
        UsersRowProjection::Session {
            session_id: 1,
            is_expanded: false,
            has_children: true,
            ..
        }
    ));

    cfg.users_expanded.insert(1);
    let users_expanded = engine.project_users(&sessions, &cfg, &cold, 2);
    assert_eq!(users_expanded.rows.len(), 4);
    assert!(matches!(
        &users_expanded.rows[1],
        UsersRowProjection::Process { process, .. } if process.key.pid == 101
    ));
    assert!(matches!(
        &users_expanded.rows[2],
        UsersRowProjection::Process { process, .. } if process.key.pid == 102
    ));

    // Search for CJK child display name auto-expands collapsed session 1.
    cfg.users_expanded.clear();
    cfg.search_query = "记事本".to_owned();
    let users_auto = engine.project_users(&sessions, &cfg, &cold, 3);
    assert_eq!(users_auto.rows.len(), 2);
    assert!(matches!(
        users_auto.rows[0],
        UsersRowProjection::Session {
            session_id: 1,
            is_expanded: true,
            ..
        }
    ));
    assert!(matches!(
        &users_auto.rows[1],
        UsersRowProjection::Process { process, .. } if process.key.pid == 102
    ));

    // 4. AppHistory: CJK filtering and column sorting.
    let history = vec![
        AppHistoryRecord {
            app_id: Arc::from("app.alpha"),
            name: Arc::from("浏览器"),
            cpu_time_100ns: 1_200_000_000,
            network_bytes: 10_000,
            metered_network_bytes: 500,
        },
        AppHistoryRecord {
            app_id: Arc::from("app.beta"),
            name: Arc::from("Beta Studio"),
            cpu_time_100ns: 36_000_000_000,
            network_bytes: 500_000,
            metered_network_bytes: 10_000,
        },
        AppHistoryRecord {
            app_id: Arc::from("app.gamma"),
            name: Arc::from("下载工具"),
            cpu_time_100ns: 300_000_000,
            network_bytes: 2_000_000,
            metered_network_bytes: 0,
        },
    ];

    cfg.search_query.clear();
    cfg.app_history_sort_column = AppHistorySortColumn::Network;
    cfg.app_history_sort_direction = SortDirection::Descending;
    let hist_net = engine.project_app_history(Some(1_700_000_000), &history, &cfg, 1);
    assert_eq!(
        hist_net
            .visible_indices
            .iter()
            .map(|&i| hist_net.items[i].app_id.as_ref())
            .collect::<Vec<_>>(),
        vec!["app.gamma", "app.beta", "app.alpha"]
    );

    cfg.search_query = "下载".to_owned();
    let hist_cjk = engine.project_app_history(Some(1_700_000_000), &history, &cfg, 2);
    assert_eq!(hist_cjk.visible_indices.len(), 1);
    assert_eq!(
        hist_cjk.items[hist_cjk.visible_indices[0]].app_id.as_ref(),
        "app.gamma"
    );
}

#[test]
fn stress_1000_processes_and_500_cjk_services_projection_is_fast_and_deterministic() {
    let mut engine = ProjectionEngine::with_platform(PlatformProfile::WINDOWS);
    let mut scratch = ProcessSampleScratch::<()>::with_capacity(1_000, 32 * 1024);
    let cold = ColdMetaCache::new();

    let fill_1000 = |s: &mut ProcessSampleScratch<()>, tick: u64| {
        s.clear();
        s.perf.logical_cpu_count = 16;
        s.perf.mem_total_bytes = 32 * 1024 * 1024 * 1024;
        for i in 1..=1_000u32 {
            let (cat, name) = match i % 5 {
                0 => (ProcessCategory::App, "chrome.exe"),
                1 => (ProcessCategory::Background, "chrome.exe"),
                2 => (ProcessCategory::Background, "code.exe"),
                3 => (ProcessCategory::System, "svchost.exe"),
                _ => (ProcessCategory::Background, "worker.exe"),
            };
            make_detailed_sample(
                s,
                i * 4,
                u64::from(i) * 100,
                if i > 20 { (i % 20 + 1) * 4 } else { 0 },
                1,
                name,
                cat,
                ProcessStatusFlags::empty(),
                tick * u64::from(i) * 1_000,
                u64::from(i) * 1024 * 1024,
                tick * u64::from(i) * 256,
            );
        }
    };

    let services: Vec<ServiceRecord> = (0..500u32)
        .map(|i| ServiceRecord {
            name: Arc::from(format!("LoomSvc_{i:04}")),
            display_name: Arc::from(format!("系统服务组件 {i:04}")),
            description: Some(Arc::from(format!(
                "{} — 负责处理第 {:04} 号后台任务与网络代理",
                match i % 4 {
                    0 => "打印后台处理程序",
                    1 => "网络位置感知服务",
                    2 => "安全中心监控服务",
                    _ => "高性能索引服务",
                },
                i
            ))),
            pid: (i % 2 == 0).then_some(1000 + i),
            state: if i % 2 == 0 {
                ServiceState::Running
            } else {
                ServiceState::Stopped
            },
            group: Some(Arc::from("netsvcs")),
        })
        .collect();

    let cfg = ViewProjectionConfig {
        process_view_mode: ProcessViewMode::Grouped,
        process_sort_column: ProcessSortColumn::Cpu,
        process_sort_direction: SortDirection::Descending,
        services_sort_column: ServicesSortColumn::Description,
        services_sort_direction: SortDirection::Ascending,
        ..ViewProjectionConfig::default()
    };

    // Warm up caches and scratch buffers.
    fill_1000(&mut scratch, 0);
    engine.ingest_process_sample(Timestamp100Ns::new(100_000_000), &scratch, 16);
    let _ = engine.project_processes(&cfg, &cold, 1);
    let baseline_services = engine.project_services(&services, &cfg, 1);
    assert_eq!(baseline_services.visible_indices.len(), 500);

    let mut durations = Vec::with_capacity(10);
    for tick in 1..=10u64 {
        fill_1000(&mut scratch, tick);
        let started = Instant::now();
        engine.ingest_process_sample(
            Timestamp100Ns::new(100_000_000 + tick * 10_000_000),
            &scratch,
            16,
        );
        let proc_slice = engine.project_processes(&cfg, &cold, tick + 1);
        let svc_slice = engine.project_services(&services, &cfg, tick + 1);
        durations.push(started.elapsed());

        assert_eq!(proc_slice.total_process_count, 1_000);
        assert_eq!(svc_slice.visible_indices, baseline_services.visible_indices);
    }

    durations.sort();
    let median = durations[durations.len() / 2];
    assert!(
        median < std::time::Duration::from_millis(50),
        "1000-process + 500-CJK-service projection median latency {median:?} exceeded 50 ms debug budget (all: {durations:?})"
    );
}

#[test]
fn details_static_sort_cache_and_dense_frozen_ranks_stay_accurate() {
    let mut engine = ProjectionEngine::new();
    let mut cold = ColdMetaCache::new();
    let mut scratch = ProcessSampleScratch::<()>::with_capacity(8, 256);

    let populate = |scratch: &mut ProcessSampleScratch<()>, cpu_a: u64, cpu_b: u64| {
        scratch.clear();
        for (pid, name, cpu) in [
            (300u32, "zebra.exe", cpu_a),
            (100u32, "alpha.exe", cpu_b),
            (200u32, "middle.exe", 50_000),
        ] {
            let (name_offset, name_len) = scratch.push_str(name);
            scratch.samples.push(ProcessHotSample {
                key: ProcessKey::new(pid, 1000),
                name_offset,
                name_len,
                user_time_100ns: cpu,
                private_working_set_bytes: 16 * 1024 * 1024,
                ..ProcessHotSample::default()
            });
        }
    };

    let cfg = ViewProjectionConfig {
        details_view_mode: DetailsViewMode::Flat,
        details_sort_column: DetailsSortColumn::Name,
        details_sort_direction: SortDirection::Ascending,
        ..ViewProjectionConfig::default()
    };
    let empty_query = ParsedQuery::default();

    populate(&mut scratch, 10_000, 20_000);
    engine.ingest_process_sample(Timestamp100Ns::new(10_000_000), &scratch, 4);
    let s1 = engine.project_details_with_query(&cfg, &empty_query, &cold, 1);
    let names1: Vec<&str> = s1.rows.iter().map(|r| r.name.as_ref()).collect();
    assert_eq!(names1, vec!["alpha.exe", "middle.exe", "zebra.exe"]);
    assert!(engine.scratch.details_static_order.valid);

    // Next tick with identical process set & cold version hits the static-order cache
    // while still projecting live updated CPU counters.
    populate(&mut scratch, 900_000, 300_000);
    engine.ingest_process_sample(Timestamp100Ns::new(20_000_000), &scratch, 4);
    let s2 = engine.project_details_with_query(&cfg, &empty_query, &cold, 2);
    let names2: Vec<&str> = s2.rows.iter().map(|r| r.name.as_ref()).collect();
    assert_eq!(names2, vec!["alpha.exe", "middle.exe", "zebra.exe"]);
    assert!(s2.rows[2].cpu_permille > s2.rows[0].cpu_permille);

    // Cold metadata arrival bumps ColdMetaCache::version and invalidates publisher order cleanly.
    let v_before = cold.version();
    cold.insert_process(loom_types::ProcessColdMeta {
        key: ProcessKey::new(300, 1000),
        exe_path: Some(Arc::from(r"C:\zebra.exe")),
        command_line: None,
        user_name: None,
        arch: ProcessArch::X64,
        uac_virtualization: UacVirtualization::NotAllowed,
        efficiency_mode: false,
        is_elevated: false,
        app_group_id: None,
        asset: Some(Arc::new(loom_types::ExeStaticAsset {
            normalized_path: Arc::from(r"c:\zebra.exe"),
            publisher: Some(Arc::from("AAA Corp")),
            file_description: None,
            icon: None,
            is_network_path: false,
        })),
    });
    assert!(cold.version() > v_before);

    let pub_cfg = ViewProjectionConfig {
        details_sort_column: DetailsSortColumn::Publisher,
        details_sort_direction: SortDirection::Descending,
        ..cfg.clone()
    };
    let s3 = engine.project_details_with_query(&pub_cfg, &empty_query, &cold, 3);
    assert_eq!(s3.rows[0].key.pid, 300);

    // Dense frozen ranks preserve captured order regardless of live metric swings.
    let mut frozen_cfg = ViewProjectionConfig {
        details_sort_column: DetailsSortColumn::Cpu,
        details_sort_direction: SortDirection::Descending,
        ..cfg
    };
    frozen_cfg.freeze_details_order(&s1.rows);
    let s4 = engine.project_details_with_query(&frozen_cfg, &empty_query, &cold, 4);
    let frozen_pids: Vec<u32> = s4.rows.iter().map(|r| r.key.pid).collect();
    assert_eq!(frozen_pids, vec![100, 200, 300]);
}
