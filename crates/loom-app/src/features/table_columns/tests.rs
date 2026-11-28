//! Unit tests for `table_columns` layout, budgeting, coalescing, telemetry, export,
//! process controls, and keyboard shortcut classification.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{KeyDownEvent, Keystroke, Modifiers, px};
use loom_config::TableColumnSetting;
use loom_types::{CapabilityFlags, ExeIconRgba, ProcessStatusFlags, WebView2Role};
use loom_ui_kit::DEFAULT_CELL_PREVIEW_CHARS;

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestCol {
    Name,
    Pid,
    Cpu,
    Memory,
    Publisher,
}

impl ColumnKey for TestCol {
    fn slug(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Pid => "pid",
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::Publisher => "publisher",
        }
    }
}

fn sample_layout() -> TableColumnLayout<TestCol> {
    TableColumnLayout::new(vec![
        ColumnSpec::pinned(TestCol::Name, 200.0, 100.0, 400.0),
        ColumnSpec::hidden(TestCol::Pid, 76.0, 56.0, 160.0),
        ColumnSpec::new(TestCol::Cpu, 80.0, 50.0, 200.0),
        ColumnSpec::new(TestCol::Memory, 100.0, 60.0, 240.0),
        ColumnSpec::new(TestCol::Publisher, 140.0, 80.0, 320.0),
    ])
}

#[test]
fn apply_visible_widths_clamps_to_min_and_max_bounds() {
    let mut layout = sample_layout();

    assert!(layout.apply_visible_widths(&[px(240.), px(120.), px(100.), px(140.)]));
    assert_eq!(layout.visible_column(0).unwrap().width, 240.0);
    assert_eq!(layout.visible_column(1).unwrap().width, 120.0);

    // No-op when widths match current values.
    assert!(!layout.apply_visible_widths(&[px(240.), px(120.), px(100.), px(140.)]));

    // Clamps out-of-range widths to [min_width, max_width].
    assert!(layout.apply_visible_widths(&[px(240.), px(10.), px(999.), px(140.)]));
    assert_eq!(layout.visible_column(1).unwrap().width, 50.0);
    assert_eq!(layout.visible_column(2).unwrap().width, 240.0);
}

#[test]
fn move_visible_column_protects_pinned_columns_and_preserves_hidden() {
    let mut layout = sample_layout();
    // Visible: [Name (pinned), Cpu, Memory, Publisher]; Hidden: Pid.

    // Cannot move pinned Name or move another column before it.
    assert!(!layout.move_visible_column(0, 2));
    assert!(!layout.move_visible_column(1, 0));

    // Move Publisher (vis 3) to vis 1 (immediately after Name).
    assert!(layout.move_visible_column(3, 1));
    assert_eq!(
        layout
            .visible_columns()
            .iter()
            .map(|c| c.id)
            .collect::<Vec<_>>(),
        vec![
            TestCol::Name,
            TestCol::Publisher,
            TestCol::Cpu,
            TestCol::Memory
        ]
    );
    // Hidden Pid column remains intact in all_columns().
    assert!(!layout.is_visible(TestCol::Pid));
    assert_eq!(layout.all_columns().len(), 5);
}

#[test]
fn visibility_hiding_locked_guard_and_select_columns_apply() {
    let mut layout = sample_layout();

    // Pinned Name column cannot be hidden.
    assert!(!layout.hide_column(TestCol::Name));
    assert!(layout.is_visible(TestCol::Name));

    // Hide Publisher.
    assert!(layout.hide_column(TestCol::Publisher));
    assert!(!layout.is_visible(TestCol::Publisher));
    assert_eq!(layout.visible_count(), 3);

    // Re-applying visibility with Pid enabled and Cpu disabled keeps Name visible.
    assert!(layout.apply_visible_slugs(&["pid", "memory"]));
    assert_eq!(
        layout
            .visible_columns()
            .iter()
            .map(|c| c.id)
            .collect::<Vec<_>>(),
        vec![TestCol::Name, TestCol::Pid, TestCol::Memory]
    );

    // Empty selection still preserves the pinned Name column.
    assert!(layout.apply_visible_slugs(&[]));
    assert_eq!(
        layout
            .visible_columns()
            .iter()
            .map(|c| c.id)
            .collect::<Vec<_>>(),
        vec![TestCol::Name]
    );
}

#[test]
fn saved_snapshot_roundtrip_normalizes_pinned_columns_and_guards_corrupt_data() {
    let mut layout = sample_layout();
    assert!(layout.apply_visible_widths(&[px(260.), px(80.), px(100.), px(140.)]));
    assert!(layout.apply_visible_slugs(&["pid", "cpu", "publisher"]));
    assert!(layout.move_visible_column(3, 1)); // move Publisher after Name
    let mut saved = layout.snapshot();

    // Simulate legacy config where Name was moved non-leading, hidden, and oversized.
    saved.swap(0, 1);
    saved.push(TableColumnSetting {
        id: "legacy_removed_col".to_owned(),
        width: 999,
        visible: true,
    });
    if let Some(name_entry) = saved.iter_mut().find(|e| e.id == "name") {
        name_entry.width = 9999; // clamps to max_width (400.0)
        name_entry.visible = false; // locked Name stays visible
    }

    let mut restored = sample_layout();
    restored.apply_saved(&saved);

    assert_eq!(
        restored
            .visible_columns()
            .iter()
            .map(|c| c.id)
            .collect::<Vec<_>>(),
        vec![
            TestCol::Name,
            TestCol::Publisher,
            TestCol::Pid,
            TestCol::Cpu
        ]
    );
    assert_eq!(restored.visible_column(0).unwrap().width, 400.0);
    assert!(!restored.is_visible(TestCol::Memory));
}

#[test]
fn select_columns_draft_tracks_checkbox_toggles() {
    let mut draft = SelectColumnsDraft::new(vec![
        ColumnToggleItem {
            slug: "pid",
            label: "PID".to_owned(),
            visible: false,
        },
        ColumnToggleItem {
            slug: "cpu",
            label: "CPU".to_owned(),
            visible: true,
        },
    ]);
    assert_eq!(draft.selected_slugs(), vec!["cpu"]);
    draft.set_checked(0, true);
    draft.set_checked(1, false);
    assert_eq!(draft.selected_slugs(), vec!["pid"]);
}

#[test]
fn order_freeze_state_coordinates_hover_header_and_context_menu() {
    let mut state = OrderFreezeState::default();
    assert!(!state.is_frozen());

    // Hovering table body freezes order.
    assert_eq!(state.set_table_hovered(true), Some(true));
    assert!(state.is_frozen());
    // Duplicate hover emits no transition.
    assert_eq!(state.set_table_hovered(true), None);

    // Moving onto the column header temporarily unfreezes so clicking sort works immediately.
    assert_eq!(state.set_header_hovered(true), Some(false));
    assert!(!state.is_frozen());
    assert_eq!(state.set_header_hovered(false), Some(true));
    assert!(state.is_frozen());

    // Opening context menu keeps row order frozen even after mouse leaves table.
    assert_eq!(state.set_context_menu_open(true), None);
    assert_eq!(state.set_table_hovered(false), None);
    assert!(state.is_frozen());

    // Dismissing context menu while mouse is outside unfreezes immediately.
    assert_eq!(state.set_context_menu_open(false), Some(false));
    assert!(!state.is_frozen());
}

#[test]
fn type_to_search_state_handles_direct_typing_and_cjk_ime_cancel_commit() {
    let mut search = TypeToSearchState::default();

    // Direct ASCII typing appends without losing the first keystroke.
    assert_eq!(search.replace_text(None, "c", ""), Some("c".to_owned()));
    assert_eq!(search.replace_text(None, "h", "c"), Some("ch".to_owned()));

    // Start CJK IME composition on top of existing query "ch":
    // committed query stays untouched while composing.
    search.replace_and_mark(None, "wei", "ch");
    assert!(search.is_composing());
    assert_eq!(search.preedit_text(), Some("wei"));
    assert_eq!(search.marked_utf16_range(), Some(2..5));

    // Cancelling composition (Escape or empty Windows WM_IME_COMPOSITION commit)
    // leaves previous committed query "ch" intact.
    assert_eq!(search.replace_text(None, "", "ch"), None);
    assert!(!search.is_composing());
    assert_eq!(search.preedit_text(), None);
    assert_eq!(search.text_for_utf16_range(0..2).1, "ch");

    // Second CJK composition -> commit "微信" appends cleanly to "ch".
    search.replace_and_mark(None, "wx", "ch");
    assert_eq!(search.preedit_text(), Some("wx"));
    assert_eq!(
        search.replace_text(None, "微信", "ch"),
        Some("ch微信".to_owned())
    );
    assert!(!search.is_composing());
    assert_eq!(search.utf16_len(), 4);

    // Backspace pops one full Unicode scalar ("信").
    assert_eq!(search.handle_backspace("ch微信"), Some("ch微".to_owned()));

    // Escape cancels composition if active, otherwise clears query.
    search.replace_and_mark(None, "pin", "ch微");
    assert_eq!(search.handle_escape("ch微"), None);
    assert!(!search.is_composing());
    assert_eq!(search.handle_escape("ch微"), Some(String::new()));
}

#[test]
fn table_key_down_classifier_handles_copy_delete_expand_collapse_and_search_keys() {
    let mk = |key: &str, modifiers: Modifiers| KeyDownEvent {
        keystroke: Keystroke {
            modifiers,
            key: key.to_owned(),
            key_char: None,
        },
        is_held: false,
        prefer_character_input: false,
    };

    assert_eq!(
        classify_table_key_down(&mk("c", Modifiers::control())),
        Some(TableKeyAction::CopyShort)
    );
    assert_eq!(
        classify_table_key_down(&mk(
            "c",
            Modifiers {
                control: true,
                shift: true,
                ..Modifiers::none()
            }
        )),
        Some(TableKeyAction::CopySummary)
    );
    assert_eq!(
        classify_table_key_down(&mk("delete", Modifiers::none())),
        Some(TableKeyAction::EndTask)
    );
    assert_eq!(
        classify_table_key_down(&mk("right", Modifiers::none())),
        Some(TableKeyAction::ExpandRow)
    );
    assert_eq!(
        classify_table_key_down(&mk("left", Modifiers::none())),
        Some(TableKeyAction::CollapseRow)
    );
    assert_eq!(
        classify_table_key_down(&mk("backspace", Modifiers::none())),
        Some(TableKeyAction::BackspaceSearch)
    );
    assert_eq!(
        classify_table_key_down(&mk("escape", Modifiers::none())),
        Some(TableKeyAction::EscapeSearch)
    );
    // Modified arrow keys are ignored by tree expand/collapse.
    assert_eq!(
        classify_table_key_down(&mk("right", Modifiers::control())),
        None
    );
}

#[test]
fn csv_export_filename_and_formula_safe_write_output() {
    assert_eq!(
        format_csv_export_filename("loom-processes", 0),
        "loom-processes-19700101-000000.csv"
    );
    // 2026-01-02 03:04:05 UTC = 1767323045
    assert_eq!(
        format_csv_export_filename("loom-details", 1_767_323_045),
        "loom-details-20260102-030405.csv"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let headers = vec![
        "Name".to_owned(),
        "PID".to_owned(),
        "Command line".to_owned(),
    ];
    let rows = vec![vec![
        "app.exe".to_owned(),
        "1234".to_owned(),
        "=CMD|'/C calc'!A0".to_owned(),
    ]];
    let written =
        write_csv_export_file(dir.path(), "export.csv", &headers, &rows).expect("write csv");
    let content = std::fs::read_to_string(&written).expect("read csv");
    assert!(content.starts_with(loom_ui_kit::CSV_UTF8_BOM));
    assert!(content.contains("Name,PID,Command line\r\n"));
    assert!(content.contains("app.exe,1234,'=CMD|'/C calc'!A0\r\n"));
}

#[test]
fn suspend_resume_explorer_and_leak_target_gating() {
    let with_cap = CapabilityFlags::SUSPEND_RESUME;
    let no_cap = CapabilityFlags::empty();

    assert!(can_suspend_or_resume_process(
        4242,
        "notepad.exe",
        ProcessStatusFlags::empty(),
        with_cap
    ));
    assert!(!can_suspend_or_resume_process(
        4242,
        "notepad.exe",
        ProcessStatusFlags::empty(),
        no_cap
    ));
    assert!(!can_suspend_or_resume_process(
        0,
        "System Idle Process",
        ProcessStatusFlags::SYSTEM_IDLE,
        with_cap
    ));
    assert!(!can_suspend_or_resume_process(
        std::process::id(),
        "loom-app.exe",
        ProcessStatusFlags::empty(),
        with_cap
    ));
    assert!(!can_suspend_or_resume_process(
        512,
        "DWM.EXE",
        ProcessStatusFlags::empty(),
        with_cap
    ));
    assert!(!can_suspend_or_resume_process(
        600,
        "csrss.exe",
        ProcessStatusFlags::CRITICAL,
        with_cap
    ));

    assert!(is_explorer_image("explorer.exe"));
    assert!(is_explorer_image("EXPLORER.EXE"));
    assert!(!is_explorer_image("iexplore.exe"));

    assert_eq!(
        leak_ignore_target(Some(r" C:\Apps\leak.exe "), "leak.exe"),
        Some(r"C:\Apps\leak.exe".to_owned())
    );
    assert_eq!(
        leak_ignore_target(None, "leak.exe"),
        Some("leak.exe".to_owned())
    );
    assert_eq!(leak_ignore_target(Some("  "), "   "), None);
}

#[test]
fn processes_and_details_ux_locale_keys_exist_in_en_and_zh() {
    let keys = [
        "proc_ux_sort_paused_tooltip",
        "proc_ux_hide_system_tooltip",
        "proc_ux_copy_submenu",
        "proc_ux_copy_name",
        "proc_ux_copy_pid",
        "proc_ux_copy_path",
        "proc_ux_copy_cmdline",
        "proc_ux_copy_summary",
        "proc_ux_menu_suspend",
        "proc_ux_menu_resume",
        "proc_ux_menu_restart_explorer",
        "proc_ux_ime_searching",
        "csv_export_button",
        "csv_export_success_title",
        "csv_export_error_title",
        "csv_export_open_location",
        "leak_warning_ignore_menu",
        "leak_warning_badge_prefix",
        "webview2_role_browser",
        "webview2_role_renderer",
        "webview2_role_gpu",
        "webview2_role_network",
        "webview2_role_storage",
        "webview2_role_audio",
        "webview2_role_utility",
        "webview2_role_crashpad",
    ];
    for key in keys {
        let en = rust_i18n::t!(key, locale = "en-US");
        let zh = rust_i18n::t!(key, locale = "zh-CN");
        assert!(!en.is_empty() && en != key, "missing en-US for {key}");
        assert!(!zh.is_empty() && zh != key, "missing zh-CN for {key}");
    }
    let wv2_labels = WebView2RoleLabels::build();
    for role in [
        WebView2Role::Browser,
        WebView2Role::Renderer,
        WebView2Role::Gpu,
        WebView2Role::Network,
        WebView2Role::Storage,
        WebView2Role::Audio,
        WebView2Role::Utility,
        WebView2Role::Crashpad,
    ] {
        assert_eq!(wv2_labels.get(role).as_ref(), webview2_role_label(role));
    }
    assert_eq!(
        rust_i18n::t!(
            "proc_ux_copy_column",
            locale = "en-US",
            column = "Publisher"
        ),
        "Copy \"Publisher\""
    );
    assert_eq!(
        rust_i18n::t!("proc_ux_copy_column", locale = "zh-CN", column = "发布者"),
        "复制“发布者”"
    );
}

#[test]
fn column_index_at_x_handles_pinned_scroll_bounds_and_layout_mutations() {
    let mut layout = sample_layout();
    // Visible initially: [Name (pinned, 200), Cpu (80), Memory (100), Publisher (140)].
    // Total width = 520.

    // Negative or non-finite coordinates never match.
    assert_eq!(layout.column_index_at_x(-1.0, 0.0), None);
    assert_eq!(layout.column_index_at_x(f32::NAN, 0.0), None);
    assert_eq!(layout.column_index_at_x(f32::INFINITY, 0.0), None);

    // Left-pinned Name column [0, 200) hits index 0 regardless of horizontal scroll.
    assert_eq!(layout.column_index_at_x(0.0, 0.0), Some(0));
    assert_eq!(layout.column_index_at_x(199.5, 0.0), Some(0));
    assert_eq!(layout.column_index_at_x(50.0, 120.0), Some(0));
    assert_eq!(layout.column_index_at_x(199.9, 250.0), Some(0));

    // Scrollable columns at scroll_x = 0:
    // Cpu: [200, 280), Memory: [280, 380), Publisher: [380, 520), past end: >= 520.
    assert_eq!(layout.column_index_at_x(200.0, 0.0), Some(1));
    assert_eq!(layout.column_index_at_x(279.9, 0.0), Some(1));
    assert_eq!(layout.column_index_at_x(280.0, 0.0), Some(2));
    assert_eq!(layout.column_index_at_x(379.9, 0.0), Some(2));
    assert_eq!(layout.column_index_at_x(380.0, 0.0), Some(3));
    assert_eq!(layout.column_index_at_x(519.9, 0.0), Some(3));
    assert_eq!(layout.column_index_at_x(520.0, 0.0), None);
    assert_eq!(layout.column_index_at_x(900.0, 0.0), None);

    // After scrolling right by 80 px, Cpu is scrolled under Name, so local_x = 210 hits Memory (2).
    assert_eq!(layout.column_index_at_x(210.0, 80.0), Some(2));
    // Scrolling right by 180 px brings Publisher to local_x = 200.
    assert_eq!(layout.column_index_at_x(200.0, 180.0), Some(3));
    // Past Publisher when scrolled right by 180 px: content_x = (340 - 200) + 180 = 320 -> None.
    assert_eq!(layout.column_index_at_x(340.0, 180.0), None);

    // After hiding Cpu, reordering Publisher before Memory, and resizing Name to 150 px:
    assert!(layout.hide_column(TestCol::Cpu));
    assert!(layout.move_visible_column(2, 1));
    assert!(layout.apply_visible_widths(&[px(150.0), px(140.0), px(100.0)]));
    // Pinned Name is now [0, 150), Publisher [150, 290), Memory [290, 390).
    assert_eq!(layout.column_index_at_x(149.0, 0.0), Some(0));
    assert_eq!(layout.column_index_at_x(150.0, 0.0), Some(1));
    assert_eq!(layout.column_index_at_x(290.0, 0.0), Some(2));
    assert_eq!(layout.column_index_at_x(390.0, 0.0), None);
    // With scroll_x = 140, local_x = 155 lands on Memory (index 2).
    assert_eq!(layout.column_index_at_x(155.0, 140.0), Some(2));
}

#[test]
fn visible_cols_mask_always_includes_pinned_and_handles_empty_range() {
    let layout = sample_layout();
    assert_eq!(layout.pinned_count(), 1);
    assert_eq!(layout.visible_cols_mask(&(0..0)), 0b1111);
    assert_eq!(layout.visible_cols_mask(&(0..1)), 0b0011);
    assert_eq!(layout.visible_cols_mask(&(1..3)), 0b1101);
}

#[test]
fn scroll_burst_coalescer_defers_during_scroll_flushes_on_quiet_cap_and_user_action() {
    let quiet = Duration::from_millis(150);
    let cap = Duration::from_millis(500);
    let mut coalescer = ScrollBurstCoalescer::with_windows(quiet, cap);
    let t0 = Instant::now();

    // 1. No prior scroll -> apply immediately.
    assert!(!coalescer.should_defer_snapshot(t0, false));
    assert_eq!(coalescer.remaining_quiet_delay(t0), None);
    assert!(!coalescer.take_pending());

    // 2. During active scroll -> defer and report remaining quiet delay.
    coalescer.on_scroll(t0);
    let t_50 = t0 + Duration::from_millis(50);
    assert!(coalescer.should_defer_snapshot(t_50, false));
    assert_eq!(
        coalescer.remaining_quiet_delay(t_50),
        Some(Duration::from_millis(100))
    );

    // 3. Quiet window elapsed (>= 150ms since last scroll) -> flush immediately.
    let t_150 = t0 + Duration::from_millis(150);
    assert_eq!(coalescer.remaining_quiet_delay(t_150), Some(Duration::ZERO));
    assert!(!coalescer.should_defer_snapshot(t_150, false));
    assert!(!coalescer.take_pending());

    // 4. Continuous scroll past max_defer_window (500ms) -> cap reached, force flush!
    let base = t0 + Duration::from_secs(2);
    coalescer.on_scroll(base);
    assert!(coalescer.should_defer_snapshot(base + Duration::from_millis(10), false));
    for step in 1..=4 {
        let now = base + Duration::from_millis(step * 100);
        coalescer.on_scroll(now);
        assert!(
            coalescer.should_defer_snapshot(now + Duration::from_millis(10), false),
            "step {step} should still defer before 500ms cap"
        );
    }
    let t_near_cap = base + Duration::from_millis(500);
    coalescer.on_scroll(t_near_cap);
    assert_eq!(
        coalescer.remaining_quiet_delay(t_near_cap),
        Some(Duration::from_millis(10))
    );
    let t_cap = base + Duration::from_millis(510);
    assert!(!coalescer.should_defer_snapshot(t_cap, false));
    assert!(!coalescer.take_pending());

    // 5. User-initiated snapshot always bypasses deferral immediately, even mid-scroll.
    let t_user = base + Duration::from_secs(2);
    coalescer.on_scroll(t_user);
    assert!(coalescer.should_defer_snapshot(t_user + Duration::from_millis(20), false));
    assert!(!coalescer.should_defer_snapshot(t_user + Duration::from_millis(30), true));
    assert!(!coalescer.take_pending());
}

#[test]
fn should_attach_cell_tooltip_only_when_text_can_overflow_column_width() {
    assert!(!should_attach_cell_tooltip(r"C:\app.exe", 300.0));
    assert!(should_attach_cell_tooltip(
        r#""C:\Program Files\Example\app.exe" --profile-directory=Default --enable-features=Fast"#,
        300.0
    ));
    assert!(should_attach_cell_tooltip(
        "C:\\用户\\测试目录\\应用程序主程序.exe",
        180.0
    ));
    let huge = "a".repeat(DEFAULT_CELL_PREVIEW_CHARS + 10);
    assert!(should_attach_cell_tooltip(&huge, 5000.0));
}

#[test]
fn icon_render_cache_enforces_per_frame_upload_budget_and_zero_cost_hits() {
    let mut cache = IconRenderCache::with_limits(16, 2);
    let mk_icon = |seed: u8| ExeIconRgba {
        width: 2,
        height: 2,
        rgba: Arc::from([seed; 16]),
    };

    let icon1 = mk_icon(1);
    let icon2 = mk_icon(2);
    let icon3 = mk_icon(3);

    cache.begin_frame();
    assert!(matches!(
        cache.get_or_insert_budgeted(&icon1),
        BudgetedIconLookup::Uploaded(_)
    ));
    assert!(matches!(
        cache.get_or_insert_budgeted(&icon1),
        BudgetedIconLookup::Hit(_)
    ));
    assert!(matches!(
        cache.get_or_insert_budgeted(&icon2),
        BudgetedIconLookup::Uploaded(_)
    ));
    assert!(!cache.has_deferred_uploads());
    assert!(matches!(
        cache.get_or_insert_budgeted(&icon3),
        BudgetedIconLookup::Deferred
    ));
    assert!(cache.has_deferred_uploads());
    assert_eq!(cache.len(), 2);

    cache.begin_frame();
    assert!(!cache.has_deferred_uploads());
    assert!(matches!(
        cache.get_or_insert_budgeted(&icon1),
        BudgetedIconLookup::Hit(_)
    ));
    assert!(matches!(
        cache.get_or_insert_budgeted(&icon2),
        BudgetedIconLookup::Hit(_)
    ));
    assert!(matches!(
        cache.get_or_insert_budgeted(&icon3),
        BudgetedIconLookup::Uploaded(_)
    ));
    assert!(!cache.has_deferred_uploads());
    assert_eq!(cache.len(), 3);
}

#[test]
fn list_telemetry_registry_records_counters_and_formats_privacy_safe_line() {
    let reg = ListTelemetryRegistry::new();
    assert!(reg.format_diagnostic_line().is_none());

    reg.record_commit(ListViewTelemetryKind::Processes, 180, true);
    reg.record_commit(ListViewTelemetryKind::Processes, 95, false);
    reg.record_scroll_defer(ListViewTelemetryKind::Processes);
    reg.record_icon_upload(ListViewTelemetryKind::Processes);
    reg.record_icon_defer(ListViewTelemetryKind::Processes);
    reg.record_render_td(ListViewTelemetryKind::Processes);

    let snap = reg.snapshot(ListViewTelemetryKind::Processes);
    assert_eq!(snap.commits, 2);
    assert_eq!(snap.viewport_fp_skips, 1);
    assert_eq!(snap.scroll_defers, 1);
    assert_eq!(snap.icon_uploads, 1);
    assert_eq!(snap.icon_defers, 1);
    assert_eq!(snap.last_commit_us, 95);
    assert_eq!(snap.max_commit_us, 180);
    assert_eq!(snap.render_td_calls, 1);

    let line = reg
        .format_diagnostic_line()
        .expect("non-zero commits produce telemetry line");
    assert!(line.contains("Processes(commits=2, fp_skips=1, scroll_defers=1, icon_uploads=1, icon_defers=1, max_commit=180us)"));
    assert!(!line.contains("Details("));
}
