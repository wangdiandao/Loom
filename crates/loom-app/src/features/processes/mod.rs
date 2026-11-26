//! Live-data Processes overview vertical feature slice.
//!
//! Renders real live OS processes from [`UiStore`] with virtualized scrolling,
//! Grouped / Tree view mode switching, sortable column headers, cached RGBA8 process
//! icons, `loom-ui-kit` heatmap cells and status badges, `ProcessKey`-anchored
//! selection + `Delete` hotkey, multi-level right-click `PopupMenu`, and cross-page
//! navigation to Details via [`ShellHandle`].

mod state;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _,
    StyledExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, Context, Entity, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
    KeyDownEvent, MouseButton, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, UniformListScrollHandle, Window,
    div, img, prelude::FluentBuilder as _, px, uniform_list,
};
use loom_types::{
    CapabilityFlags, ProcessKey, ProcessRowKind, ProcessRowProjection, ProcessSortColumn,
    ProcessStatusFlags, ProcessViewMode, ResourceDisplayMode, SortDirection,
};
use loom_ui_kit::{
    HeatmapLevel, LoomSemanticColors, StatusLabels, format_disk_cell, format_memory_cell,
    format_network_cell, format_permille, heatmap_cell, status_badges,
};

use crate::bridge::{UiCommands, UiStore, UiStoreEvent};
use crate::i18n;
use crate::shell::{ShellHandle, dialogs};

use self::state::{
    IconRenderCache, ResourceColumn, apply_resource_display_mode, can_terminate_process,
    find_selected_row, is_efficiency_mode_eligible, next_sort_direction,
    requires_end_task_confirmation, search_online_query, secondary_subtitle, stable_row_id,
};

const PROCESS_ROW_HEIGHT: f32 = 32.0;

const COL_STATUS_W: f32 = 116.0;
const COL_CPU_W: f32 = 74.0;
const COL_MEMORY_W: f32 = 88.0;
const COL_DISK_W: f32 = 82.0;
const COL_NETWORK_W: f32 = 84.0;
const COL_GPU_W: f32 = 68.0;
const COL_PUBLISHER_W: f32 = 148.0;

/// Embedded Lucide-style leaf vector matching `loom_ui_kit::status_badges` for the
/// Efficiency Mode toolbar button and context menu.
const ECO_LEAF_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11 20a10 10 0 0 0 10-10 25.9 25.9 0 0 0-1.04-7.281 1 1 0 0 0-1.755-.325C15.833 5.5 13 5.5 9.8 6.1A7 7 0 0 0 11 20"/><path d="M2 21a5 5 0 0 1 2.911-4.544C7.613 15.212 8.351 15.24 11 13"/></svg>"#;

/// Live Processes overview view.
pub(crate) struct ProcessesView {
    store: Entity<UiStore>,
    shell: ShellHandle,
    focus_handle: FocusHandle,
    selected_key: Option<ProcessKey>,
    last_processes_rev: u64,
    scroll_handle: UniformListScrollHandle,
    icon_cache: Rc<RefCell<IconRenderCache>>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for ProcessesView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ProcessesView {
    /// Creates a new [`ProcessesView`] subscribed to [`UiStore`] snapshot changes.
    pub(crate) fn new(store: Entity<UiStore>, shell: ShellHandle, cx: &mut Context<Self>) -> Self {
        let initial_rev = store.read(cx).snapshot().revisions.processes;
        let sub = cx.subscribe(&store, |this, _store, event, cx| match event {
            UiStoreEvent::SnapshotUpdated(revs) => {
                if revs.processes != this.last_processes_rev {
                    this.last_processes_rev = revs.processes;
                    cx.notify();
                }
            }
            UiStoreEvent::SettingsChanged => cx.notify(),
            UiStoreEvent::ElevationRequired { .. } | UiStoreEvent::CommandFailed { .. } => {}
        });

        Self {
            store,
            shell,
            focus_handle: cx.focus_handle(),
            selected_key: None,
            last_processes_rev: initial_rev,
            scroll_handle: UniformListScrollHandle::new(),
            icon_cache: Rc::new(RefCell::new(IconRenderCache::new())),
            _subscriptions: vec![sub],
        }
    }

    fn select_process(&mut self, key: ProcessKey, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_key = Some(key);
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn trigger_end_task_for_selected(&self, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self.store.read(cx).snapshot().clone();
        let Some(row) = find_selected_row(&snapshot.processes.rows, self.selected_key) else {
            return;
        };
        trigger_end_task_for_row(&self.store, row, window, cx);
    }

    fn toggle_efficiency_mode_for_selected(&self, cx: &mut Context<Self>) {
        let snapshot = self.store.read(cx).snapshot().clone();
        let Some(row) = find_selected_row(&snapshot.processes.rows, self.selected_key) else {
            return;
        };
        if !is_efficiency_mode_eligible(row, snapshot.capabilities) {
            return;
        }
        let next_enabled = !row.status.contains(ProcessStatusFlags::EFFICIENCY_MODE);
        UiCommands::set_efficiency_mode(&self.store, row.key, next_enabled, cx);
    }
}

/// Triggers End Task on `row`, routing system/critical/protected processes through a confirmation dialog.
fn trigger_end_task_for_row(
    store: &Entity<UiStore>,
    row: &ProcessRowProjection,
    window: &mut Window,
    cx: &mut App,
) {
    if !can_terminate_process(row) {
        return;
    }
    if requires_end_task_confirmation(row) {
        dialogs::open_confirm_end_task_dialog(
            store.clone(),
            row.key,
            row.display_name.to_string(),
            true,
            window,
            cx,
        );
    } else {
        UiCommands::terminate_process(store, row.key, cx);
    }
}

/// Builds the nested `Resource values > {Memory, Disk, Network} > {Values, Percents}` submenu.
fn build_resource_values_submenu(
    menu: PopupMenu,
    store: Entity<UiStore>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let settings = store.read(cx).settings().clone();
    let mem_mode = settings.memory_display_mode;
    let disk_mode = settings.disk_display_mode;
    let net_mode = settings.network_display_mode;

    let store_mem = store.clone();
    let store_disk = store.clone();
    let store_net = store;

    menu.submenu(
        i18n::tr("processes.menu_resource_memory"),
        window,
        cx,
        move |sub, _window, _cx| {
            add_resource_mode_items(
                sub,
                store_mem.clone(),
                mem_mode,
                disk_mode,
                net_mode,
                ResourceColumn::Memory,
            )
        },
    )
    .submenu(
        i18n::tr("processes.menu_resource_disk"),
        window,
        cx,
        move |sub, _window, _cx| {
            add_resource_mode_items(
                sub,
                store_disk.clone(),
                mem_mode,
                disk_mode,
                net_mode,
                ResourceColumn::Disk,
            )
        },
    )
    .submenu(
        i18n::tr("processes.menu_resource_network"),
        window,
        cx,
        move |sub, _window, _cx| {
            add_resource_mode_items(
                sub,
                store_net.clone(),
                mem_mode,
                disk_mode,
                net_mode,
                ResourceColumn::Network,
            )
        },
    )
}

fn add_resource_mode_items(
    menu: PopupMenu,
    store: Entity<UiStore>,
    mem_mode: ResourceDisplayMode,
    disk_mode: ResourceDisplayMode,
    net_mode: ResourceDisplayMode,
    column: ResourceColumn,
) -> PopupMenu {
    let current = match column {
        ResourceColumn::Memory => mem_mode,
        ResourceColumn::Disk => disk_mode,
        ResourceColumn::Network => net_mode,
    };

    let (val_m, val_d, val_n) = apply_resource_display_mode(
        mem_mode,
        disk_mode,
        net_mode,
        column,
        ResourceDisplayMode::Values,
    );
    let (pct_m, pct_d, pct_n) = apply_resource_display_mode(
        mem_mode,
        disk_mode,
        net_mode,
        column,
        ResourceDisplayMode::Percent,
    );

    let store_val = store.clone();
    let store_pct = store;

    menu.item(
        PopupMenuItem::new(i18n::tr("processes.menu_mode_values"))
            .checked(current == ResourceDisplayMode::Values)
            .on_click(move |_, _, cx| {
                UiCommands::set_resource_display_modes(&store_val, val_m, val_d, val_n, cx);
            }),
    )
    .item(
        PopupMenuItem::new(i18n::tr("processes.menu_mode_percent"))
            .checked(current == ResourceDisplayMode::Percent)
            .on_click(move |_, _, cx| {
                UiCommands::set_resource_display_modes(&store_pct, pct_m, pct_d, pct_n, cx);
            }),
    )
}

/// Builds the full `ProcessKey`-anchored right-click context menu for a process row.
fn build_row_context_menu(
    mut menu: PopupMenu,
    store: Entity<UiStore>,
    shell: ShellHandle,
    fallback_row: ProcessRowProjection,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let key = fallback_row.key;
    let snapshot = store.read(cx).snapshot().clone();
    let eco_supported = snapshot.capabilities.contains(CapabilityFlags::ECO_MODE);
    let row = find_selected_row(&snapshot.processes.rows, Some(key))
        .cloned()
        .unwrap_or(fallback_row);

    let has_children = row.has_children;
    let is_expanded = row.is_expanded;
    let can_end = can_terminate_process(&row);
    let eco_eligible = is_efficiency_mode_eligible(&row, snapshot.capabilities);
    let eco_active = row.status.contains(ProcessStatusFlags::EFFICIENCY_MODE);
    let exe_path = row.exe_path.clone();
    let query = search_online_query(&row);

    if has_children {
        let store_expand = store.clone();
        let expand_label = if is_expanded {
            i18n::tr("processes.menu_collapse")
        } else {
            i18n::tr("processes.menu_expand")
        };
        let expand_icon = if is_expanded {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        };
        menu = menu.item(PopupMenuItem::new(expand_label).icon(expand_icon).on_click(
            move |_, _, cx| {
                UiCommands::toggle_process_expanded(&store_expand, key, cx);
            },
        ));
    }

    let store_end = store.clone();
    let row_for_end = row.clone();
    menu = menu.item(
        PopupMenuItem::new(i18n::tr("processes.menu_end_task"))
            .icon(IconName::CircleX)
            .disabled(!can_end)
            .on_click(move |_, window, cx| {
                trigger_end_task_for_row(&store_end, &row_for_end, window, cx);
            }),
    );

    if eco_supported {
        let store_eco = store.clone();
        menu = menu.item(
            PopupMenuItem::new(i18n::tr("processes.menu_efficiency_mode"))
                .checked(eco_active)
                .disabled(!eco_eligible)
                .on_click(move |_, _, cx| {
                    UiCommands::set_efficiency_mode(&store_eco, key, !eco_active, cx);
                }),
        );
    }

    let store_res = store.clone();
    menu = menu
        .separator()
        .submenu(
            i18n::tr("processes.menu_resource_values"),
            window,
            cx,
            move |sub, window, cx| {
                build_resource_values_submenu(sub, store_res.clone(), window, cx)
            },
        )
        .separator();

    let shell_details = shell;
    menu = menu.item(
        PopupMenuItem::new(i18n::tr("processes.jump_to_details"))
            .icon(IconName::ArrowRight)
            .on_click(move |_, window, cx| {
                shell_details.jump_to_details(key, window, cx);
            }),
    );

    let store_loc = store.clone();
    let path_loc = exe_path.clone();
    menu = menu.item(
        PopupMenuItem::new(i18n::tr("processes.menu_open_file_location"))
            .icon(IconName::FolderOpen)
            .disabled(path_loc.is_none())
            .on_click(move |_, _, cx| {
                if let Some(ref p) = path_loc {
                    UiCommands::open_file_location(&store_loc, PathBuf::from(p.as_ref()), cx);
                }
            }),
    );

    let store_search = store.clone();
    menu = menu.item(
        PopupMenuItem::new(i18n::tr("processes.menu_search_online"))
            .icon(IconName::Globe)
            .on_click(move |_, _, cx| {
                UiCommands::search_online(&store_search, query.clone(), cx);
            }),
    );

    let store_props = store;
    let path_props = exe_path;
    menu.item(
        PopupMenuItem::new(i18n::tr("processes.menu_properties"))
            .icon(IconName::Info)
            .disabled(path_props.is_none())
            .on_click(move |_, _, cx| {
                if let Some(ref p) = path_props {
                    UiCommands::show_file_properties(&store_props, PathBuf::from(p.as_ref()), cx);
                }
            }),
    )
}

impl Render for ProcessesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store_ref = self.store.read(cx);
        let snapshot = store_ref.snapshot().clone();
        let settings = store_ref.settings().clone();
        let slice = snapshot.processes.clone();
        let capabilities = snapshot.capabilities;
        let platform = snapshot.platform;
        let eco_supported = capabilities.contains(CapabilityFlags::ECO_MODE);
        let view_mode = slice.view_mode;
        let sort_col = slice.sort_column;
        let sort_dir = slice.sort_direction;
        let row_count = slice.rows.len();

        let selected_row = find_selected_row(&slice.rows, self.selected_key);
        let end_task_enabled = selected_row.is_some_and(can_terminate_process);
        let eco_eligible =
            selected_row.is_some_and(|row| is_efficiency_mode_eligible(row, capabilities));
        let eco_active =
            selected_row.is_some_and(|r| r.status.contains(ProcessStatusFlags::EFFICIENCY_MODE));

        let theme = cx.theme();
        let border = theme.border;
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let muted_bg = theme.muted;
        let secondary_bg = theme.secondary;
        let primary = theme.primary;
        let radius = theme.radius;
        let semantic = LoomSemanticColors::from_theme(theme);

        let store_grouped = self.store.clone();
        let store_tree = self.store.clone();
        let store_run_task = self.store.clone();
        let store_refresh = self.store.clone();

        let toolbar = h_flex()
            .w_full()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_3()
            .px_4()
            .py_2p5()
            .border_b_1()
            .border_color(border)
            .child(
                h_flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_base()
                            .font_semibold()
                            .text_color(fg)
                            .mr_1()
                            .child(i18n::tr("processes.title")),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .p_0p5()
                            .rounded(radius)
                            .bg(muted_bg.opacity(0.65))
                            .child(
                                Button::new("processes-mode-grouped")
                                    .xsmall()
                                    .label(i18n::tr("processes.view_grouped"))
                                    .map(|btn| {
                                        if view_mode == ProcessViewMode::Grouped {
                                            btn.primary()
                                        } else {
                                            btn.ghost()
                                        }
                                    })
                                    .on_click(move |_, _, cx| {
                                        UiCommands::set_process_view_mode(
                                            &store_grouped,
                                            ProcessViewMode::Grouped,
                                            cx,
                                        );
                                    }),
                            )
                            .child(
                                Button::new("processes-mode-tree")
                                    .xsmall()
                                    .label(i18n::tr("processes.view_tree"))
                                    .map(|btn| {
                                        if view_mode == ProcessViewMode::Tree {
                                            btn.primary()
                                        } else {
                                            btn.ghost()
                                        }
                                    })
                                    .on_click(move |_, _, cx| {
                                        UiCommands::set_process_view_mode(
                                            &store_tree,
                                            ProcessViewMode::Tree,
                                            cx,
                                        );
                                    }),
                            ),
                    )
                    .child(metric_pill(
                        i18n::tr("processes.count_label"),
                        slice.total_process_count.to_string(),
                        muted_bg,
                        muted_fg,
                        fg,
                        radius,
                    ))
                    .child(metric_pill(
                        i18n::tr("processes.cpu_label"),
                        format_permille(slice.total_cpu_permille),
                        muted_bg,
                        muted_fg,
                        fg,
                        radius,
                    ))
                    .child(metric_pill(
                        i18n::tr("processes.memory_label"),
                        format_permille(slice.total_memory_permille),
                        muted_bg,
                        muted_fg,
                        fg,
                        radius,
                    )),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1p5()
                    .child(
                        Button::new("processes-run-new-task")
                            .small()
                            .primary()
                            .icon(IconName::Play)
                            .label(i18n::tr("processes.run_new_task"))
                            .on_click(move |_, window, cx| {
                                dialogs::open_run_new_task_dialog(
                                    store_run_task.clone(),
                                    window,
                                    cx,
                                );
                            }),
                    )
                    .child(
                        Button::new("processes-end-task")
                            .small()
                            .map(|btn| {
                                if end_task_enabled {
                                    btn.danger()
                                } else {
                                    btn.outline()
                                }
                            })
                            .icon(IconName::CircleX)
                            .label(i18n::tr("processes.end_task"))
                            .disabled(!end_task_enabled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.trigger_end_task_for_selected(window, cx);
                            })),
                    )
                    .when(eco_supported, |this| {
                        this.child(
                            Button::new("processes-efficiency-mode")
                                .small()
                                .outline()
                                .selected(eco_active)
                                .icon(Icon::default().data(ECO_LEAF_SVG))
                                .label(i18n::tr("processes.efficiency_mode"))
                                .disabled(!eco_eligible)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.toggle_efficiency_mode_for_selected(cx);
                                })),
                        )
                    })
                    .child(
                        Button::new("processes-refresh")
                            .small()
                            .ghost()
                            .icon(IconName::RefreshCw)
                            .label(i18n::tr("processes.refresh"))
                            .on_click(move |_, _, cx| {
                                UiCommands::force_refresh(&store_refresh, cx);
                            }),
                    ),
            );

        let column_header = h_flex()
            .w_full()
            .h(px(32.))
            .flex_shrink_0()
            .items_center()
            .px_4()
            .gap_2()
            .bg(secondary_bg.opacity(0.45))
            .border_b_1()
            .border_color(border)
            .text_xs()
            .font_medium()
            .text_color(muted_fg)
            .child(div().flex_1().min_w(px(180.)).child(sortable_column_header(
                "proc-col-name",
                i18n::tr("processes.col_name"),
                ProcessSortColumn::Name,
                sort_col,
                sort_dir,
                false,
                fg,
                muted_fg,
                primary,
                self.store.clone(),
            )))
            .child(
                div()
                    .w(px(COL_STATUS_W))
                    .flex_shrink_0()
                    .child(sortable_column_header(
                        "proc-col-status",
                        i18n::tr("processes.col_status"),
                        ProcessSortColumn::Status,
                        sort_col,
                        sort_dir,
                        false,
                        fg,
                        muted_fg,
                        primary,
                        self.store.clone(),
                    )),
            )
            .child(
                div()
                    .w(px(COL_CPU_W))
                    .flex_shrink_0()
                    .child(sortable_column_header(
                        "proc-col-cpu",
                        i18n::tr("processes.col_cpu"),
                        ProcessSortColumn::Cpu,
                        sort_col,
                        sort_dir,
                        true,
                        fg,
                        muted_fg,
                        primary,
                        self.store.clone(),
                    )),
            )
            .child(
                div()
                    .w(px(COL_MEMORY_W))
                    .flex_shrink_0()
                    .child(sortable_column_header(
                        "proc-col-memory",
                        i18n::tr("processes.col_memory"),
                        ProcessSortColumn::Memory,
                        sort_col,
                        sort_dir,
                        true,
                        fg,
                        muted_fg,
                        primary,
                        self.store.clone(),
                    )),
            )
            .child(
                div()
                    .w(px(COL_DISK_W))
                    .flex_shrink_0()
                    .child(sortable_column_header(
                        "proc-col-disk",
                        i18n::tr("processes.col_disk"),
                        ProcessSortColumn::Disk,
                        sort_col,
                        sort_dir,
                        true,
                        fg,
                        muted_fg,
                        primary,
                        self.store.clone(),
                    )),
            )
            .child(
                div()
                    .w(px(COL_NETWORK_W))
                    .flex_shrink_0()
                    .child(sortable_column_header(
                        "proc-col-network",
                        i18n::tr("processes.col_network"),
                        ProcessSortColumn::Network,
                        sort_col,
                        sort_dir,
                        true,
                        fg,
                        muted_fg,
                        primary,
                        self.store.clone(),
                    )),
            )
            .child(
                div()
                    .w(px(COL_GPU_W))
                    .flex_shrink_0()
                    .child(sortable_column_header(
                        "proc-col-gpu",
                        i18n::tr("processes.col_gpu"),
                        ProcessSortColumn::Gpu,
                        sort_col,
                        sort_dir,
                        true,
                        fg,
                        muted_fg,
                        primary,
                        self.store.clone(),
                    )),
            )
            .child(
                div()
                    .w(px(COL_PUBLISHER_W))
                    .flex_shrink_0()
                    .truncate()
                    .child(i18n::tr("processes.col_publisher")),
            );

        let body = if row_count == 0 {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .p_8()
                .child(
                    Icon::new(IconName::Search)
                        .size(px(24.))
                        .text_color(muted_fg),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(muted_fg)
                        .child(i18n::tr("processes.empty_search")),
                )
                .into_any_element()
        } else {
            let view_weak = cx.entity().downgrade();
            let store_for_rows = self.store.clone();
            let shell_for_rows = self.shell.clone();
            let icon_cache = self.icon_cache.clone();
            let selected_key = self.selected_key;
            let mem_mode = settings.memory_display_mode;
            let disk_mode = settings.disk_display_mode;
            let net_mode = settings.network_display_mode;
            let status_labels = StatusLabels::new(
                i18n::tr("processes.status_efficiency"),
                i18n::tr("processes.status_suspended"),
                i18n::tr("processes.status_not_responding"),
            );

            div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(
                    uniform_list(
                        "processes-virtual-list",
                        row_count,
                        move |range, _window, cx| {
                            let theme = cx.theme();
                            let row_border = theme.border.opacity(0.45);
                            let row_hover = theme.accent.opacity(0.45);
                            let row_selected = theme.primary.opacity(0.14);
                            let section_bg = theme.muted.opacity(0.6);

                            range
                                .filter_map(|ix| {
                                    let row = slice.rows.get(ix)?;
                                    match row.kind {
                                        ProcessRowKind::SectionHeader { category, count } => {
                                            let label = format!(
                                                "{} ({count})",
                                                i18n::process_category_label(category, platform)
                                            );
                                            Some(
                                                h_flex()
                                                    .id(("proc-section", ix))
                                                    .w_full()
                                                    .h(px(PROCESS_ROW_HEIGHT))
                                                    .items_center()
                                                    .px_4()
                                                    .bg(section_bg)
                                                    .border_b_1()
                                                    .border_color(row_border)
                                                    .text_xs()
                                                    .font_semibold()
                                                    .text_color(primary)
                                                    .child(label)
                                                    .into_any_element(),
                                            )
                                        }
                                        ProcessRowKind::Process => {
                                            let key = row.key;
                                            let row_id = stable_row_id(key);
                                            let is_selected = selected_key == Some(key);
                                            let indent = f32::from(row.depth.min(8)) * 16.0;
                                            let has_children = row.has_children;
                                            let is_expanded = row.is_expanded;

                                            let rendered_icon = row.icon.as_ref().and_then(|icon| {
                                                icon_cache.borrow_mut().get_or_insert(icon)
                                            });

                                            let display_name: SharedString =
                                                row.display_name.to_string().into();
                                            let child_badge: Option<SharedString> =
                                                (row.child_count > 0)
                                                    .then(|| format!("({})", row.child_count).into());
                                            let subtitle: Option<SharedString> =
                                                secondary_subtitle(row)
                                                    .map(|s| s.to_owned().into());
                                            let publisher_text: SharedString =
                                                row.publisher.as_deref().unwrap_or("").to_owned().into();

                                            let cpu_text = format_permille(row.cpu_permille);
                                            let mem_text = format_memory_cell(
                                                row.memory_bytes,
                                                row.memory_permille,
                                                mem_mode,
                                            );
                                            let disk_text = format_disk_cell(
                                                row.disk_bytes_per_sec,
                                                row.disk_permille,
                                                disk_mode,
                                            );
                                            let net_text = format_network_cell(
                                                row.net_bytes_per_sec,
                                                row.net_permille,
                                                net_mode,
                                            );
                                            let gpu_text = format_permille(row.gpu_util_permille);

                                            let toggle_store = store_for_rows.clone();
                                            let select_left = view_weak.clone();
                                            let select_right = view_weak.clone();
                                            let menu_store = store_for_rows.clone();
                                            let menu_shell = shell_for_rows.clone();
                                            let menu_row = row.clone();

                                            let icon_el = if let Some(render_img) = rendered_icon {
                                                img(render_img)
                                                    .size(px(16.0))
                                                    .flex_shrink_0()
                                                    .into_any_element()
                                            } else {
                                                Icon::new(IconName::Frame)
                                                    .size(px(15.0))
                                                    .text_color(muted_fg.opacity(0.6))
                                                    .into_any_element()
                                            };

                                            let row_a11y_label: SharedString =
                                                format!("{} (PID {})", row.display_name, key.pid)
                                                    .into();
                                            Some(
                                                h_flex()
                                                    .id(("proc-row", row_id))
                                                    .role(gpui_kit::Role::ListItem)
                                                    .aria_label(row_a11y_label)
                                                    .aria_selected(is_selected)
                                                    .w_full()
                                                    .h(px(PROCESS_ROW_HEIGHT))
                                                    .items_center()
                                                    .px_4()
                                                    .gap_2()
                                                    .border_b_1()
                                                    .border_color(row_border)
                                                    .text_xs()
                                                    .text_color(fg)
                                                    .when(is_selected, |s| s.bg(row_selected))
                                                    .hover(move |s| {
                                                        if is_selected {
                                                            s.bg(row_selected)
                                                        } else {
                                                            s.bg(row_hover)
                                                        }
                                                    })
                                                    .on_click(move |_, window, cx| {
                                                        let _ = select_left.update(
                                                            cx,
                                                            |view, cx| {
                                                                view.select_process(
                                                                    key, window, cx,
                                                                );
                                                            },
                                                        );
                                                    })
                                                    .on_mouse_down(
                                                        MouseButton::Right,
                                                        move |_, window, cx| {
                                                            let _ = select_right.update(
                                                                cx,
                                                                |view, cx| {
                                                                    view.select_process(
                                                                        key, window, cx,
                                                                    );
                                                                },
                                                            );
                                                        },
                                                    )
                                                    .child(
                                                        h_flex()
                                                            .flex_1()
                                                            .min_w(px(180.))
                                                            .items_center()
                                                            .gap_1p5()
                                                            .pl(px(indent))
                                                            .overflow_hidden()
                                                            .child(if has_children {
                                                                Button::new((
                                                                    "proc-expand",
                                                                    row_id,
                                                                ))
                                                                .xsmall()
                                                                .ghost()
                                                                .icon(if is_expanded {
                                                                    IconName::ChevronDown
                                                                } else {
                                                                    IconName::ChevronRight
                                                                })
                                                                .on_click(move |_, _, cx| {
                                                                    UiCommands::toggle_process_expanded(
                                                                        &toggle_store,
                                                                        key,
                                                                        cx,
                                                                    );
                                                                })
                                                                .into_any_element()
                                                            } else {
                                                                div()
                                                                    .w(px(20.))
                                                                    .flex_shrink_0()
                                                                    .into_any_element()
                                                            })
                                                            .child(icon_el)
                                                            .child(
                                                                h_flex()
                                                                    .flex_1()
                                                                    .min_w_0()
                                                                    .items_center()
                                                                    .gap_1p5()
                                                                    .overflow_hidden()
                                                                    .child(
                                                                        div()
                                                                            .min_w_0()
                                                                            .truncate()
                                                                            .when(
                                                                                is_selected
                                                                                    || has_children,
                                                                                |d| d.font_medium(),
                                                                            )
                                                                            .child(display_name),
                                                                    )
                                                                    .when_some(
                                                                        child_badge,
                                                                        |this, badge| {
                                                                            this.child(
                                                                                div()
                                                                                    .flex_shrink_0()
                                                                                    .text_color(
                                                                                        muted_fg,
                                                                                    )
                                                                                    .child(badge),
                                                                            )
                                                                        },
                                                                    )
                                                                    .when_some(
                                                                        subtitle,
                                                                        |this, sub| {
                                                                            this.child(
                                                                                div()
                                                                                    .flex_1()
                                                                                    .min_w_0()
                                                                                    .truncate()
                                                                                    .text_color(
                                                                                        muted_fg
                                                                                            .opacity(
                                                                                                0.75,
                                                                                            ),
                                                                                    )
                                                                                    .child(sub),
                                                                            )
                                                                        },
                                                                    ),
                                                            ),
                                                    )
                                                    .child(
                                                        div()
                                                            .w(px(COL_STATUS_W))
                                                            .flex_shrink_0()
                                                            .overflow_hidden()
                                                            .child(status_badges(
                                                                row.status,
                                                                &status_labels,
                                                                &semantic,
                                                            )),
                                                    )
                                                    .child(
                                                        div().w(px(COL_CPU_W)).flex_shrink_0().child(
                                                            heatmap_cell(
                                                                cpu_text,
                                                                HeatmapLevel::from_permille(
                                                                    row.cpu_permille,
                                                                ),
                                                                &semantic,
                                                            ),
                                                        ),
                                                    )
                                                    .child(
                                                        div()
                                                            .w(px(COL_MEMORY_W))
                                                            .flex_shrink_0()
                                                            .child(heatmap_cell(
                                                                mem_text,
                                                                HeatmapLevel::from_permille(
                                                                    row.memory_permille,
                                                                ),
                                                                &semantic,
                                                            )),
                                                    )
                                                    .child(
                                                        div()
                                                            .w(px(COL_DISK_W))
                                                            .flex_shrink_0()
                                                            .child(heatmap_cell(
                                                                disk_text,
                                                                HeatmapLevel::from_bytes_per_sec(
                                                                    row.disk_bytes_per_sec,
                                                                ),
                                                                &semantic,
                                                            )),
                                                    )
                                                    .child(
                                                        div()
                                                            .w(px(COL_NETWORK_W))
                                                            .flex_shrink_0()
                                                            .child(heatmap_cell(
                                                                net_text,
                                                                HeatmapLevel::from_bytes_per_sec(
                                                                    row.net_bytes_per_sec,
                                                                ),
                                                                &semantic,
                                                            )),
                                                    )
                                                    .child(
                                                        div().w(px(COL_GPU_W)).flex_shrink_0().child(
                                                            heatmap_cell(
                                                                gpu_text,
                                                                HeatmapLevel::from_permille(
                                                                    row.gpu_util_permille,
                                                                ),
                                                                &semantic,
                                                            ),
                                                        ),
                                                    )
                                                    .child(
                                                        div()
                                                            .w(px(COL_PUBLISHER_W))
                                                            .flex_shrink_0()
                                                            .truncate()
                                                            .text_color(muted_fg)
                                                            .child(publisher_text),
                                                    )
                                                    .context_menu(move |menu, window, cx| {
                                                        build_row_context_menu(
                                                            menu,
                                                            menu_store.clone(),
                                                            menu_shell.clone(),
                                                            menu_row.clone(),
                                                            window,
                                                            cx,
                                                        )
                                                    })
                                                    .into_any_element(),
                                            )
                                        }
                                    }
                                })
                                .collect()
                        },
                    )
                    .size_full()
                    .track_scroll(&self.scroll_handle),
                )
                .vertical_scrollbar(&self.scroll_handle)
                .into_any_element()
        };

        v_flex()
            .id("processes-view-root")
            .track_focus(&self.focus_handle)
            .size_full()
            .min_h_0()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let no_modifiers = !event.keystroke.modifiers.control
                    && !event.keystroke.modifiers.alt
                    && !event.keystroke.modifiers.platform
                    && !event.keystroke.modifiers.shift;
                if no_modifiers && event.keystroke.key == "delete" && this.selected_key.is_some() {
                    this.trigger_end_task_for_selected(window, cx);
                    cx.stop_propagation();
                }
            }))
            .child(toolbar)
            .child(column_header)
            .child(body)
    }
}

#[allow(clippy::too_many_arguments)]
fn sortable_column_header(
    id: &'static str,
    label: String,
    column: ProcessSortColumn,
    active_column: ProcessSortColumn,
    active_direction: SortDirection,
    right_aligned: bool,
    active_fg: gpui_kit::Hsla,
    muted_fg: gpui_kit::Hsla,
    primary: gpui_kit::Hsla,
    store: Entity<UiStore>,
) -> impl IntoElement {
    let is_active = active_column == column;
    let next_dir = next_sort_direction(active_column, active_direction, column);
    let chevron = match active_direction {
        SortDirection::Ascending => IconName::ChevronUp,
        SortDirection::Descending => IconName::ChevronDown,
    };

    h_flex()
        .id(id)
        .w_full()
        .h_full()
        .items_center()
        .gap_1()
        .cursor_pointer()
        .when(right_aligned, |this| this.justify_end())
        .text_color(if is_active { active_fg } else { muted_fg })
        .when(is_active, |this| this.font_semibold())
        .hover(move |s| s.text_color(active_fg))
        .on_click(move |_, _, cx| {
            UiCommands::set_process_sort(&store, column, next_dir, cx);
        })
        .child(div().truncate().child(label))
        .when(is_active, |this| {
            this.child(Icon::new(chevron).xsmall().text_color(primary))
        })
}

fn metric_pill(
    label: String,
    value: String,
    bg: gpui_kit::Hsla,
    label_fg: gpui_kit::Hsla,
    value_fg: gpui_kit::Hsla,
    radius: gpui_kit::Pixels,
) -> impl IntoElement {
    h_flex()
        .items_center()
        .gap_1p5()
        .px_2p5()
        .py_1()
        .rounded(radius)
        .bg(bg.opacity(0.65))
        .text_xs()
        .child(div().text_color(label_fg).child(label))
        .child(div().font_semibold().text_color(value_fg).child(value))
}
