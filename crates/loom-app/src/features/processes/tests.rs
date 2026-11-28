//! UI integration and pure unit tests for [`super::ProcessesView`] and its table delegate.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::table::{Column, TableDelegate, TableEvent, TableState};
use gpui_kit::{
    App, AppContext as _, Context, Entity, IntoElement, Modifiers, MouseButton, ParentElement as _,
    Render, SharedString, Styled as _, TestAppContext, VisualTestContext, Window, div, point, px,
    size,
};
use loom_types::{ProcessCategory, ProcessKey, ProcessRowKind};

use super::ProcessesView;
use super::columns::{
    ProcessColumn, build_process_row_div, compute_processes_viewport_fingerprint,
};
use crate::features::table_columns::{
    ColumnKey as _, DataTablePane, ListViewTelemetryKind, TableColumnLayout, list_telemetry,
};

struct LiveMetricDelegate {
    layout: TableColumnLayout<ProcessColumn>,
    rows: Vec<(SharedString, u16)>,
    sorts: Rc<Cell<usize>>,
}

impl TableDelegate for LiveMetricDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.layout.visible_count()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        let spec = self.layout.visible_column(col_ix).unwrap();
        let mut col = Column::new(spec.id.slug(), spec.id.slug())
            .width(px(spec.width))
            .min_width(px(spec.min_width))
            .max_width(px(spec.max_width))
            .selectable(false)
            .sortable();
        if spec.locked {
            col = col.fixed_left().movable(false);
        }
        col
    }

    fn perform_sort(
        &mut self,
        _col_ix: usize,
        _sort: gpui_kit::component::table::ColumnSort,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) {
        self.sorts.set(self.sorts.get() + 1);
    }

    fn move_column(
        &mut self,
        col_ix: usize,
        to_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) {
        self.layout.move_visible_column(col_ix, to_ix);
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        let section_bg = cx.theme().muted.opacity(0.6);
        let kind = if row_ix == 0 {
            ProcessRowKind::SectionHeader {
                category: ProcessCategory::App,
                count: 1,
            }
        } else {
            ProcessRowKind::Process
        };
        let key = ProcessKey {
            pid: row_ix as u32,
            create_time_100ns: 1,
        };
        build_process_row_div(Some((kind, key)), row_ix, section_bg)
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let (name, cpu) = &self.rows[row_ix];
        if col_ix == 0 {
            div().child(name.clone())
        } else {
            div().child(format!("{}.{}%", cpu / 10, cpu % 10))
        }
    }
}

struct SplitToolbarHarness {
    table: Entity<TableState<LiveMetricDelegate>>,
    table_pane: Entity<DataTablePane<LiveMetricDelegate>>,
    outer_renders: Rc<Cell<usize>>,
    _sub: gpui_kit::Subscription,
}

impl Render for SplitToolbarHarness {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.outer_renders.set(self.outer_renders.get() + 1);
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(div().h(px(40.)).child("Static Toolbar"))
            .child(div().flex_1().min_h_0().child(self.table_pane.clone()))
    }
}

#[gpui_kit::test]
fn split_datatable_updates_rows_without_rerendering_outer_toolbar_and_supports_selection(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let outer_renders = Rc::new(Cell::new(0));
    let sorts = Rc::new(Cell::new(0));
    let resized_widths = Rc::new(Cell::new(0));

    let outer_renders_init = outer_renders.clone();
    let sorts_init = sorts.clone();
    let resized_init = resized_widths.clone();
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), move |window, cx| {
            let harness = cx.new(|cx| {
                let table = cx.new(|cx| {
                    let delegate = LiveMetricDelegate {
                        layout: TableColumnLayout::new(ProcessColumn::default_specs()),
                        rows: (0..200)
                            .map(|i| (SharedString::from(format!("proc-{i}.exe")), i as u16))
                            .collect(),
                        sorts: sorts_init,
                    };
                    TableState::new(delegate, window, cx)
                        .row_selectable(true)
                        .col_selectable(false)
                        .col_resizable(true)
                        .col_movable(true)
                        .sortable(true)
                });
                let sub = cx.subscribe(&table, move |_, table, event: &TableEvent, cx| {
                    if let TableEvent::ColumnWidthsChanged(widths) = event {
                        table.update(cx, |table, _| {
                            if table.delegate_mut().layout.apply_visible_widths(widths) {
                                resized_init.set(resized_init.get() + 1);
                            }
                        });
                    }
                });
                let table_pane = cx.new(|_| {
                    DataTablePane::new(
                        table.clone(),
                        gpui_kit::component::Size::Medium,
                        px(32.),
                        false,
                        "Processes",
                    )
                });
                SplitToolbarHarness {
                    table,
                    table_pane,
                    outer_renders: outer_renders_init,
                    _sub: sub,
                }
            });
            cx.new(|cx| gpui_kit::base::Root::new(harness, window, cx))
        })
        .unwrap()
    });

    cx.simulate_window_resize(window.into(), size(px(1120.), px(720.)));
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    vcx.update(|window, cx| window.draw(cx).clear(cx));

    let renders_after_init = outer_renders.get();
    assert!(renders_after_init >= 1);

    let table = window
        .read_with(&vcx, |root, cx| {
            root.view()
                .clone()
                .downcast::<SplitToolbarHarness>()
                .expect("root wraps SplitToolbarHarness")
                .read(cx)
                .table
                .clone()
        })
        .unwrap();

    for tick in 1..=10u16 {
        table.update(&mut vcx, |table, cx| {
            table.delegate_mut().rows[0].1 = tick * 10;
            cx.notify();
        });
        vcx.update(|window, cx| window.draw(cx).clear(cx));
    }

    assert_eq!(
        outer_renders.get(),
        renders_after_init,
        "live table ticks must not re-render the static outer toolbar"
    );

    vcx.simulate_click(point(px(100.), px(90.)), Modifiers::default());
    vcx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(
        table.read_with(&vcx, |t, _| t.selected_row().is_some()),
        "clicking a data row must select it in DataTable"
    );

    vcx.simulate_mouse_down(
        point(px(100.), px(90.)),
        MouseButton::Right,
        Modifiers::default(),
    );
    vcx.simulate_mouse_up(
        point(px(100.), px(90.)),
        MouseButton::Right,
        Modifiers::default(),
    );
    vcx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(
        table.read_with(&vcx, |t, _| t.right_clicked_row().is_some()),
        "right-clicking a data row must record right_clicked_row"
    );
}

#[gpui_kit::test]
fn processes_view_hover_freeze_ime_search_copy_export_and_diagnostics(cx: &mut TestAppContext) {
    use std::sync::Arc;

    use gpui_kit::EntityInputHandler as _;
    use loom_config::{AppSettings, ConfigStore};
    use loom_engine::EngineCoordinator;
    use loom_pal::{NativeBackend, PlatformBackend as _};
    use loom_types::{
        LeakKind, LeakWarning, ProcessRowProjection, ProcessStatusFlags, ProcessTableSlice,
        WebView2Role,
    };

    use crate::bridge::UiStore;
    use crate::features::table_columns::{
        dispatch_hide_system_processes, dispatch_ignore_leak_path,
    };
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
            let view = cx.new(|cx| ProcessesView::new(store_for_window, shell, window, cx));
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
                .downcast::<ProcessesView>()
                .expect("root wraps ProcessesView")
        })
        .expect("read root");

    let key_wv2 = ProcessKey::new(4242, 100_000);
    let key_exp = ProcessKey::new(5151, 200_000);
    let synthetic_slice = Arc::new(ProcessTableSlice {
        revision: 999,
        rows: vec![
            ProcessRowProjection {
                kind: ProcessRowKind::SectionHeader {
                    category: ProcessCategory::App,
                    count: 2,
                },
                key: ProcessKey::default(),
                depth: 0,
                has_children: false,
                is_expanded: false,
                child_count: 0,
                name: Arc::from(""),
                display_name: Arc::from(""),
                window_title: None,
                publisher: None,
                exe_path: None,
                command_line: None,
                icon: None,
                category: ProcessCategory::App,
                status: ProcessStatusFlags::empty(),
                cpu_permille: 0,
                memory_bytes: 0,
                memory_permille: 0,
                disk_bytes_per_sec: 0,
                disk_permille: 0,
                net_bytes_per_sec: 0,
                net_permille: 0,
                gpu_util_permille: 0,
                gpu_dedicated_bytes: 0,
                parent_key: None,
                leak_warning: None,
                webview2_role: None,
            },
            ProcessRowProjection {
                kind: ProcessRowKind::Process,
                key: key_wv2,
                depth: 0,
                has_children: false,
                is_expanded: false,
                child_count: 0,
                name: Arc::from("msedgewebview2.exe"),
                display_name: Arc::from("Microsoft Edge WebView2"),
                window_title: None,
                publisher: Some(Arc::from("Microsoft Corporation")),
                exe_path: Some(Arc::from(r"C:\WebView2\msedgewebview2.exe")),
                command_line: Some(Arc::from("=formula_cmd --type=renderer")),
                icon: None,
                category: ProcessCategory::App,
                status: ProcessStatusFlags::EFFICIENCY_MODE,
                cpu_permille: 145,
                memory_bytes: 256 * 1024 * 1024,
                memory_permille: 16,
                disk_bytes_per_sec: 0,
                disk_permille: 0,
                net_bytes_per_sec: 0,
                net_permille: 0,
                gpu_util_permille: 20,
                gpu_dedicated_bytes: 0,
                parent_key: None,
                leak_warning: Some(LeakWarning {
                    kind: LeakKind::Handles,
                    summary: Arc::from("Handles grew monotonically from 1000 to 1600"),
                    current_value: 1600,
                    baseline_value: 1000,
                }),
                webview2_role: Some(WebView2Role::Renderer),
            },
            ProcessRowProjection {
                kind: ProcessRowKind::Process,
                key: key_exp,
                depth: 0,
                has_children: false,
                is_expanded: false,
                child_count: 0,
                name: Arc::from("explorer.exe"),
                display_name: Arc::from("Windows Explorer"),
                window_title: None,
                publisher: Some(Arc::from("Microsoft Corporation")),
                exe_path: Some(Arc::from(r"C:\Windows\explorer.exe")),
                command_line: None,
                icon: None,
                category: ProcessCategory::System,
                status: ProcessStatusFlags::empty(),
                cpu_permille: 10,
                memory_bytes: 64 * 1024 * 1024,
                memory_permille: 4,
                disk_bytes_per_sec: 0,
                disk_permille: 0,
                net_bytes_per_sec: 0,
                net_permille: 0,
                gpu_util_permille: 0,
                gpu_dedicated_bytes: 0,
                parent_key: None,
                leak_warning: None,
                webview2_role: None,
            },
        ],
        ..Default::default()
    });

    let table = view.read_with(&vcx, |v, _| v.table.clone());
    table.update(&mut vcx, |table, cx| {
        let d = table.delegate_mut();
        d.slice = Arc::clone(&synthetic_slice);
        d.selected_key = Some(key_wv2);
        cx.notify();
    });
    vcx.update(|window, cx| window.draw(cx).clear(cx));

    // 1. Pause-sort-on-hover transitions & amber indicator draw cleanly.
    table.update(&mut vcx, |table, cx| {
        table.delegate_mut().set_table_hovered(true, cx);
        assert!(table.delegate().order_freeze.is_frozen());
    });
    vcx.update(|window, cx| window.draw(cx).clear(cx));
    table.update(&mut vcx, |table, cx| {
        table.delegate_mut().set_context_menu_open(true, cx);
        table.delegate_mut().set_table_hovered(false, cx);
        assert!(table.delegate().order_freeze.is_frozen());
        table.delegate_mut().set_context_menu_open(false, cx);
        assert!(!table.delegate().order_freeze.is_frozen());
    });

    // 2. Direct type-to-search with CJK IME composition (cancel preserves previous query, commit updates UiStore).
    view.update_in(&mut vcx, |this, window, cx| {
        this.replace_text_in_range(None, "edge", window, cx);
    });
    assert_eq!(
        store.read_with(&vcx, |s, _| s.search_query().to_owned()),
        "edge"
    );
    view.update_in(&mut vcx, |this, window, cx| {
        this.replace_and_mark_text_in_range(None, "zhong", None, window, cx);
        assert!(this.type_to_search.is_composing());
        this.replace_text_in_range(None, "", window, cx);
        assert!(!this.type_to_search.is_composing());
    });
    assert_eq!(
        store.read_with(&vcx, |s, _| s.search_query().to_owned()),
        "edge"
    );
    view.update_in(&mut vcx, |this, window, cx| {
        this.replace_and_mark_text_in_range(None, "wen", None, window, cx);
        this.replace_text_in_range(None, "文", window, cx);
    });
    assert_eq!(
        store.read_with(&vcx, |s, _| s.search_query().to_owned()),
        "edge文"
    );

    // 3. Smart copy (short + structured summary) writes to clipboard.
    view.update(&mut vcx, |this, cx| {
        assert!(this.copy_selected_short(cx));
    });
    let short_clip = vcx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("short clipboard text");
    assert_eq!(short_clip, "msedgewebview2.exe (4242)");

    view.update(&mut vcx, |this, cx| {
        assert!(this.copy_selected_summary(cx));
    });
    let summary_clip = vcx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("summary clipboard text");
    assert!(summary_clip.contains("PID: 4242"));
    assert!(summary_clip.contains("Publisher: Microsoft Corporation"));

    // 4. CSV export payload excludes SectionHeader rows and captures visible columns.
    let (headers, rows) = table.read_with(&vcx, |t, _| t.delegate().export_visible_csv_payload());
    assert!(!headers.is_empty());
    assert_eq!(
        rows.len(),
        2,
        "SectionHeader rows must be excluded from CSV export"
    );
    assert_eq!(rows[0].len(), headers.len());
    assert_eq!(rows[0][0], "Microsoft Edge WebView2");

    // 5. Hide Windows system processes toggle & ignore leak path persist to settings.toml.
    vcx.update(|_, cx| {
        dispatch_hide_system_processes(&store, true, cx);
        dispatch_ignore_leak_path(&store, r"C:\WebView2\msedgewebview2.exe".to_owned(), cx);
        dispatch_ignore_leak_path(&store, r"c:\webview2\msedgewebview2.exe".to_owned(), cx);
    });
    assert!(store.read_with(&vcx, |s, _| s.settings().hide_windows_system_processes));
    assert_eq!(
        store.read_with(&vcx, |s, _| s.settings().leak_ignored_paths.clone()),
        vec![r"C:\WebView2\msedgewebview2.exe".to_owned()]
    );

    coordinator_for_shutdown.shutdown();
}

#[test]
fn processes_viewport_fingerprint_detects_visible_changes_and_ignores_offscreen_and_jitter() {
    use std::sync::Arc;

    use loom_types::{
        ProcessCategory, ProcessKey, ProcessRowKind, ProcessRowProjection, ProcessStatusFlags,
        ProcessTableSlice, ResourceDisplayMode,
    };

    let make_row = |i: u32| ProcessRowProjection {
        kind: ProcessRowKind::Process,
        key: ProcessKey::new(1000 + i, 50_000 + u64::from(i)),
        depth: 0,
        has_children: false,
        is_expanded: false,
        child_count: 0,
        name: Arc::from(format!("worker-{i}.exe")),
        display_name: Arc::from(format!("Worker {i}")),
        window_title: None,
        publisher: Some(Arc::from("Example Corp")),
        exe_path: None,
        command_line: None,
        icon: None,
        category: ProcessCategory::App,
        status: ProcessStatusFlags::empty(),
        cpu_permille: 25,
        memory_bytes: 128 * 1024 * 1024,
        memory_permille: 8,
        disk_bytes_per_sec: 0,
        disk_permille: 0,
        net_bytes_per_sec: 0,
        net_permille: 0,
        gpu_util_permille: 0,
        gpu_dedicated_bytes: 0,
        parent_key: None,
        leak_warning: None,
        webview2_role: None,
    };

    let layout = TableColumnLayout::new(ProcessColumn::default_specs());
    let visible_rows = 0..10;
    let visible_cols = 0..6;

    let mut slice = ProcessTableSlice {
        revision: 1,
        rows: (0..60).map(make_row).collect(),
        ..Default::default()
    };

    let fp_base = compute_processes_viewport_fingerprint(
        &slice,
        &layout,
        &visible_rows,
        &visible_cols,
        None,
        ResourceDisplayMode::Values,
        ResourceDisplayMode::Values,
        ResourceDisplayMode::Values,
    );

    assert_eq!(
        fp_base,
        compute_processes_viewport_fingerprint(
            &slice,
            &layout,
            &visible_rows,
            &visible_cols,
            None,
            ResourceDisplayMode::Values,
            ResourceDisplayMode::Values,
            ResourceDisplayMode::Values,
        )
    );

    // Mutating an off-screen row (index 50) must NOT invalidate the viewport fingerprint.
    slice.rows[50].cpu_permille = 950;
    slice.rows[50].memory_bytes = 2 * 1024 * 1024 * 1024;
    assert_eq!(
        fp_base,
        compute_processes_viewport_fingerprint(
            &slice,
            &layout,
            &visible_rows,
            &visible_cols,
            None,
            ResourceDisplayMode::Values,
            ResourceDisplayMode::Values,
            ResourceDisplayMode::Values,
        )
    );

    // Sub-display-tenth memory jitter (+512 bytes on 128.0 MB) renders identically -> same fp.
    slice.rows[2].memory_bytes += 512;
    assert_eq!(
        fp_base,
        compute_processes_viewport_fingerprint(
            &slice,
            &layout,
            &visible_rows,
            &visible_cols,
            None,
            ResourceDisplayMode::Values,
            ResourceDisplayMode::Values,
            ResourceDisplayMode::Values,
        )
    );

    // Visible CPU change invalidates the fingerprint.
    slice.rows[2].cpu_permille = 120;
    let fp_cpu = compute_processes_viewport_fingerprint(
        &slice,
        &layout,
        &visible_rows,
        &visible_cols,
        None,
        ResourceDisplayMode::Values,
        ResourceDisplayMode::Values,
        ResourceDisplayMode::Values,
    );
    assert_ne!(fp_base, fp_cpu);

    // Selection change invalidates the fingerprint.
    let fp_sel = compute_processes_viewport_fingerprint(
        &slice,
        &layout,
        &visible_rows,
        &visible_cols,
        Some(2),
        ResourceDisplayMode::Values,
        ResourceDisplayMode::Values,
        ResourceDisplayMode::Values,
    );
    assert_ne!(fp_cpu, fp_sel);
}

#[gpui_kit::test]
fn processes_view_right_click_column_coord_long_cmdline_cap_and_viewport_dirty_skip(
    cx: &mut TestAppContext,
) {
    use std::sync::Arc;

    use loom_config::{AppSettings, ConfigStore};
    use loom_engine::EngineCoordinator;
    use loom_pal::{NativeBackend, PlatformBackend as _};
    use loom_types::{ProcessRowProjection, ProcessStatusFlags, ProcessTableSlice};
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
            let view = cx.new(|cx| ProcessesView::new(store_for_window, shell, window, cx));
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
                .downcast::<ProcessesView>()
                .expect("root wraps ProcessesView")
        })
        .expect("read root");
    let table = view.read_with(&vcx, |v, _| v.table.clone());

    coordinator_for_shutdown.shutdown();
    vcx.run_until_parked();

    let long_cmdline: Arc<str> = Arc::from(format!(
        "\"C:\\Program Files\\LongApp\\app.exe\" --token={}",
        "x".repeat(8192)
    ));
    let make_rows = |visible_cpu: u16, offscreen_cpu: u16| -> Vec<ProcessRowProjection> {
        (0..60u32)
            .map(|i| ProcessRowProjection {
                kind: ProcessRowKind::Process,
                key: ProcessKey::new(2000 + i, 10_000 + u64::from(i)),
                depth: 0,
                has_children: false,
                is_expanded: false,
                child_count: 0,
                name: Arc::from(format!("proc-{i}.exe")),
                display_name: Arc::from(format!("Process {i}")),
                window_title: None,
                publisher: Some(Arc::from("Test Publisher")),
                exe_path: Some(Arc::from(r"C:\Program Files\LongApp\app.exe")),
                command_line: if i == 0 {
                    Some(Arc::clone(&long_cmdline))
                } else {
                    None
                },
                icon: None,
                category: ProcessCategory::App,
                status: ProcessStatusFlags::empty(),
                cpu_permille: if i == 0 {
                    visible_cpu
                } else if i == 55 {
                    offscreen_cpu
                } else {
                    5
                },
                memory_bytes: 64 * 1024 * 1024,
                memory_permille: 4,
                disk_bytes_per_sec: 0,
                disk_permille: 0,
                net_bytes_per_sec: 0,
                net_permille: 0,
                gpu_util_permille: 0,
                gpu_dedicated_bytes: 0,
                parent_key: None,
                leak_warning: None,
                webview2_role: None,
            })
            .collect()
    };

    let platform = store.read_with(&vcx, |s, _| s.snapshot().platform);
    let slice_v1 = Arc::new(ProcessTableSlice {
        revision: 100,
        rows: make_rows(10, 10),
        ..Default::default()
    });
    let notified_v1 = view.update(&mut vcx, |this, cx| {
        this.commit_snapshot(100, Arc::clone(&slice_v1), platform, cx)
    });
    assert!(notified_v1, "initial commit must notify TableState");
    vcx.update(|window, cx| window.draw(cx).clear(cx));

    // 1. Table-level right-click column coordinate detection (zero per-cell listeners).
    for (x_pos, expected_col) in [(100.0, Some(0)), (300.0, Some(1)), (380.0, Some(2))] {
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

    // 2. 8 KB command line is capped in RowTextCache preview while Copy & CSV keep full text.
    let key0 = slice_v1.rows[0].key;
    let preview = table.update(&mut vcx, |t, cx| {
        let preview = {
            let d = t.delegate_mut();
            d.selected_key = Some(key0);
            d.column_layout
                .apply_visible_slugs(&["status", "cpu", "memory", "command_line"]);
            d.invalidate_viewport_fingerprint();
            d.row_cache
                .preview_slot(key0, 0, &long_cmdline, DEFAULT_CELL_PREVIEW_CHARS)
        };
        t.refresh(cx);
        preview
    });
    assert!(
        preview.chars().count() <= DEFAULT_CELL_PREVIEW_CHARS + 1,
        "preview must be capped to <= {} chars (+ ellipsis), got {}",
        DEFAULT_CELL_PREVIEW_CHARS,
        preview.chars().count()
    );
    assert!(preview.ends_with('…'));

    let (summary_text, csv_rows) = table.read_with(&vcx, |t, _| {
        (
            t.delegate().selected_copy_summary().expect("row 0 summary"),
            t.delegate().export_visible_csv_payload().1,
        )
    });
    assert!(
        summary_text.contains(long_cmdline.as_ref()),
        "copy summary must preserve full 8 KB command line"
    );
    assert!(
        csv_rows[0]
            .iter()
            .any(|cell| cell.as_str() == long_cmdline.as_ref()),
        "CSV export must preserve full 8 KB command line"
    );

    vcx.update(|window, cx| window.draw(cx).clear(cx));

    // 3. Viewport dirty-check skips cx.notify() when only an off-screen row (index 55) changes.
    let slice_v2 = Arc::new(ProcessTableSlice {
        revision: 101,
        rows: make_rows(10, 999),
        ..Default::default()
    });
    let notified_v2 = view.update(&mut vcx, |this, cx| {
        this.commit_snapshot(101, slice_v2, platform, cx)
    });
    assert!(
        !notified_v2,
        "off-screen metric change must skip TableState cx.notify()"
    );

    // 4. Visible row change (index 0 CPU 10 -> 250) triggers cx.notify().
    let slice_v3 = Arc::new(ProcessTableSlice {
        revision: 102,
        rows: make_rows(250, 999),
        ..Default::default()
    });
    let notified_v3 = view.update(&mut vcx, |this, cx| {
        this.commit_snapshot(102, slice_v3, platform, cx)
    });
    assert!(notified_v3, "visible row CPU change must notify TableState");

    // 5. Programmatic selection re-anchoring preserves last_viewport_fingerprint across
    //    the emitted TableEvent::SelectRow callback, and hover-freeze transitions update
    //    the isolated SortPausedBadge without clearing the table's viewport fingerprint.
    let mut reordered = make_rows(250, 999);
    reordered.swap(0, 3);
    let slice_v4 = Arc::new(ProcessTableSlice {
        revision: 103,
        rows: reordered,
        ..Default::default()
    });
    view.update(&mut vcx, |this, cx| {
        this.commit_snapshot(103, slice_v4, platform, cx);
    });
    vcx.run_until_parked();
    assert_eq!(
        table.read_with(&vcx, |t, _| t.selected_row()),
        Some(3),
        "selected ProcessKey must re-anchor to its new row index"
    );
    assert!(
        table.read_with(&vcx, |t, _| t
            .delegate()
            .last_viewport_fingerprint
            .is_some()),
        "programmatic selection re-anchoring must not self-invalidate last_viewport_fingerprint"
    );

    let fp_before_hover = table.read_with(&vcx, |t, _| t.delegate().last_viewport_fingerprint);
    table.update(&mut vcx, |t, cx| {
        t.delegate_mut().set_table_hovered(true, cx);
    });
    assert_eq!(
        table.read_with(&vcx, |t, _| t.delegate().last_viewport_fingerprint),
        fp_before_hover,
        "hover-freeze transition must update isolated SortPausedBadge without clearing viewport fingerprint"
    );

    // 6. ProcessesView holds MonitorDomain::Processes while mounted.
    assert!(
        coordinator_for_shutdown
            .leases()
            .is_active(loom_types::MonitorDomain::Processes),
        "mounted ProcessesView must hold MonitorDomain::Processes lease"
    );

    // 7. Selection enablement transitions notify only the isolated ToolbarActionButton
    //    entity without re-rendering the outer ProcessesView toolbar, and telemetry records
    //    both commits and viewport fingerprint skips.
    let outer_notifies = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let outer_notifies_clone = Arc::clone(&outer_notifies);
    let _obs = vcx.update(|_, cx| {
        cx.observe(&view, move |_, _| {
            outer_notifies_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
    });
    let end_btn = view.read_with(&vcx, |this, _| this.end_task_button.clone());
    assert!(
        end_btn.read_with(&vcx, |b, _| b.is_enabled()),
        "end task button should be enabled while row 3 is selected"
    );
    table.update(&mut vcx, |t, cx| t.clear_selection(cx));
    vcx.run_until_parked();
    assert!(
        !end_btn.read_with(&vcx, |b, _| b.is_enabled()),
        "clearing selection must disable isolated end task button"
    );
    table.update(&mut vcx, |t, cx| t.set_selected_row(1, cx));
    vcx.run_until_parked();
    assert!(
        end_btn.read_with(&vcx, |b, _| b.is_enabled()),
        "selecting a terminable row must re-enable isolated end task button"
    );
    assert_eq!(
        table.read_with(&vcx, |t, _| t.delegate().selected_row_hint),
        Some(1),
        "selected_row_hint must stay synced with TableEvent::SelectRow"
    );
    assert_eq!(
        outer_notifies.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "selection transitions must not notify the enclosing ProcessesView toolbar"
    );
    let tel = list_telemetry().snapshot(ListViewTelemetryKind::Processes);
    assert!(tel.commits >= 4);
    assert!(tel.viewport_fp_skips >= 1);

    coordinator_for_shutdown.shutdown();
}
