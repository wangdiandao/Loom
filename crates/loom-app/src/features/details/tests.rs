//! Unit and UI integration tests for [`super::DetailsView`] and its submodules.

use std::sync::Arc;

use loom_types::{
    CapabilityFlags, CpuAffinityMask, DetailsRowProjection, DetailsSortColumn, DetailsTableSlice,
    PlatformProfile, ProcessArch, ProcessKey, ProcessPriority, ProcessStatusFlags, SortDirection,
    UacVirtualization,
};

use crate::features::table_columns::{ColumnKey as _, TableColumnLayout};
use crate::i18n;

use super::DetailsView;
use super::affinity_dialog::{AffinitySelectionState, CpuGroupRange, compute_cpu_groups};
use super::columns::{
    COL_UAC_W, DetailsColumn, DetailsLocaleCache, compute_details_viewport_fingerprint,
    table_min_width,
};
use super::state::{
    PRIORITY_MENU_ORDER, can_set_affinity, can_set_priority, can_terminate_details_row,
    default_sort_direction, display_user_name, find_row_index_by_key, format_details_copy_short,
    format_details_row_summary, is_critical_or_system_process, is_eco_mode_eligible,
    next_sort_direction,
};

fn sample_row(pid: u32, create_time_100ns: u64, name: &str) -> DetailsRowProjection {
    DetailsRowProjection {
        key: ProcessKey::new(pid, create_time_100ns),
        parent_key: None,
        depth: 0,
        has_children: false,
        is_expanded: false,
        name: Arc::from(name),
        description: None,
        publisher: None,
        icon: None,
        status: ProcessStatusFlags::empty(),
        user_name: None,
        session_id: 1,
        cpu_permille: 0,
        cpu_time_100ns: 0,
        private_working_set_bytes: 0,
        working_set_bytes: 0,
        working_set_delta_bytes: 0,
        commit_bytes: 0,
        thread_count: 1,
        handle_count: 10,
        priority: ProcessPriority::Normal,
        arch: ProcessArch::X64,
        uac_virtualization: UacVirtualization::NotAllowed,
        gpu_dedicated_bytes: 0,
        gpu_shared_bytes: 0,
        exe_path: None,
        command_line: None,
        leak_warning: None,
        webview2_role: None,
    }
}

#[test]
fn jump_to_details_lookup_tracks_process_key_across_resorts_and_pid_reuse() {
    let target = ProcessKey::new(4242, 9_000_000);
    let reused_pid_old = ProcessKey::new(4242, 1_000_000);

    let initial_rows = vec![
        sample_row(100, 100, "alpha.exe"),
        sample_row(4242, 1_000_000, "old-worker.exe"),
        sample_row(4242, 9_000_000, "target.exe"),
        sample_row(8888, 500, "zeta.exe"),
    ];
    assert_eq!(find_row_index_by_key(&initial_rows, target), Some(2));
    assert_eq!(
        find_row_index_by_key(&initial_rows, reused_pid_old),
        Some(1)
    );

    let resorted_rows = vec![
        sample_row(4242, 9_000_000, "target.exe"),
        sample_row(8888, 500, "zeta.exe"),
        sample_row(100, 100, "alpha.exe"),
    ];
    assert_eq!(find_row_index_by_key(&resorted_rows, target), Some(0));
    assert_eq!(find_row_index_by_key(&resorted_rows, reused_pid_old), None);
}

#[test]
fn affinity_group_slicing_supports_single_and_multi_group_topologies() {
    assert!(compute_cpu_groups(0).is_empty());

    assert_eq!(
        compute_cpu_groups(16),
        vec![CpuGroupRange {
            group_index: 0,
            start_cpu: 0,
            end_cpu_exclusive: 16,
        }]
    );

    assert_eq!(
        compute_cpu_groups(64),
        vec![CpuGroupRange {
            group_index: 0,
            start_cpu: 0,
            end_cpu_exclusive: 64,
        }]
    );

    assert_eq!(
        compute_cpu_groups(96),
        vec![
            CpuGroupRange {
                group_index: 0,
                start_cpu: 0,
                end_cpu_exclusive: 64,
            },
            CpuGroupRange {
                group_index: 1,
                start_cpu: 64,
                end_cpu_exclusive: 96,
            },
        ]
    );

    assert_eq!(
        compute_cpu_groups(144),
        vec![
            CpuGroupRange {
                group_index: 0,
                start_cpu: 0,
                end_cpu_exclusive: 64,
            },
            CpuGroupRange {
                group_index: 1,
                start_cpu: 64,
                end_cpu_exclusive: 128,
            },
            CpuGroupRange {
                group_index: 2,
                start_cpu: 128,
                end_cpu_exclusive: 144,
            },
        ]
    );
}

#[test]
fn affinity_all_processors_toggle_and_indeterminate_state_logic() {
    let mut sel = AffinitySelectionState::new(CpuAffinityMask::all(96));
    assert!(sel.has_multiple_groups());
    assert!(sel.all_checked());
    assert!(!sel.is_indeterminate());
    assert!(!sel.is_empty());

    assert!(sel.set_cpu(70, false));
    assert!(!sel.all_checked());
    assert!(sel.is_indeterminate());
    assert!(!sel.is_empty());
    assert_eq!(sel.mask().enabled_count(), 95);

    sel.toggle_all(false);
    assert!(!sel.all_checked());
    assert!(!sel.is_indeterminate());
    assert!(sel.is_empty());
    assert_eq!(sel.mask().enabled_count(), 0);

    sel.toggle_all(true);
    assert!(sel.all_checked());
    assert!(!sel.is_indeterminate());
    assert!(!sel.is_empty());
    assert!(sel.mask().is_enabled(0));
    assert!(sel.mask().is_enabled(63));
    assert!(sel.mask().is_enabled(64));
    assert!(sel.mask().is_enabled(95));
    assert!(!sel.mask().is_enabled(96));
}

#[test]
fn priority_menu_ordering_and_localized_labels() {
    for pair in PRIORITY_MENU_ORDER.windows(2) {
        assert!(
            pair[0] > pair[1],
            "expected {:?} > {:?} in menu order",
            pair[0],
            pair[1]
        );
    }

    let expected = [
        (
            ProcessPriority::Realtime,
            "details.priority_realtime",
            "Realtime",
            "实时",
        ),
        (ProcessPriority::High, "details.priority_high", "High", "高"),
        (
            ProcessPriority::AboveNormal,
            "details.priority_above_normal",
            "Above normal",
            "高于正常",
        ),
        (
            ProcessPriority::Normal,
            "details.priority_normal",
            "Normal",
            "正常",
        ),
        (
            ProcessPriority::BelowNormal,
            "details.priority_below_normal",
            "Below normal",
            "低于正常",
        ),
        (ProcessPriority::Idle, "details.priority_idle", "Low", "低"),
    ];

    let _locale_guard = i18n::locale_test_lock();
    for (priority, key, en, zh) in expected {
        assert_eq!(rust_i18n::t!(key, locale = "en-US"), en);
        assert_eq!(rust_i18n::t!(key, locale = "zh-CN"), zh);
        let live_label = i18n::process_priority_label(priority);
        assert!(
            live_label == en || live_label == zh,
            "unexpected priority label '{live_label}' for {priority:?}"
        );
    }
}

#[test]
fn eco_mode_eligibility_and_critical_process_guards() {
    let with_eco = CapabilityFlags::ECO_MODE;
    let without_eco = CapabilityFlags::empty();
    let prio_and_affinity = CapabilityFlags::PROCESS_PRIORITY | CapabilityFlags::CPU_AFFINITY;

    assert!(is_eco_mode_eligible(ProcessStatusFlags::empty(), with_eco));
    assert!(is_eco_mode_eligible(
        ProcessStatusFlags::EFFICIENCY_MODE,
        with_eco
    ));
    assert!(!is_eco_mode_eligible(
        ProcessStatusFlags::empty(),
        without_eco
    ));
    assert!(!is_critical_or_system_process(ProcessStatusFlags::empty()));
    assert!(can_terminate_details_row(ProcessStatusFlags::empty()));
    assert!(can_terminate_details_row(ProcessStatusFlags::PROTECTED));
    assert!(can_terminate_details_row(ProcessStatusFlags::CRITICAL));
    assert!(can_set_priority(
        ProcessStatusFlags::empty(),
        prio_and_affinity
    ));
    assert!(can_set_affinity(
        ProcessStatusFlags::empty(),
        prio_and_affinity
    ));
    assert!(!can_set_priority(
        ProcessStatusFlags::empty(),
        CapabilityFlags::empty()
    ));
    assert!(!can_set_affinity(
        ProcessStatusFlags::empty(),
        CapabilityFlags::empty()
    ));

    for blocked in [
        ProcessStatusFlags::SYSTEM_IDLE,
        ProcessStatusFlags::KERNEL,
        ProcessStatusFlags::PROTECTED,
        ProcessStatusFlags::CRITICAL,
    ] {
        assert!(!is_eco_mode_eligible(blocked, with_eco));
        assert!(is_critical_or_system_process(blocked));
    }

    for non_schedulable in [ProcessStatusFlags::SYSTEM_IDLE, ProcessStatusFlags::KERNEL] {
        assert!(!can_terminate_details_row(non_schedulable));
        assert!(!can_set_priority(non_schedulable, prio_and_affinity));
        assert!(!can_set_affinity(non_schedulable, prio_and_affinity));
    }
}

#[test]
fn table_min_width_includes_virtualization_column_only_when_supported() {
    let without_uac = table_min_width(CapabilityFlags::empty());
    let with_uac = table_min_width(CapabilityFlags::UAC_VIRTUALIZATION);
    assert!(
        (with_uac - without_uac - COL_UAC_W).abs() < f32::EPSILON,
        "expected UAC column width ({}) delta, got {}",
        COL_UAC_W,
        with_uac - without_uac
    );
}

#[test]
fn sort_direction_toggles_active_column_and_defaults_on_switch() {
    assert_eq!(
        next_sort_direction(
            DetailsSortColumn::Name,
            SortDirection::Ascending,
            DetailsSortColumn::Name
        ),
        SortDirection::Descending
    );
    assert_eq!(
        next_sort_direction(
            DetailsSortColumn::Name,
            SortDirection::Ascending,
            DetailsSortColumn::Cpu
        ),
        SortDirection::Descending
    );
    assert_eq!(
        next_sort_direction(
            DetailsSortColumn::Cpu,
            SortDirection::Descending,
            DetailsSortColumn::Pid
        ),
        SortDirection::Ascending
    );
}

#[test]
fn display_user_name_strips_computer_and_domain_prefix() {
    assert_eq!(display_user_name(Some(r"LAPTOP-M850NT2C\Lenovo")), "Lenovo");
    assert_eq!(display_user_name(Some(r"NT AUTHORITY\SYSTEM")), "SYSTEM");
    assert_eq!(display_user_name(Some("Lenovo")), "Lenovo");
    assert_eq!(display_user_name(Some("")), "—");
    assert_eq!(display_user_name(Some("   ")), "—");
    assert_eq!(display_user_name(None), "—");
    assert_eq!(display_user_name(Some(r"DOMAIN\")), r"DOMAIN\");
}

#[test]
fn details_smart_copy_formats_short_and_structured_summary() {
    let mut row = sample_row(7777, 123_456, "explorer.exe");
    row.user_name = Some(Arc::from(r"WORKSTATION\Alice"));
    row.cpu_permille = 55;
    row.private_working_set_bytes = 128 * 1024 * 1024;
    row.handle_count = 900;
    row.thread_count = 32;
    row.publisher = Some(Arc::from("Microsoft Windows"));
    row.exe_path = Some(Arc::from(r"C:\Windows\explorer.exe"));
    row.command_line = Some(Arc::from(r"C:\Windows\explorer.exe"));

    assert_eq!(format_details_copy_short(&row), "explorer.exe (7777)");
    let summary = format_details_row_summary(&row, "Running", "Normal", "64-bit");
    assert!(summary.contains("Name: explorer.exe"));
    assert!(summary.contains("PID: 7777"));
    assert!(summary.contains("User: Alice"));
    assert!(summary.contains("CPU: 5.5%"));
    assert!(summary.contains("Memory: 128.0 MB"));
    assert!(summary.contains("Handles: 900"));
    assert!(summary.contains("Threads: 32"));
    assert!(summary.contains("Publisher: Microsoft Windows"));
    assert!(summary.contains(r"Path: C:\Windows\explorer.exe"));
}

#[gpui_kit::test]
fn details_view_hover_freeze_ime_search_copy_and_csv_payload(cx: &mut gpui_kit::TestAppContext) {
    use gpui_kit::{AppContext as _, EntityInputHandler as _, VisualTestContext, px, size};
    use loom_config::{AppSettings, ConfigStore};
    use loom_engine::EngineCoordinator;
    use loom_pal::{NativeBackend, PlatformBackend as _};
    use loom_types::{LeakKind, LeakWarning, WebView2Role};

    use crate::bridge::UiStore;
    use crate::shell::ShellHandle;

    cx.update(gpui_kit::init);

    let dir = tempfile::tempdir().expect("tempdir");
    let config_store = ConfigStore::new(dir.path().join("settings.toml"));
    let backend = NativeBackend::initialize().expect("native backend");
    let coordinator =
        Arc::new(EngineCoordinator::new(backend, AppSettings::default()).expect("coordinator"));
    let coordinator_for_shutdown = Arc::clone(&coordinator);

    let store = cx.new(|cx| UiStore::new(coordinator, config_store, AppSettings::default(), cx));
    let shell = ShellHandle::new(|_, _, _| {});

    let store_for_window = store.clone();
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), move |window, cx| {
            let view = cx.new(|cx| DetailsView::new(store_for_window, None, shell, window, cx));
            cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
        })
        .expect("open window")
    });
    cx.simulate_window_resize(window.into(), size(px(1200.), px(720.)));
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    vcx.update(|window, cx| window.draw(cx).clear(cx));

    let view = window
        .read_with(&vcx, |root, _| {
            root.view()
                .clone()
                .downcast::<DetailsView>()
                .expect("root wraps DetailsView")
        })
        .expect("read root");

    let mut row = sample_row(8080, 555_000, "msedgewebview2.exe");
    row.publisher = Some(Arc::from("Microsoft Corporation"));
    row.webview2_role = Some(WebView2Role::Gpu);
    row.leak_warning = Some(LeakWarning {
        kind: LeakKind::Children,
        summary: Arc::from("Sustained child process growth (18 live children, baseline 2)"),
        current_value: 18,
        baseline_value: 2,
    });
    let key = row.key;

    let table = view.read_with(&vcx, |v, _| v.table.clone());
    table.update(&mut vcx, |table, cx| {
        let d = table.delegate_mut();
        d.details = Arc::new(DetailsTableSlice {
            revision: 42,
            rows: vec![row],
            ..Default::default()
        });
        d.selected_key = Some(key);
        cx.notify();
    });
    vcx.update(|window, cx| window.draw(cx).clear(cx));

    table.update(&mut vcx, |table, cx| {
        table.delegate_mut().set_table_hovered(true, cx);
        assert!(table.delegate().order_freeze.is_frozen());
        table.delegate_mut().set_header_hovered(true, cx);
        assert!(!table.delegate().order_freeze.is_frozen());
        table.delegate_mut().set_header_hovered(false, cx);
        assert!(table.delegate().order_freeze.is_frozen());
        table.delegate_mut().set_table_hovered(false, cx);
        assert!(!table.delegate().order_freeze.is_frozen());
    });

    view.update_in(&mut vcx, |this, window, cx| {
        this.replace_text_in_range(None, "gpu", window, cx);
        this.replace_and_mark_text_in_range(None, "ce", None, window, cx);
        this.replace_text_in_range(None, "", window, cx);
    });
    assert_eq!(
        store.read_with(&vcx, |s, _| s.search_query().to_owned()),
        "gpu"
    );

    view.update(&mut vcx, |this, cx| {
        assert!(this.copy_selected_short(cx));
    });
    assert_eq!(
        vcx.read_from_clipboard()
            .and_then(|item| item.text())
            .as_deref(),
        Some("msedgewebview2.exe (8080)")
    );
    view.update(&mut vcx, |this, cx| {
        assert!(this.copy_selected_summary(cx));
    });
    assert!(
        vcx.read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default()
            .contains("PID: 8080")
    );

    let (headers, rows) = table.read_with(&vcx, |t, _| t.delegate().export_visible_csv_payload());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), headers.len());
    assert_eq!(rows[0][0], "msedgewebview2.exe");

    coordinator_for_shutdown.shutdown();
}

#[test]
fn details_viewport_fingerprint_detects_visible_changes_and_ignores_offscreen_and_jitter() {
    let layout = TableColumnLayout::new(DetailsColumn::default_specs(CapabilityFlags::empty()));
    let visible_rows = 0..10;
    let visible_cols = 0..8;
    let ram = 16 * 1024 * 1024 * 1024u64;

    let mut slice = DetailsTableSlice {
        revision: 1,
        rows: (0..60u32)
            .map(|i| {
                let mut r = sample_row(1000 + i, 50_000 + u64::from(i), &format!("p-{i}.exe"));
                r.cpu_permille = 30;
                r.cpu_time_100ns = 10_000_000;
                r.private_working_set_bytes = 128 * 1024 * 1024;
                r
            })
            .collect(),
        ..Default::default()
    };

    let fp_base = compute_details_viewport_fingerprint(
        &slice,
        &layout,
        &visible_rows,
        &visible_cols,
        None,
        None,
        ram,
    );

    slice.rows[50].cpu_permille = 880;
    slice.rows[50].private_working_set_bytes = 2 * 1024 * 1024 * 1024;
    assert_eq!(
        fp_base,
        compute_details_viewport_fingerprint(
            &slice,
            &layout,
            &visible_rows,
            &visible_cols,
            None,
            None,
            ram,
        )
    );

    slice.rows[1].private_working_set_bytes += 512;
    slice.rows[1].cpu_time_100ns = 15_000_000;
    assert_eq!(
        fp_base,
        compute_details_viewport_fingerprint(
            &slice,
            &layout,
            &visible_rows,
            &visible_cols,
            None,
            None,
            ram,
        )
    );

    slice.rows[1].cpu_permille = 150;
    let fp_cpu = compute_details_viewport_fingerprint(
        &slice,
        &layout,
        &visible_rows,
        &visible_cols,
        None,
        None,
        ram,
    );
    assert_ne!(fp_base, fp_cpu);

    let mut loading_slice = slice.clone();
    loading_slice.revision = 0;
    assert_ne!(
        fp_cpu,
        compute_details_viewport_fingerprint(
            &loading_slice,
            &layout,
            &visible_rows,
            &visible_cols,
            None,
            None,
            ram,
        )
    );
}

#[gpui_kit::test]
fn details_view_right_click_column_coord_long_paths_and_viewport_dirty_skip(
    cx: &mut gpui_kit::TestAppContext,
) {
    use gpui_kit::{AppContext as _, Modifiers, MouseButton, VisualTestContext, point, px, size};
    use loom_config::{AppSettings, ConfigStore};
    use loom_engine::EngineCoordinator;
    use loom_pal::{NativeBackend, PlatformBackend as _};
    use loom_ui_kit::DEFAULT_CELL_PREVIEW_CHARS;

    use crate::bridge::UiStore;
    use crate::shell::ShellHandle;

    cx.update(gpui_kit::init);

    let dir = tempfile::tempdir().expect("tempdir");
    let config_store = ConfigStore::new(dir.path().join("settings.toml"));
    let backend = NativeBackend::initialize().expect("native backend");
    let coordinator =
        Arc::new(EngineCoordinator::new(backend, AppSettings::default()).expect("coordinator"));
    let coordinator_for_shutdown = Arc::clone(&coordinator);

    let store = cx.new(|cx| UiStore::new(coordinator, config_store, AppSettings::default(), cx));
    let shell = ShellHandle::new(|_, _, _| {});

    let store_for_window = store.clone();
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), move |window, cx| {
            let view = cx.new(|cx| DetailsView::new(store_for_window, None, shell, window, cx));
            cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
        })
        .expect("open window")
    });
    cx.simulate_window_resize(window.into(), size(px(1200.), px(720.)));
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    vcx.update(|window, cx| window.draw(cx).clear(cx));

    let view = window
        .read_with(&vcx, |root, _| {
            root.view()
                .clone()
                .downcast::<DetailsView>()
                .expect("root wraps DetailsView")
        })
        .expect("read root");
    let table = view.read_with(&vcx, |v, _| v.table.clone());

    coordinator_for_shutdown.shutdown();
    vcx.run_until_parked();

    let long_path: Arc<str> = Arc::from(format!(
        "C:\\Program Files\\{}\\{}\\app.exe",
        "deepdir\\".repeat(500),
        "segment".repeat(500)
    ));
    let long_cmd: Arc<str> = Arc::from(format!("\"{}\" --arg={}", long_path, "z".repeat(4096)));

    let make_rows = |visible_cpu: u16, offscreen_cpu: u16| -> Vec<DetailsRowProjection> {
        (0..60u32)
            .map(|i| {
                let mut r = sample_row(3000 + i, 80_000 + u64::from(i), &format!("d-{i}.exe"));
                r.user_name = Some(Arc::from(r"WORKSTATION\Alice"));
                r.private_working_set_bytes = 128 * 1024 * 1024;
                r.working_set_bytes = 256 * 1024 * 1024;
                r.commit_bytes = 512 * 1024 * 1024;
                r.cpu_permille = if i == 0 {
                    visible_cpu
                } else if i == 55 {
                    offscreen_cpu
                } else {
                    5
                };
                if i == 0 {
                    r.exe_path = Some(Arc::clone(&long_path));
                    r.command_line = Some(Arc::clone(&long_cmd));
                }
                r
            })
            .collect()
    };

    let platform = store.read_with(&vcx, |s, _| s.snapshot().platform);
    let slice_v1 = Arc::new(DetailsTableSlice {
        revision: 10,
        rows: make_rows(15, 15),
        ..Default::default()
    });
    let ram = 16 * 1024 * 1024 * 1024;
    let notified_v1 = view.update(&mut vcx, |this, cx| {
        this.commit_snapshot(
            10,
            Arc::clone(&slice_v1),
            CapabilityFlags::empty(),
            platform,
            ram,
            cx,
        )
    });
    assert!(notified_v1, "initial commit must notify TableState");
    vcx.update(|window, cx| window.draw(cx).clear(cx));

    for (x_pos, expected_col) in [(120.0, Some(0)), (260.0, Some(1)), (350.0, Some(2))] {
        vcx.simulate_mouse_down(
            point(px(x_pos), px(100.)),
            MouseButton::Right,
            Modifiers::default(),
        );
        vcx.simulate_mouse_up(
            point(px(x_pos), px(100.)),
            MouseButton::Right,
            Modifiers::default(),
        );
        let detected = table.read_with(&vcx, |t, _| t.delegate().last_right_clicked_col);
        assert_eq!(
            detected, expected_col,
            "right-click at x={x_pos} should map to column {expected_col:?}"
        );
        vcx.simulate_keystrokes("escape");
        vcx.update(|window, cx| window.draw(cx).clear(cx));
    }

    let key0 = slice_v1.rows[0].key;
    let (path_preview, cmd_preview) = table.update(&mut vcx, |t, cx| {
        let previews = {
            let d = t.delegate_mut();
            d.selected_key = Some(key0);
            d.column_layout
                .apply_visible_slugs(&["pid", "cpu", "exe_path", "command_line"]);
            d.invalidate_viewport_fingerprint();
            let p = d
                .row_cache
                .preview_slot(key0, 0, &long_path, DEFAULT_CELL_PREVIEW_CHARS);
            let c = d
                .row_cache
                .preview_slot(key0, 1, &long_cmd, DEFAULT_CELL_PREVIEW_CHARS);
            (p, c)
        };
        t.refresh(cx);
        previews
    });
    assert!(path_preview.chars().count() <= DEFAULT_CELL_PREVIEW_CHARS + 1);
    assert!(path_preview.ends_with('…'));
    assert!(cmd_preview.chars().count() <= DEFAULT_CELL_PREVIEW_CHARS + 1);
    assert!(cmd_preview.ends_with('…'));

    let (summary_text, csv_rows) = table.read_with(&vcx, |t, _| {
        (
            t.delegate().selected_copy_summary().expect("row 0 summary"),
            t.delegate().export_visible_csv_payload().1,
        )
    });
    assert!(summary_text.contains(long_path.as_ref()));
    assert!(summary_text.contains(long_cmd.as_ref()));
    assert!(
        csv_rows[0]
            .iter()
            .any(|cell| cell.as_str() == long_path.as_ref())
    );
    assert!(
        csv_rows[0]
            .iter()
            .any(|cell| cell.as_str() == long_cmd.as_ref())
    );

    vcx.update(|window, cx| window.draw(cx).clear(cx));

    let slice_v2 = Arc::new(DetailsTableSlice {
        revision: 11,
        rows: make_rows(15, 777),
        ..Default::default()
    });
    let notified_v2 = view.update(&mut vcx, |this, cx| {
        this.commit_snapshot(11, slice_v2, CapabilityFlags::empty(), platform, ram, cx)
    });
    assert!(
        !notified_v2,
        "off-screen Details row change must skip TableState cx.notify()"
    );

    let slice_v3 = Arc::new(DetailsTableSlice {
        revision: 12,
        rows: make_rows(320, 777),
        ..Default::default()
    });
    let notified_v3 = view.update(&mut vcx, |this, cx| {
        this.commit_snapshot(12, slice_v3, CapabilityFlags::empty(), platform, ram, cx)
    });
    assert!(
        notified_v3,
        "visible Details row CPU change must notify TableState"
    );

    coordinator_for_shutdown.shutdown();
}

#[test]
fn details_locale_cache_and_working_set_delta_column_alignment() {
    let _locale_guard = i18n::locale_test_lock();
    let all_cols = [
        (DetailsColumn::Name, "details.col_name"),
        (DetailsColumn::Pid, "details.col_pid"),
        (DetailsColumn::Status, "details.col_status"),
        (DetailsColumn::UserName, "details.col_user"),
        (DetailsColumn::SessionId, "details.col_session_id"),
        (DetailsColumn::Cpu, "details.col_cpu"),
        (DetailsColumn::CpuTime, "details.col_cpu_time"),
        (DetailsColumn::Memory, "details.col_memory"),
        (DetailsColumn::WorkingSet, "details.col_working_set"),
        (
            DetailsColumn::WorkingSetDelta,
            "details.col_working_set_delta",
        ),
        (DetailsColumn::CommitSize, "details.col_commit"),
        (DetailsColumn::Priority, "details.col_priority"),
        (DetailsColumn::Handles, "details.col_handles"),
        (DetailsColumn::Threads, "details.col_threads"),
        (DetailsColumn::Platform, "details.col_platform"),
        (DetailsColumn::UacVirtualization, "details.col_uac"),
        (DetailsColumn::Elevated, "details.col_elevated"),
        (DetailsColumn::GpuMemory, "details.col_gpu_memory"),
        (DetailsColumn::Publisher, "details.col_publisher"),
        (DetailsColumn::Description, "details.col_description"),
        (DetailsColumn::ExePath, "details.col_exe_path"),
        (DetailsColumn::CommandLine, "details.col_command_line"),
    ];
    let cache = DetailsLocaleCache::build(PlatformProfile::WINDOWS);
    assert_eq!(cache.col_labels.len(), all_cols.len());
    for (idx, (col, key)) in all_cols.into_iter().enumerate() {
        assert_eq!(
            col as usize, idx,
            "DetailsColumn::{col:?} discriminant must match slot {idx}"
        );
        let cached = cache.column_label(col);
        let en = rust_i18n::t!(key, locale = "en-US");
        let zh = rust_i18n::t!(key, locale = "zh-CN");
        assert!(
            cached.as_ref() == en.as_ref() || cached.as_ref() == zh.as_ref(),
            "DetailsLocaleCache slot for {col:?} returned {cached:?}, expected {en:?} or {zh:?}"
        );
    }

    assert_eq!(
        rust_i18n::t!("details.col_memory", locale = "en-US").as_ref(),
        "Memory (private working set)"
    );
    assert_eq!(
        rust_i18n::t!("details.col_working_set_delta", locale = "en-US").as_ref(),
        "Working set delta (memory)"
    );
    assert_eq!(
        rust_i18n::t!("details.col_memory", locale = "zh-CN").as_ref(),
        "内存（专用工作集）"
    );
    assert_eq!(
        rust_i18n::t!("details.col_working_set_delta", locale = "zh-CN").as_ref(),
        "工作集增量（内存）"
    );

    let mut layout = TableColumnLayout::new(DetailsColumn::default_specs(CapabilityFlags::empty()));
    assert!(layout.is_visible(DetailsColumn::Memory));
    assert!(!layout.is_visible(DetailsColumn::WorkingSetDelta));
    assert!(
        DetailsColumn::selectable_order(CapabilityFlags::empty())
            .contains(&DetailsColumn::WorkingSetDelta)
    );
    assert!(DetailsColumn::WorkingSetDelta.right_aligned());
    assert_eq!(
        DetailsColumn::WorkingSetDelta.sort_column(),
        Some(DetailsSortColumn::WorkingSetDelta)
    );
    assert_eq!(
        default_sort_direction(DetailsSortColumn::WorkingSetDelta),
        SortDirection::Descending
    );

    assert!(layout.apply_visible_slugs(&[DetailsColumn::WorkingSetDelta.slug()]));
    assert!(layout.is_visible(DetailsColumn::WorkingSetDelta));
    let ram = 16 * 1024 * 1024 * 1024u64;
    let mut slice = DetailsTableSlice {
        revision: 1,
        rows: vec![sample_row(1000, 1, "worker.exe")],
        ..Default::default()
    };
    let visible_rows = 0..1;
    let visible_cols = 0..layout.visible_count();

    let fp_zero = compute_details_viewport_fingerprint(
        &slice,
        &layout,
        &visible_rows,
        &visible_cols,
        None,
        None,
        ram,
    );
    slice.rows[0].working_set_delta_bytes = 2 * 1024 * 1024;
    let fp_pos = compute_details_viewport_fingerprint(
        &slice,
        &layout,
        &visible_rows,
        &visible_cols,
        None,
        None,
        ram,
    );
    assert_ne!(fp_zero, fp_pos);

    slice.rows[0].working_set_delta_bytes += 64;
    let fp_pos_jitter = compute_details_viewport_fingerprint(
        &slice,
        &layout,
        &visible_rows,
        &visible_cols,
        None,
        None,
        ram,
    );
    assert_eq!(fp_pos, fp_pos_jitter);

    slice.rows[0].working_set_delta_bytes = -(2 * 1024 * 1024);
    let fp_neg = compute_details_viewport_fingerprint(
        &slice,
        &layout,
        &visible_rows,
        &visible_cols,
        None,
        None,
        ram,
    );
    assert_ne!(fp_pos, fp_neg);
}
