//! Live-data Processes overview vertical feature slice.
//!
//! Splits the static toolbar shell ([`ProcessesView`]) from the high-frequency
//! 2D-virtualized `gpui-kit` `DataTable` (`Entity<TableState<ProcessesTableDelegate>>`)
//! so live snapshot ticks update only the table entity while `gpui-fast` reuses
//! the surrounding shell and toolbar as retained subtrees.

mod state;

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::assets::IconName as AssetIconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableEvent, TableState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, Size, StyledExt as _, h_flex,
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Empty, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, KeyDownEvent, MouseButton, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window, div, img,
    prelude::FluentBuilder as _, px,
};
use loom_types::{
    CapabilityFlags, LanguagePreference, PlatformProfile, ProcessCategory, ProcessKey,
    ProcessRowKind, ProcessRowProjection, ProcessSortColumn, ProcessStatusFlags, ProcessTableSlice,
    ProcessViewMode, ResourceDisplayMode, SortDirection,
};
use loom_ui_kit::{
    HeatmapLevel, LoomSemanticColors, StatusLabels, format_disk_cell_shared,
    format_memory_cell_shared, format_network_cell_shared, format_permille_shared, heatmap_cell,
    status_badges,
};

use crate::bridge::{UiCommands, UiStore, UiStoreEvent};
use crate::features::table_columns::{
    ColumnKey, ColumnSpec, ColumnToggleItem, DataTablePane, HeaderMenuAction, IconRenderCache,
    TableColumnLayout, build_column_header_menu, open_select_columns_dialog,
};
use crate::i18n;
use crate::shell::{ShellHandle, dialogs};

use self::state::{
    ResourceColumn, apply_resource_display_mode, can_terminate_process, find_selected_row,
    is_efficiency_mode_eligible, next_sort_direction, requires_end_task_confirmation,
    search_online_query, stable_row_id,
};

const PROCESS_HEADER_HEIGHT: f32 = 32.0;

/// Logical columns supported by [`ProcessesView`] (distinct from [`crate::features::details`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessColumn {
    Name,
    Category,
    Pid,
    ProcessName,
    Status,
    Cpu,
    Memory,
    Disk,
    Network,
    Gpu,
    Publisher,
    CommandLine,
}

impl ColumnKey for ProcessColumn {
    fn slug(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Category => "type",
            Self::Pid => "pid",
            Self::ProcessName => "process_name",
            Self::Status => "status",
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::Disk => "disk",
            Self::Network => "network",
            Self::Gpu => "gpu",
            Self::Publisher => "publisher",
            Self::CommandLine => "command_line",
        }
    }
}

impl ProcessColumn {
    /// Canonical toggleable column order presented in the **Select columns** dialog.
    const SELECTABLE_ORDER: [Self; 11] = [
        Self::Category,
        Self::Pid,
        Self::ProcessName,
        Self::Status,
        Self::Publisher,
        Self::Cpu,
        Self::Memory,
        Self::Disk,
        Self::Network,
        Self::Gpu,
        Self::CommandLine,
    ];

    fn default_specs() -> Vec<ColumnSpec<Self>> {
        vec![
            ColumnSpec::pinned(Self::Name, 260.0, 140.0, 720.0),
            ColumnSpec::hidden(Self::Category, 112.0, 72.0, 240.0),
            ColumnSpec::hidden(Self::Pid, 76.0, 56.0, 160.0),
            ColumnSpec::hidden(Self::ProcessName, 168.0, 96.0, 360.0),
            ColumnSpec::new(Self::Status, 80.0, 56.0, 200.0),
            ColumnSpec::new(Self::Cpu, 82.0, 60.0, 180.0),
            ColumnSpec::new(Self::Memory, 96.0, 68.0, 220.0),
            ColumnSpec::new(Self::Disk, 90.0, 68.0, 220.0),
            ColumnSpec::new(Self::Network, 92.0, 68.0, 220.0),
            ColumnSpec::new(Self::Gpu, 78.0, 60.0, 180.0),
            ColumnSpec::new(Self::Publisher, 176.0, 88.0, 480.0),
            ColumnSpec::hidden(Self::CommandLine, 320.0, 140.0, 1200.0),
        ]
    }

    fn label(self) -> String {
        match self {
            Self::Name => i18n::tr("processes.col_name"),
            Self::Category => i18n::tr("processes.col_type"),
            Self::Pid => i18n::tr("processes.col_pid"),
            Self::ProcessName => i18n::tr("processes.col_process_name"),
            Self::Status => i18n::tr("processes.col_status"),
            Self::Cpu => i18n::tr("processes.col_cpu"),
            Self::Memory => i18n::tr("processes.col_memory"),
            Self::Disk => i18n::tr("processes.col_disk"),
            Self::Network => i18n::tr("processes.col_network"),
            Self::Gpu => i18n::tr("processes.col_gpu"),
            Self::Publisher => i18n::tr("processes.col_publisher"),
            Self::CommandLine => i18n::tr("processes.col_command_line"),
        }
    }

    const fn sort_column(self) -> Option<ProcessSortColumn> {
        match self {
            Self::Name => Some(ProcessSortColumn::Name),
            Self::Category => Some(ProcessSortColumn::Category),
            Self::Pid => Some(ProcessSortColumn::Pid),
            Self::ProcessName => Some(ProcessSortColumn::ProcessName),
            Self::Status => Some(ProcessSortColumn::Status),
            Self::Cpu => Some(ProcessSortColumn::Cpu),
            Self::Memory => Some(ProcessSortColumn::Memory),
            Self::Disk => Some(ProcessSortColumn::Disk),
            Self::Network => Some(ProcessSortColumn::Network),
            Self::Gpu => Some(ProcessSortColumn::Gpu),
            Self::Publisher => Some(ProcessSortColumn::Publisher),
            Self::CommandLine => Some(ProcessSortColumn::CommandLine),
        }
    }

    const fn right_aligned(self) -> bool {
        matches!(
            self,
            Self::Pid | Self::Cpu | Self::Memory | Self::Disk | Self::Network | Self::Gpu
        )
    }

    const fn is_heatmap(self) -> bool {
        matches!(
            self,
            Self::Cpu | Self::Memory | Self::Disk | Self::Network | Self::Gpu
        )
    }
}

#[inline]
const fn to_column_sort(dir: SortDirection) -> ColumnSort {
    match dir {
        SortDirection::Ascending => ColumnSort::Ascending,
        SortDirection::Descending => ColumnSort::Descending,
    }
}

#[inline]
fn find_row_index(rows: &[ProcessRowProjection], key: Option<ProcessKey>) -> Option<usize> {
    let target = key?;
    rows.iter()
        .position(|row| row.kind == ProcessRowKind::Process && row.key == target)
}

/// Builds the row container returned by `TableDelegate::render_tr`.
///
/// Must not attach `.hover(...)` because `TableState::render_table_row` unconditionally
/// registers its own hover style on the returned element.
#[inline]
fn build_process_row_div(
    row: Option<(ProcessRowKind, ProcessKey)>,
    row_ix: usize,
    section_bg: gpui_kit::Hsla,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let Some((kind, key)) = row else {
        return div().id(("proc-empty", row_ix));
    };
    match kind {
        ProcessRowKind::SectionHeader { .. } => {
            div().id(("proc-section", row_ix)).bg(section_bg).text_xs()
        }
        ProcessRowKind::Process => div().id(("proc-row", stable_row_id(key))).text_xs(),
    }
}

/// Cached localized strings for [`ProcessesTableDelegate`] to avoid per-frame `i18n::tr` lookups.
struct ProcessesLocaleCache {
    status_labels: StatusLabels,
    cat_app: SharedString,
    cat_background: SharedString,
    cat_system: SharedString,
    empty_search: SharedString,
}

impl ProcessesLocaleCache {
    fn build(platform: PlatformProfile) -> Self {
        Self {
            status_labels: StatusLabels::new(
                i18n::tr("processes.status_efficiency"),
                i18n::tr("processes.status_suspended"),
                i18n::tr("processes.status_not_responding"),
            ),
            cat_app: i18n::process_category_label(ProcessCategory::App, platform).into(),
            cat_background: i18n::process_category_label(ProcessCategory::Background, platform)
                .into(),
            cat_system: i18n::process_category_label(ProcessCategory::System, platform).into(),
            empty_search: i18n::tr("processes.empty_search").into(),
        }
    }

    #[inline]
    fn category_label(&self, category: ProcessCategory) -> SharedString {
        match category {
            ProcessCategory::App => self.cat_app.clone(),
            ProcessCategory::Background => self.cat_background.clone(),
            ProcessCategory::System => self.cat_system.clone(),
        }
    }
}

/// 2D-virtualized `gpui-kit` [`TableDelegate`] backing the live Processes table.
struct ProcessesTableDelegate {
    store: Entity<UiStore>,
    shell: ShellHandle,
    slice: Arc<ProcessTableSlice>,
    platform: PlatformProfile,
    last_rev: u64,
    mem_mode: ResourceDisplayMode,
    disk_mode: ResourceDisplayMode,
    net_mode: ResourceDisplayMode,
    column_layout: TableColumnLayout<ProcessColumn>,
    selected_key: Option<ProcessKey>,
    icon_cache: IconRenderCache,
    locale: ProcessesLocaleCache,
}

impl ProcessesTableDelegate {
    fn new(store: Entity<UiStore>, shell: ShellHandle, cx: &App) -> Self {
        let store_ref = store.read(cx);
        let snapshot = store_ref.snapshot();
        let settings = store_ref.settings();
        let platform = snapshot.platform;

        let mut column_layout = TableColumnLayout::new(ProcessColumn::default_specs());
        let saved_cols = store_ref.processes_columns();
        if !saved_cols.is_empty() {
            column_layout.apply_saved(saved_cols);
        }

        Self {
            store: store.clone(),
            shell,
            slice: Arc::clone(&snapshot.processes),
            platform,
            last_rev: snapshot.revisions.processes,
            mem_mode: settings.memory_display_mode,
            disk_mode: settings.disk_display_mode,
            net_mode: settings.network_display_mode,
            column_layout,
            selected_key: None,
            icon_cache: IconRenderCache::new(),
            locale: ProcessesLocaleCache::build(platform),
        }
    }

    fn persist_column_layout(&self, cx: &mut App) {
        let snapshot = self.column_layout.snapshot();
        UiCommands::set_processes_columns(&self.store, snapshot, cx);
    }

    fn trigger_sort_col(&self, col_ix: usize, cx: &mut App) {
        let Some(spec) = self.column_layout.visible_column(col_ix) else {
            return;
        };
        let Some(target) = spec.id.sort_column() else {
            return;
        };
        let next_dir =
            next_sort_direction(self.slice.sort_column, self.slice.sort_direction, target);
        UiCommands::set_process_sort(&self.store, target, next_dir, cx);
    }

    fn hide_column(&mut self, col: ProcessColumn, cx: &mut Context<TableState<Self>>) -> bool {
        if self.column_layout.hide_column(col) {
            self.persist_column_layout(cx);
            true
        } else {
            false
        }
    }

    fn reset_columns(&mut self, cx: &mut Context<TableState<Self>>) {
        self.column_layout.reset(ProcessColumn::default_specs());
        UiCommands::set_processes_columns(&self.store, Vec::new(), cx);
    }

    fn open_select_columns_modal(
        &self,
        table_weak: WeakEntity<TableState<Self>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let items = ProcessColumn::SELECTABLE_ORDER
            .iter()
            .map(|&col| ColumnToggleItem {
                slug: col.slug(),
                label: col.label(),
                visible: self.column_layout.is_visible(col),
            })
            .collect();
        open_select_columns_dialog(
            items,
            Rc::new(move |selected_slugs, cx| {
                let _ = table_weak.update(cx, |table, cx| {
                    let changed = table
                        .delegate_mut()
                        .column_layout
                        .apply_visible_slugs(&selected_slugs);
                    if changed {
                        table.delegate().persist_column_layout(cx);
                        table.refresh(cx);
                    }
                });
            }),
            window,
            cx,
        );
    }

    fn build_header_context_menu(
        menu: PopupMenu,
        store: Entity<UiStore>,
        clicked_col: Option<ProcessColumn>,
        table_weak: WeakEntity<TableState<Self>>,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let hide_action: Option<HeaderMenuAction> = clicked_col
            .filter(|&col| col != ProcessColumn::Name)
            .map(|col| {
                let weak = table_weak.clone();
                Rc::new(move |_window: &mut Window, cx: &mut App| {
                    let _ = weak.update(cx, |table, cx| {
                        if table.delegate_mut().hide_column(col, cx) {
                            table.refresh(cx);
                        }
                    });
                }) as HeaderMenuAction
            });

        let weak_select = table_weak.clone();
        let on_select_columns: HeaderMenuAction =
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let _ = weak_select.update(cx, |table, cx| {
                    table
                        .delegate()
                        .open_select_columns_modal(weak_select.clone(), window, cx);
                });
            });

        let weak_reset = table_weak;
        let on_reset_defaults: HeaderMenuAction =
            Rc::new(move |_window: &mut Window, cx: &mut App| {
                let _ = weak_reset.update(cx, |table, cx| {
                    table.delegate_mut().reset_columns(cx);
                    table.refresh(cx);
                });
            });

        build_column_header_menu(
            menu,
            hide_action,
            on_select_columns,
            move |sub, window, cx| {
                sub.separator().submenu(
                    i18n::tr("processes.menu_resource_values"),
                    window,
                    cx,
                    move |nested, window, cx| {
                        build_resource_values_submenu(nested, store.clone(), window, cx)
                    },
                )
            },
            on_reset_defaults,
            window,
            cx,
        )
    }

    fn selected_end_task_enabled(&self) -> bool {
        find_selected_row(&self.slice.rows, self.selected_key).is_some_and(can_terminate_process)
    }
}

impl TableDelegate for ProcessesTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.column_layout.visible_count()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.slice.rows.len()
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        let Some(spec) = self.column_layout.visible_column(col_ix) else {
            return Column::default();
        };
        let col = spec.id;
        let mut column = Column::new(col.slug(), col.label())
            .width(px(spec.width))
            .min_width(px(spec.min_width))
            .max_width(px(spec.max_width))
            .selectable(false);

        if spec.locked {
            column = column.fixed_left().movable(false);
        }
        if col.right_aligned() {
            column = column.text_right();
        }
        if col.is_heatmap() {
            column = column.p_0();
        }
        if let Some(sort_col) = col.sort_column() {
            if sort_col == self.slice.sort_column {
                column = column.sort(to_column_sort(self.slice.sort_direction));
            } else {
                column = column.sortable();
            }
        }
        column
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        _sort: ColumnSort,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        self.trigger_sort_col(col_ix, cx);
    }

    fn move_column(
        &mut self,
        col_ix: usize,
        to_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        if self.column_layout.move_visible_column(col_ix, to_ix) {
            self.persist_column_layout(cx);
        }
    }

    fn render_header(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        let store = self.store.clone();
        let table_weak = cx.entity().downgrade();
        let bg = cx.theme().secondary.opacity(0.45);
        div()
            .id("processes-column-header")
            .relative()
            .bg(bg)
            .text_xs()
            .font_medium()
            .child(
                div()
                    .id("processes-header-bg-menu")
                    .absolute()
                    .inset_0()
                    .context_menu(move |menu, window, cx| {
                        Self::build_header_context_menu(
                            menu,
                            store.clone(),
                            None,
                            table_weak.clone(),
                            window,
                            cx,
                        )
                    }),
            )
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(spec) = self.column_layout.visible_column(col_ix) else {
            return Empty.into_any_element();
        };
        let col = spec.id;
        let is_active_sort = col.sort_column() == Some(self.slice.sort_column);
        let fg = cx.theme().foreground;
        let muted_fg = cx.theme().muted_foreground;
        let store = self.store.clone();
        let table_weak = cx.entity().downgrade();

        h_flex()
            .id(("proc-th", col_ix))
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .when(col.is_heatmap(), |d| d.px_2())
            .when(col.right_aligned(), |d| d.justify_end())
            .text_xs()
            .text_color(if is_active_sort { fg } else { muted_fg })
            .when(is_active_sort, |d| d.font_semibold())
            .cursor_pointer()
            .on_click(cx.listener(move |table, _, _, cx| {
                table.delegate().trigger_sort_col(col_ix, cx);
            }))
            .child(div().min_w_0().truncate().child(col.label()))
            .context_menu(move |menu, window, cx| {
                Self::build_header_context_menu(
                    menu,
                    store.clone(),
                    Some(col),
                    table_weak.clone(),
                    window,
                    cx,
                )
            })
            .into_any_element()
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        let section_bg = cx.theme().muted.opacity(0.6);
        let row = self.slice.rows.get(row_ix).map(|r| (r.kind, r.key));
        build_process_row_div(row, row_ix, section_bg)
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(row) = self.slice.rows.get(row_ix) else {
            return Empty.into_any_element();
        };
        let Some(spec) = self.column_layout.visible_column(col_ix) else {
            return Empty.into_any_element();
        };

        match row.kind {
            ProcessRowKind::SectionHeader { category, count } => {
                if col_ix == 0 {
                    let primary = cx.theme().primary;
                    let label = format!("{} ({count})", self.locale.category_label(category));
                    h_flex()
                        .size_full()
                        .items_center()
                        .font_semibold()
                        .text_xs()
                        .text_color(primary)
                        .child(label)
                        .into_any_element()
                } else {
                    Empty.into_any_element()
                }
            }
            ProcessRowKind::Process => {
                let theme = cx.theme();
                let muted_fg = theme.muted_foreground;
                let key = row.key;
                let row_id = stable_row_id(key);

                match spec.id {
                    ProcessColumn::Name => {
                        let indent = f32::from(row.depth.min(8)) * 16.0;
                        let has_children = row.has_children;
                        let is_expanded = row.is_expanded;
                        let is_selected = self.selected_key == Some(key);
                        let rendered_icon = row
                            .icon
                            .as_ref()
                            .and_then(|icon| self.icon_cache.get_or_insert(icon));
                        let display_name = SharedString::from(Arc::clone(&row.display_name));
                        let child_badge: Option<SharedString> =
                            (row.child_count > 0).then(|| format!("({})", row.child_count).into());
                        let toggle_store = self.store.clone();

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

                        h_flex()
                            .size_full()
                            .items_center()
                            .gap_1p5()
                            .pl(px(indent))
                            .overflow_hidden()
                            .child(if has_children {
                                div()
                                    .flex_shrink_0()
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation();
                                    })
                                    .child(
                                        Button::new(("proc-expand", row_id))
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
                                            }),
                                    )
                                    .into_any_element()
                            } else {
                                div().w(px(20.)).flex_shrink_0().into_any_element()
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
                                            .text_xs()
                                            .when(is_selected || has_children, |d| d.font_medium())
                                            .child(display_name),
                                    )
                                    .when_some(child_badge, |this, badge| {
                                        this.child(
                                            div()
                                                .flex_shrink_0()
                                                .text_xs()
                                                .text_color(muted_fg)
                                                .child(badge),
                                        )
                                    }),
                            )
                            .into_any_element()
                    }
                    ProcessColumn::Category => h_flex()
                        .size_full()
                        .items_center()
                        .overflow_hidden()
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(muted_fg)
                                .child(self.locale.category_label(row.category)),
                        )
                        .into_any_element(),
                    ProcessColumn::Pid => h_flex()
                        .size_full()
                        .items_center()
                        .justify_end()
                        .text_xs()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(muted_fg)
                        .child(key.pid.to_string())
                        .into_any_element(),
                    ProcessColumn::ProcessName => h_flex()
                        .size_full()
                        .items_center()
                        .overflow_hidden()
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .font_family(theme.mono_font_family.clone())
                                .text_color(muted_fg)
                                .child(SharedString::from(Arc::clone(&row.name))),
                        )
                        .into_any_element(),
                    ProcessColumn::Status => h_flex()
                        .size_full()
                        .items_center()
                        .overflow_hidden()
                        .child(status_badges(
                            row_id,
                            row.status,
                            &self.locale.status_labels,
                            &LoomSemanticColors::from_theme(theme),
                        ))
                        .into_any_element(),
                    ProcessColumn::Cpu => heatmap_cell(
                        format_permille_shared(row.cpu_permille),
                        HeatmapLevel::from_permille(row.cpu_permille),
                        &LoomSemanticColors::from_theme(theme),
                    )
                    .into_any_element(),
                    ProcessColumn::Memory => heatmap_cell(
                        format_memory_cell_shared(
                            row.memory_bytes,
                            row.memory_permille,
                            self.mem_mode,
                        ),
                        HeatmapLevel::from_permille(row.memory_permille),
                        &LoomSemanticColors::from_theme(theme),
                    )
                    .into_any_element(),
                    ProcessColumn::Disk => heatmap_cell(
                        format_disk_cell_shared(
                            row.disk_bytes_per_sec,
                            row.disk_permille,
                            self.disk_mode,
                        ),
                        HeatmapLevel::from_bytes_per_sec(row.disk_bytes_per_sec),
                        &LoomSemanticColors::from_theme(theme),
                    )
                    .into_any_element(),
                    ProcessColumn::Network => heatmap_cell(
                        format_network_cell_shared(
                            row.net_bytes_per_sec,
                            row.net_permille,
                            self.net_mode,
                        ),
                        HeatmapLevel::from_bytes_per_sec(row.net_bytes_per_sec),
                        &LoomSemanticColors::from_theme(theme),
                    )
                    .into_any_element(),
                    ProcessColumn::Gpu => heatmap_cell(
                        format_permille_shared(row.gpu_util_permille),
                        HeatmapLevel::from_permille(row.gpu_util_permille),
                        &LoomSemanticColors::from_theme(theme),
                    )
                    .into_any_element(),
                    ProcessColumn::Publisher => {
                        let publisher_text = row
                            .publisher
                            .as_ref()
                            .map(|p| SharedString::from(Arc::clone(p)))
                            .unwrap_or_default();
                        h_flex()
                            .size_full()
                            .items_center()
                            .overflow_hidden()
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(muted_fg)
                                    .child(publisher_text),
                            )
                            .into_any_element()
                    }
                    ProcessColumn::CommandLine => {
                        let cmd_arc = row
                            .command_line
                            .as_ref()
                            .filter(|s| !s.is_empty())
                            .map(Arc::clone);
                        let cmd_text = cmd_arc
                            .as_ref()
                            .map(|s| SharedString::from(Arc::clone(s)))
                            .unwrap_or_else(|| SharedString::new_static("—"));

                        h_flex()
                            .id(("proc-cmd", row_id))
                            .size_full()
                            .items_center()
                            .overflow_hidden()
                            .text_xs()
                            .font_family(theme.mono_font_family.clone())
                            .text_color(muted_fg)
                            .when_some(cmd_arc, |el, full_cmd| {
                                let tooltip_text = SharedString::from(full_cmd);
                                el.tooltip(move |window, cx| {
                                    Tooltip::new(tooltip_text.clone()).build(window, cx)
                                })
                            })
                            .child(div().min_w_0().truncate().child(cmd_text))
                            .into_any_element()
                    }
                }
            }
        }
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(row) = self.slice.rows.get(row_ix) else {
            return menu;
        };
        if row.kind != ProcessRowKind::Process {
            return menu;
        }
        let store = self.store.clone();
        let shell = self.shell.clone();
        let fallback_row = row.clone();
        build_row_context_menu(menu, store, shell, fallback_row, window, cx)
    }

    fn row_label(&self, row_ix: usize, _cx: &App) -> Option<SharedString> {
        let row = self.slice.rows.get(row_ix)?;
        match row.kind {
            ProcessRowKind::SectionHeader { category, count } => {
                Some(format!("{} ({count})", self.locale.category_label(category)).into())
            }
            ProcessRowKind::Process => {
                Some(format!("{} (PID {})", row.display_name, row.key.pid).into())
            }
        }
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let muted_fg = cx.theme().muted_foreground;
        v_flex()
            .size_full()
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
                    .child(self.locale.empty_search.clone()),
            )
    }
}

/// Live Processes overview view pairing a static toolbar with a 2D-virtualized `DataTable`.
///
/// Live metric snapshots update `self.table` directly without marking `ProcessesView`
/// dirty unless toolbar-visible state (`view_mode`, `end_task_enabled`, or locale) changes,
/// allowing `gpui-fast` to splice the table entity into the retained shell/toolbar frame.
pub(crate) struct ProcessesView {
    store: Entity<UiStore>,
    table: Entity<TableState<ProcessesTableDelegate>>,
    table_pane: Entity<DataTablePane<ProcessesTableDelegate>>,
    view_mode: ProcessViewMode,
    end_task_enabled: bool,
    last_locale: LanguagePreference,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for ProcessesView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.table.focus_handle(cx)
    }
}

impl ProcessesView {
    /// Creates a new [`ProcessesView`] backed by a 2D-virtualized `DataTable`.
    pub(crate) fn new(
        store: Entity<UiStore>,
        shell: ShellHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (view_mode, last_locale) = {
            let s = store.read(cx);
            (s.snapshot().processes.view_mode, s.settings().language)
        };

        let table = cx.new(|cx| {
            let delegate = ProcessesTableDelegate::new(store.clone(), shell, cx);
            TableState::new(delegate, window, cx)
                .row_selectable(true)
                .col_selectable(false)
                .col_resizable(true)
                .col_movable(true)
                .sortable(true)
        });
        let table_pane = cx.new(|_| {
            DataTablePane::new(
                table.clone(),
                Size::Medium,
                px(PROCESS_HEADER_HEIGHT),
                false,
                i18n::tr("processes.title"),
            )
        });

        let mut subscriptions = Vec::with_capacity(2);

        // Subscribe to UiStore: push live snapshots straight to `self.table`; notify outer
        // `ProcessesView` ONLY when toolbar-visible state or locale changed.
        let table_for_store = table.clone();
        subscriptions.push(
            cx.subscribe(&store, move |this, store, event, cx| match event {
                UiStoreEvent::SnapshotUpdated(revs) => {
                    let current_rev = table_for_store.read(cx).delegate().last_rev;
                    if revs.processes == current_rev {
                        return;
                    }
                    let (next_slice, next_platform) = {
                        let snap = store.read(cx).snapshot();
                        (Arc::clone(&snap.processes), snap.platform)
                    };
                    let (next_view_mode, next_end_enabled) =
                        table_for_store.update(cx, |table, cx| {
                            let d = table.delegate_mut();
                            let sort_changed = d.slice.sort_column != next_slice.sort_column
                                || d.slice.sort_direction != next_slice.sort_direction;
                            d.slice = next_slice;
                            d.platform = next_platform;
                            d.last_rev = revs.processes;

                            let view_mode = d.slice.view_mode;
                            let selected_idx = find_row_index(&d.slice.rows, d.selected_key);
                            let end_enabled = d.selected_end_task_enabled();

                            // Re-anchor selection index to ProcessKey without scrolling.
                            if let Some(idx) = selected_idx {
                                if table.selected_row() != Some(idx) {
                                    table.set_selected_row(idx, cx);
                                }
                            } else if table.selected_row().is_some() {
                                table.clear_selection(cx);
                            }

                            if sort_changed {
                                table.refresh(cx);
                            }
                            cx.notify();
                            (view_mode, end_enabled)
                        });

                    if this.view_mode != next_view_mode || this.end_task_enabled != next_end_enabled
                    {
                        this.view_mode = next_view_mode;
                        this.end_task_enabled = next_end_enabled;
                        cx.notify();
                    }
                }
                UiStoreEvent::SettingsChanged => {
                    let (next_locale, mem_mode, disk_mode, net_mode) = {
                        let s = store.read(cx).settings();
                        (
                            s.language,
                            s.memory_display_mode,
                            s.disk_display_mode,
                            s.network_display_mode,
                        )
                    };
                    let locale_changed = this.last_locale != next_locale;
                    this.last_locale = next_locale;

                    table_for_store.update(cx, |table, cx| {
                        let d = table.delegate_mut();
                        let mode_changed = d.mem_mode != mem_mode
                            || d.disk_mode != disk_mode
                            || d.net_mode != net_mode;
                        d.mem_mode = mem_mode;
                        d.disk_mode = disk_mode;
                        d.net_mode = net_mode;
                        if locale_changed {
                            let platform = d.platform;
                            d.locale = ProcessesLocaleCache::build(platform);
                            table.refresh(cx);
                        }
                        if locale_changed || mode_changed {
                            cx.notify();
                        }
                    });

                    if locale_changed {
                        this.table_pane.update(cx, |pane, cx| {
                            pane.set_accessibility_label(i18n::tr("processes.title"), cx);
                        });
                        cx.notify();
                    }
                }
                UiStoreEvent::ElevationRequired { .. } | UiStoreEvent::CommandFailed { .. } => {}
            }),
        );

        // Subscribe to DataTable events: ProcessKey selection sync and column width persistence.
        subscriptions.push(cx.subscribe(
            &table,
            |this, table, event: &TableEvent, cx| match event {
                TableEvent::SelectRow(row_ix) => {
                    let next_end_enabled = table.update(cx, |table, _| {
                        let d = table.delegate_mut();
                        match d.slice.rows.get(*row_ix) {
                            Some(row) if row.kind == ProcessRowKind::Process => {
                                d.selected_key = Some(row.key);
                            }
                            _ => {
                                d.selected_key = None;
                            }
                        }
                        d.selected_end_task_enabled()
                    });
                    if this.end_task_enabled != next_end_enabled {
                        this.end_task_enabled = next_end_enabled;
                        cx.notify();
                    }
                }
                TableEvent::RightClickedRow(Some(row_ix)) => {
                    let next_end_enabled = table.update(cx, |table, cx| {
                        let key_opt = table
                            .delegate()
                            .slice
                            .rows
                            .get(*row_ix)
                            .filter(|r| r.kind == ProcessRowKind::Process)
                            .map(|r| r.key);
                        table.delegate_mut().selected_key = key_opt;
                        if key_opt.is_some() && table.selected_row() != Some(*row_ix) {
                            table.set_selected_row(*row_ix, cx);
                        }
                        table.delegate().selected_end_task_enabled()
                    });
                    if this.end_task_enabled != next_end_enabled {
                        this.end_task_enabled = next_end_enabled;
                        cx.notify();
                    }
                }
                TableEvent::ClearSelection => {
                    table.update(cx, |table, _| {
                        table.delegate_mut().selected_key = None;
                    });
                    if this.end_task_enabled {
                        this.end_task_enabled = false;
                        cx.notify();
                    }
                }
                TableEvent::ColumnWidthsChanged(widths) => {
                    table.update(cx, |table, cx| {
                        if table
                            .delegate_mut()
                            .column_layout
                            .apply_visible_widths(widths)
                        {
                            table.delegate().persist_column_layout(cx);
                        }
                    });
                }
                _ => {}
            },
        ));

        Self {
            store,
            table,
            table_pane,
            view_mode,
            end_task_enabled: false,
            last_locale,
            _subscriptions: subscriptions,
        }
    }

    fn trigger_end_task_for_selected(&self, window: &mut Window, cx: &mut Context<Self>) {
        let selected_key = self.table.read(cx).delegate().selected_key;
        let slice = Arc::clone(&self.table.read(cx).delegate().slice);
        let Some(row) = find_selected_row(&slice.rows, selected_key) else {
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
    cx: &mut Context<TableState<ProcessesTableDelegate>>,
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
    let res_submenu = PopupMenu::build(window, cx, move |sub, window, cx| {
        build_resource_values_submenu(sub, store_res.clone(), window, cx)
    });
    menu = menu
        .separator()
        .item(PopupMenuItem::submenu(
            i18n::tr("processes.menu_resource_values"),
            res_submenu,
        ))
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
        let view_mode = self.view_mode;
        let end_task_enabled = self.end_task_enabled;

        let theme = cx.theme();
        let border = theme.border;
        let fg = theme.foreground;
        let muted_bg = theme.muted;
        let radius = theme.radius;

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

        v_flex()
            .id("processes-view-root")
            .size_full()
            .min_h_0()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let no_modifiers = !event.keystroke.modifiers.control
                    && !event.keystroke.modifiers.alt
                    && !event.keystroke.modifiers.platform
                    && !event.keystroke.modifiers.shift;
                if no_modifiers && event.keystroke.key == "delete" && this.end_task_enabled {
                    this.trigger_end_task_for_selected(window, cx);
                    cx.stop_propagation();
                }
            }))
            .child(toolbar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.table_pane.clone()),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui_kit::component::table::{Column, TableDelegate, TableEvent, TableState};
    use gpui_kit::{
        App, AppContext as _, Context, Entity, IntoElement, Modifiers, MouseButton,
        ParentElement as _, Render, SharedString, Styled as _, TestAppContext, VisualTestContext,
        Window, div, point, px, size,
    };

    use super::{ProcessColumn, build_process_row_div};
    use crate::features::table_columns::{ColumnKey as _, DataTablePane, TableColumnLayout};
    use gpui_kit::component::ActiveTheme as _;
    use loom_types::{ProcessCategory, ProcessKey, ProcessRowKind};

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

        // Extract the inner SplitToolbarHarness's table entity via Root.
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

        // Simulate 10 live metric snapshot ticks directed solely at `table`.
        for tick in 1..=10u16 {
            table.update(&mut vcx, |table, cx| {
                table.delegate_mut().rows[0].1 = tick * 10;
                cx.notify();
            });
            vcx.update(|window, cx| window.draw(cx).clear(cx));
        }

        // Outer toolbar harness must not have been forced to re-render by live table ticks.
        assert_eq!(
            outer_renders.get(),
            renders_after_init,
            "live table ticks must not re-render the static outer toolbar"
        );

        // Clicking a row selects it cleanly inside DataTable.
        vcx.simulate_click(point(px(100.), px(90.)), Modifiers::default());
        vcx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(
            table.read_with(&vcx, |t, _| t.selected_row().is_some()),
            "clicking a data row must select it in DataTable"
        );

        // Right-clicking a row records right_clicked_row without panicking.
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
}
