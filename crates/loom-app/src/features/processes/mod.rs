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

use gpui_kit::assets::IconName as AssetIconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};
use gpui_kit::{
    App, Context, Entity, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
    KeyDownEvent, MouseButton, ParentElement as _, Render, ScrollHandle, SharedString,
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
use crate::features::table_columns::{
    ColumnKey, ColumnSpec, HeaderColumnDescriptor, TableColumnLayout,
    attach_header_reorder_handlers, attach_table_column_drag_handlers,
    render_interactive_header_cell,
};
use crate::i18n;
use crate::shell::{ShellHandle, dialogs};

use self::state::{
    IconRenderCache, ResourceColumn, apply_resource_display_mode, can_terminate_process,
    find_selected_row, is_efficiency_mode_eligible, next_sort_direction,
    requires_end_task_confirmation, search_online_query, stable_row_id,
};

const PROCESS_ROW_HEIGHT: f32 = 32.0;
const TABLE_HORIZONTAL_PADDING: f32 = 16.0;
const PROCESSES_TABLE_ID: &str = "processes";

/// Logical columns rendered by [`ProcessesView`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessColumn {
    Name,
    Status,
    Cpu,
    Memory,
    Disk,
    Network,
    Gpu,
    Publisher,
}

impl ColumnKey for ProcessColumn {
    fn slug(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Status => "status",
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::Disk => "disk",
            Self::Network => "network",
            Self::Gpu => "gpu",
            Self::Publisher => "publisher",
        }
    }
}

impl ProcessColumn {
    fn default_specs() -> Vec<ColumnSpec<Self>> {
        vec![
            ColumnSpec::new(Self::Name, 260.0, 140.0, 720.0),
            ColumnSpec::new(Self::Status, 80.0, 56.0, 200.0),
            ColumnSpec::new(Self::Cpu, 82.0, 60.0, 180.0),
            ColumnSpec::new(Self::Memory, 96.0, 68.0, 220.0),
            ColumnSpec::new(Self::Disk, 90.0, 68.0, 220.0),
            ColumnSpec::new(Self::Network, 92.0, 68.0, 220.0),
            ColumnSpec::new(Self::Gpu, 78.0, 60.0, 180.0),
            ColumnSpec::new(Self::Publisher, 176.0, 88.0, 480.0),
        ]
    }

    fn label(self) -> String {
        match self {
            Self::Name => i18n::tr("processes.col_name"),
            Self::Status => i18n::tr("processes.col_status"),
            Self::Cpu => i18n::tr("processes.col_cpu"),
            Self::Memory => i18n::tr("processes.col_memory"),
            Self::Disk => i18n::tr("processes.col_disk"),
            Self::Network => i18n::tr("processes.col_network"),
            Self::Gpu => i18n::tr("processes.col_gpu"),
            Self::Publisher => i18n::tr("processes.col_publisher"),
        }
    }

    const fn sort_column(self) -> Option<ProcessSortColumn> {
        match self {
            Self::Name => Some(ProcessSortColumn::Name),
            Self::Status => Some(ProcessSortColumn::Status),
            Self::Cpu => Some(ProcessSortColumn::Cpu),
            Self::Memory => Some(ProcessSortColumn::Memory),
            Self::Disk => Some(ProcessSortColumn::Disk),
            Self::Network => Some(ProcessSortColumn::Network),
            Self::Gpu => Some(ProcessSortColumn::Gpu),
            Self::Publisher => None,
        }
    }

    const fn right_aligned(self) -> bool {
        matches!(
            self,
            Self::Cpu | Self::Memory | Self::Disk | Self::Network | Self::Gpu
        )
    }
}

/// Live Processes overview view.
pub(crate) struct ProcessesView {
    store: Entity<UiStore>,
    shell: ShellHandle,
    focus_handle: FocusHandle,
    selected_key: Option<ProcessKey>,
    last_processes_rev: u64,
    scroll_handle: UniformListScrollHandle,
    h_scroll_handle: ScrollHandle,
    column_layout: TableColumnLayout<ProcessColumn>,
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
        let mut column_layout = TableColumnLayout::new(ProcessColumn::default_specs());
        if let Some(saved) = store.read(cx).table_column_layout(PROCESSES_TABLE_ID) {
            column_layout.apply_saved(saved);
        }
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
            h_scroll_handle: ScrollHandle::new(),
            column_layout,
            icon_cache: Rc::new(RefCell::new(IconRenderCache::new())),
            _subscriptions: vec![sub],
        }
    }

    fn persist_column_layout(&self, cx: &mut Context<Self>) {
        let snapshot = self.column_layout.snapshot();
        self.store.update(cx, |store, _| {
            store.set_table_column_layout(PROCESSES_TABLE_ID, snapshot);
        });
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
        let platform = snapshot.platform;
        let view_mode = slice.view_mode;
        let sort_col = slice.sort_column;
        let sort_dir = slice.sort_direction;
        let row_count = slice.rows.len();

        let selected_row = find_selected_row(&slice.rows, self.selected_key);
        let end_task_enabled = selected_row.is_some_and(can_terminate_process);

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

        let grouped_label = i18n::tr("processes.view_grouped");
        let tree_label = i18n::tr("processes.view_tree");

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
                                    .icon(Icon::new(AssetIconName::LayoutList))
                                    .tooltip(grouped_label.clone())
                                    .accessibility_label(grouped_label)
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
                                    .icon(Icon::new(AssetIconName::ListTree))
                                    .tooltip(tree_label.clone())
                                    .accessibility_label(tree_label)
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
                    ),
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
                    ),
            );

        let columns: Vec<ColumnSpec<ProcessColumn>> = self.column_layout.columns().to_vec();
        let total_cols = columns.len();
        let table_width = self.column_layout.total_width(TABLE_HORIZONTAL_PADDING);
        let column_layout_snapshot = self.column_layout.clone();

        let header_cells = columns.iter().enumerate().map(|(col_ix, spec)| {
            let col = spec.id;
            let sort_target = col.sort_column();
            let is_active_sort = sort_target == Some(sort_col);
            let next_dir = sort_target
                .map(|target| next_sort_direction(sort_col, sort_dir, target))
                .unwrap_or(SortDirection::Descending);
            let store_for_sort = self.store.clone();

            render_interactive_header_cell(
                col_ix,
                total_cols,
                HeaderColumnDescriptor {
                    id: format!("proc-col-{}", col.slug()).into(),
                    label: col.label().into(),
                    width: spec.width,
                    right_aligned: col.right_aligned(),
                    sortable: sort_target.is_some(),
                    is_active_sort,
                    sort_direction: sort_dir,
                },
                &column_layout_snapshot,
                cx,
                |view: &mut Self| &mut view.column_layout,
                move |_view, _window, cx| {
                    if let Some(target) = sort_target {
                        UiCommands::set_process_sort(&store_for_sort, target, next_dir, cx);
                    }
                },
                |view: &mut Self, cx| view.persist_column_layout(cx),
            )
            .into_any_element()
        });

        let column_header = attach_header_reorder_handlers(
            h_flex()
                .id("processes-column-header")
                .w_full()
                .h(px(32.))
                .flex_shrink_0()
                .items_center()
                .px_2()
                .bg(secondary_bg.opacity(0.45))
                .border_b_1()
                .border_color(border)
                .text_xs()
                .font_medium()
                .text_color(muted_fg)
                .children(header_cells),
            cx,
            |view: &mut Self| &mut view.column_layout,
            |view: &mut Self, cx| view.persist_column_layout(cx),
        );

        let table_area = if row_count == 0 {
            v_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(column_header)
                .child(
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
                        ),
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
                .w_full()
                .relative()
                .child(
                    div()
                        .id("processes-table-hscroll")
                        .size_full()
                        .overflow_x_scroll()
                        .track_scroll(&self.h_scroll_handle)
                        .child(
                            v_flex()
                                .min_w(px(table_width))
                                .w_full()
                                .h_full()
                                .child(column_header)
                                .child(
                                    div().flex_1().min_h_0().w_full().child(
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
                                                            ProcessRowKind::SectionHeader {
                                                                category,
                                                                count,
                                                            } => {
                                                                let label = format!(
                                                                    "{} ({count})",
                                                                    i18n::process_category_label(
                                                                        category, platform
                                                                    )
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
                                                                let is_selected =
                                                                    selected_key == Some(key);
                                                                let indent = f32::from(
                                                                    row.depth.min(8),
                                                                ) * 16.0;
                                                                let has_children = row.has_children;
                                                                let is_expanded = row.is_expanded;

                                                                let rendered_icon =
                                                                    row.icon.as_ref().and_then(
                                                                        |icon| {
                                                                            icon_cache
                                                                                .borrow_mut()
                                                                                .get_or_insert(icon)
                                                                        },
                                                                    );

                                                                let display_name: SharedString =
                                                                    row.display_name
                                                                        .to_string()
                                                                        .into();
                                                                let child_badge: Option<
                                                                    SharedString,
                                                                > = (row.child_count > 0).then(
                                                                    || {
                                                                        format!(
                                                                            "({})",
                                                                            row.child_count
                                                                        )
                                                                        .into()
                                                                    },
                                                                );
                                                                let publisher_text: SharedString =
                                                                    row.publisher
                                                                        .as_deref()
                                                                        .unwrap_or("")
                                                                        .to_owned()
                                                                        .into();

                                                                let cpu_text = format_permille(
                                                                    row.cpu_permille,
                                                                );
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
                                                                let gpu_text = format_permille(
                                                                    row.gpu_util_permille,
                                                                );

                                                                let toggle_store =
                                                                    store_for_rows.clone();
                                                                let select_left = view_weak.clone();
                                                                let select_right =
                                                                    view_weak.clone();
                                                                let menu_store =
                                                                    store_for_rows.clone();
                                                                let menu_shell =
                                                                    shell_for_rows.clone();
                                                                let menu_row = row.clone();

                                                                let row_a11y_label: SharedString =
                                                                    format!(
                                                                        "{} (PID {})",
                                                                        row.display_name, key.pid
                                                                    )
                                                                    .into();

                                                                let cells = columns.iter().map(|spec| {
                                                                    let w = px(spec.width);
                                                                    match spec.id {
                                                                        ProcessColumn::Name => {
                                                                            let icon_el = if let Some(
                                                                                render_img,
                                                                            ) =
                                                                                rendered_icon.clone()
                                                                            {
                                                                                img(render_img)
                                                                                    .size(px(16.0))
                                                                                    .flex_shrink_0()
                                                                                    .into_any_element()
                                                                            } else {
                                                                                Icon::new(
                                                                                    IconName::Frame,
                                                                                )
                                                                                .size(px(15.0))
                                                                                .text_color(
                                                                                    muted_fg
                                                                                        .opacity(0.6),
                                                                                )
                                                                                .into_any_element()
                                                                            };
                                                                            let toggle_store =
                                                                                toggle_store.clone();
                                                                            h_flex()
                                                                                .w(w)
                                                                                .h_full()
                                                                                .flex_shrink_0()
                                                                                .items_center()
                                                                                .gap_1p5()
                                                                                .px_2()
                                                                                .pl(px(8.0 + indent))
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
                                                                                                .child(display_name.clone()),
                                                                                        )
                                                                                        .when_some(
                                                                                            child_badge.clone(),
                                                                                            |this, badge| {
                                                                                                this.child(
                                                                                                    div()
                                                                                                        .flex_shrink_0()
                                                                                                        .text_color(muted_fg)
                                                                                                        .child(badge),
                                                                                                )
                                                                                            },
                                                                                        ),
                                                                                )
                                                                                .into_any_element()
                                                                        }
                                                                        ProcessColumn::Status => {
                                                                            h_flex()
                                                                                .w(w)
                                                                                .h_full()
                                                                                .flex_shrink_0()
                                                                                .items_center()
                                                                                .px_2()
                                                                                .overflow_hidden()
                                                                                .child(status_badges(
                                                                                    row_id,
                                                                                    row.status,
                                                                                    &status_labels,
                                                                                    &semantic,
                                                                                ))
                                                                                .into_any_element()
                                                                        }
                                                                        ProcessColumn::Cpu => div()
                                                                            .w(w)
                                                                            .h_full()
                                                                            .flex_shrink_0()
                                                                            .px_1()
                                                                            .flex()
                                                                            .items_center()
                                                                            .child(heatmap_cell(
                                                                                cpu_text.clone(),
                                                                                HeatmapLevel::from_permille(
                                                                                    row.cpu_permille,
                                                                                ),
                                                                                &semantic,
                                                                            ))
                                                                            .into_any_element(),
                                                                        ProcessColumn::Memory => div()
                                                                            .w(w)
                                                                            .h_full()
                                                                            .flex_shrink_0()
                                                                            .px_1()
                                                                            .flex()
                                                                            .items_center()
                                                                            .child(heatmap_cell(
                                                                                mem_text.clone(),
                                                                                HeatmapLevel::from_permille(
                                                                                    row.memory_permille,
                                                                                ),
                                                                                &semantic,
                                                                            ))
                                                                            .into_any_element(),
                                                                        ProcessColumn::Disk => div()
                                                                            .w(w)
                                                                            .h_full()
                                                                            .flex_shrink_0()
                                                                            .px_1()
                                                                            .flex()
                                                                            .items_center()
                                                                            .child(heatmap_cell(
                                                                                disk_text.clone(),
                                                                                HeatmapLevel::from_bytes_per_sec(
                                                                                    row.disk_bytes_per_sec,
                                                                                ),
                                                                                &semantic,
                                                                            ))
                                                                            .into_any_element(),
                                                                        ProcessColumn::Network => div()
                                                                            .w(w)
                                                                            .h_full()
                                                                            .flex_shrink_0()
                                                                            .px_1()
                                                                            .flex()
                                                                            .items_center()
                                                                            .child(heatmap_cell(
                                                                                net_text.clone(),
                                                                                HeatmapLevel::from_bytes_per_sec(
                                                                                    row.net_bytes_per_sec,
                                                                                ),
                                                                                &semantic,
                                                                            ))
                                                                            .into_any_element(),
                                                                        ProcessColumn::Gpu => div()
                                                                            .w(w)
                                                                            .h_full()
                                                                            .flex_shrink_0()
                                                                            .px_1()
                                                                            .flex()
                                                                            .items_center()
                                                                            .child(heatmap_cell(
                                                                                gpu_text.clone(),
                                                                                HeatmapLevel::from_permille(
                                                                                    row.gpu_util_permille,
                                                                                ),
                                                                                &semantic,
                                                                            ))
                                                                            .into_any_element(),
                                                                        ProcessColumn::Publisher => {
                                                                            h_flex()
                                                                                .w(w)
                                                                                .h_full()
                                                                                .flex_shrink_0()
                                                                                .items_center()
                                                                                .px_2()
                                                                                .overflow_hidden()
                                                                                .child(
                                                                                    div()
                                                                                        .min_w_0()
                                                                                        .truncate()
                                                                                        .text_color(muted_fg)
                                                                                        .child(publisher_text.clone()),
                                                                                )
                                                                                .into_any_element()
                                                                        }
                                                                    }
                                                                });

                                                                Some(
                                                                    h_flex()
                                                                        .id(("proc-row", row_id))
                                                                        .role(gpui_kit::Role::ListItem)
                                                                        .aria_label(row_a11y_label)
                                                                        .aria_selected(is_selected)
                                                                        .w_full()
                                                                        .h(px(PROCESS_ROW_HEIGHT))
                                                                        .items_center()
                                                                        .px_2()
                                                                        .border_b_1()
                                                                        .border_color(row_border)
                                                                        .text_xs()
                                                                        .text_color(fg)
                                                                        .when(is_selected, |s| {
                                                                            s.bg(row_selected)
                                                                        })
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
                                                                        .children(cells)
                                                                        .context_menu(
                                                                            move |menu, window, cx| {
                                                                                build_row_context_menu(
                                                                                    menu,
                                                                                    menu_store.clone(),
                                                                                    menu_shell.clone(),
                                                                                    menu_row.clone(),
                                                                                    window,
                                                                                    cx,
                                                                                )
                                                                            },
                                                                        )
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
                                    ),
                                ),
                        ),
                )
                .vertical_scrollbar(&self.scroll_handle)
                .horizontal_scrollbar(&self.h_scroll_handle)
                .into_any_element()
        };

        attach_table_column_drag_handlers(
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
                    if no_modifiers
                        && event.keystroke.key == "delete"
                        && this.selected_key.is_some()
                    {
                        this.trigger_end_task_for_selected(window, cx);
                        cx.stop_propagation();
                    }
                }))
                .child(toolbar)
                .child(table_area),
            cx,
            |view: &mut Self| &mut view.column_layout,
            |view: &mut Self, cx| view.persist_column_layout(cx),
        )
    }
}
