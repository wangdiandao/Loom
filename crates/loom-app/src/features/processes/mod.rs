//! Live-data Processes overview vertical feature slice.
//!
//! Splits the static toolbar shell ([`ProcessesView`]) from the high-frequency
//! 2D-virtualized `gpui-kit` `DataTable` (`Entity<TableState<ProcessesTableDelegate>>`)
//! so live snapshot ticks update only the table entity while `gpui-fast` reuses
//! the surrounding shell and toolbar as retained subtrees.

mod state;

use std::ops::Range;
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
    App, AppContext as _, Bounds, ClipboardItem, Context, Empty, Entity, EntityInputHandler,
    FocusHandle, Focusable, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton,
    ParentElement as _, Pixels, Point, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, UTF16Selection, WeakEntity, Window, div, img,
    prelude::FluentBuilder as _, px,
};
use loom_engine::{EngineCommand, EngineCoordinator};
use loom_pal::NativeBackend;
use loom_types::{
    CapabilityFlags, LanguagePreference, MonitorDomain, PlatformProfile, ProcessCategory,
    ProcessKey, ProcessRowKind, ProcessRowProjection, ProcessSortColumn, ProcessStatusFlags,
    ProcessTableSlice, ProcessViewMode, ResourceDisplayMode, SortDirection, WebView2Role,
};
use loom_ui_kit::{
    HeatmapLevel, LoomSemanticColors, StatusLabels, format_disk_cell, format_disk_cell_shared,
    format_memory_cell, format_memory_cell_shared, format_network_cell, format_network_cell_shared,
    format_permille, format_permille_shared, heatmap_cell, status_badges,
};

use crate::bridge::{ScopedMonitorLease, UiCommands, UiStore, UiStoreEvent};
use crate::features::table_columns::{
    ColumnKey, ColumnSpec, ColumnToggleItem, CopyRowFields, DataTablePane, HeaderMenuAction,
    IconRenderCache, OrderFreezeState, TableColumnLayout, TableInputBridge, TypeToSearchState,
    build_column_header_menu, build_copy_submenu, can_suspend_or_resume_process,
    dispatch_hide_system_processes, dispatch_ignore_leak_path, dispatch_restart_explorer,
    dispatch_suspend_or_resume, is_explorer_image, leak_ignore_target, leak_warning_badge,
    open_select_columns_dialog, sort_paused_indicator, spawn_csv_export, webview2_role_badge,
    webview2_role_label,
};
use crate::i18n;
use crate::shell::{ShellHandle, dialogs};

use self::state::{
    ResourceColumn, apply_resource_display_mode, can_terminate_process, find_selected_row,
    format_process_copy_short, format_process_row_summary, format_process_status_text,
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
    status_elevated: SharedString,
    status_running: SharedString,
    cat_app: SharedString,
    cat_background: SharedString,
    cat_system: SharedString,
    empty_search: SharedString,
    wv2_browser: SharedString,
    wv2_renderer: SharedString,
    wv2_gpu: SharedString,
    wv2_network: SharedString,
    wv2_storage: SharedString,
    wv2_audio: SharedString,
    wv2_utility: SharedString,
    wv2_crashpad: SharedString,
}

impl ProcessesLocaleCache {
    fn build(platform: PlatformProfile) -> Self {
        Self {
            status_labels: StatusLabels::new(
                i18n::tr("processes.status_efficiency"),
                i18n::tr("processes.status_suspended"),
                i18n::tr("processes.status_not_responding"),
            ),
            status_elevated: i18n::tr("processes.status_elevated").into(),
            status_running: i18n::tr("details.status_running").into(),
            cat_app: i18n::process_category_label(ProcessCategory::App, platform).into(),
            cat_background: i18n::process_category_label(ProcessCategory::Background, platform)
                .into(),
            cat_system: i18n::process_category_label(ProcessCategory::System, platform).into(),
            empty_search: i18n::tr("processes.empty_search").into(),
            wv2_browser: webview2_role_label(WebView2Role::Browser).into(),
            wv2_renderer: webview2_role_label(WebView2Role::Renderer).into(),
            wv2_gpu: webview2_role_label(WebView2Role::Gpu).into(),
            wv2_network: webview2_role_label(WebView2Role::Network).into(),
            wv2_storage: webview2_role_label(WebView2Role::Storage).into(),
            wv2_audio: webview2_role_label(WebView2Role::Audio).into(),
            wv2_utility: webview2_role_label(WebView2Role::Utility).into(),
            wv2_crashpad: webview2_role_label(WebView2Role::Crashpad).into(),
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

    #[inline]
    fn webview2_label(&self, role: WebView2Role) -> SharedString {
        match role {
            WebView2Role::Browser => self.wv2_browser.clone(),
            WebView2Role::Renderer => self.wv2_renderer.clone(),
            WebView2Role::Gpu => self.wv2_gpu.clone(),
            WebView2Role::Network => self.wv2_network.clone(),
            WebView2Role::Storage => self.wv2_storage.clone(),
            WebView2Role::Audio => self.wv2_audio.clone(),
            WebView2Role::Utility => self.wv2_utility.clone(),
            WebView2Role::Crashpad => self.wv2_crashpad.clone(),
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
    last_right_clicked_col: Option<usize>,
    order_freeze: OrderFreezeState,
    menu_focus_subs: Vec<Subscription>,
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
            last_right_clicked_col: None,
            order_freeze: OrderFreezeState::default(),
            menu_focus_subs: Vec::new(),
            icon_cache: IconRenderCache::new(),
            locale: ProcessesLocaleCache::build(platform),
        }
    }

    fn apply_order_freeze_transition(
        &mut self,
        transition: Option<bool>,
        cx: &mut Context<TableState<Self>>,
    ) {
        if let Some(frozen) = transition {
            let _ = self
                .store
                .read(cx)
                .dispatch_command(EngineCommand::SetProcessOrderFreeze(frozen));
            cx.notify();
        }
    }

    fn set_table_hovered(&mut self, hovered: bool, cx: &mut Context<TableState<Self>>) {
        let transition = self.order_freeze.set_table_hovered(hovered);
        self.apply_order_freeze_transition(transition, cx);
    }

    fn set_header_hovered(&mut self, hovered: bool, cx: &mut Context<TableState<Self>>) {
        let transition = self.order_freeze.set_header_hovered(hovered);
        self.apply_order_freeze_transition(transition, cx);
    }

    fn set_context_menu_open(&mut self, open: bool, cx: &mut Context<TableState<Self>>) {
        let transition = self.order_freeze.set_context_menu_open(open);
        self.apply_order_freeze_transition(transition, cx);
    }

    fn format_cell_text(&self, row: &ProcessRowProjection, col: ProcessColumn) -> String {
        match col {
            ProcessColumn::Name => row.display_name.to_string(),
            ProcessColumn::Category => self.locale.category_label(row.category).to_string(),
            ProcessColumn::Pid => row.key.pid.to_string(),
            ProcessColumn::ProcessName => row.name.to_string(),
            ProcessColumn::Status => format_process_status_text(
                row.status,
                &self.locale.status_labels,
                self.locale.status_elevated.as_ref(),
                self.locale.status_running.as_ref(),
            ),
            ProcessColumn::Cpu => format_permille(row.cpu_permille),
            ProcessColumn::Memory => {
                format_memory_cell(row.memory_bytes, row.memory_permille, self.mem_mode)
            }
            ProcessColumn::Disk => {
                format_disk_cell(row.disk_bytes_per_sec, row.disk_permille, self.disk_mode)
            }
            ProcessColumn::Network => {
                format_network_cell(row.net_bytes_per_sec, row.net_permille, self.net_mode)
            }
            ProcessColumn::Gpu => format_permille(row.gpu_util_permille),
            ProcessColumn::Publisher => row
                .publisher
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("")
                .to_owned(),
            ProcessColumn::CommandLine => row
                .command_line
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("")
                .to_owned(),
        }
    }

    fn format_row_summary(&self, row: &ProcessRowProjection) -> String {
        let status_text = self.format_cell_text(row, ProcessColumn::Status);
        format_process_row_summary(
            row,
            self.mem_mode,
            self.disk_mode,
            self.net_mode,
            &status_text,
        )
    }

    fn selected_copy_short(&self) -> Option<String> {
        let row = find_selected_row(&self.slice.rows, self.selected_key)?;
        Some(format_process_copy_short(row))
    }

    fn selected_copy_summary(&self) -> Option<String> {
        let row = find_selected_row(&self.slice.rows, self.selected_key)?;
        Some(self.format_row_summary(row))
    }

    fn export_visible_csv_payload(&self) -> (Vec<String>, Vec<Vec<String>>) {
        let visible_cols = self.column_layout.visible_columns();
        let headers: Vec<String> = visible_cols.iter().map(|spec| spec.id.label()).collect();
        let rows: Vec<Vec<String>> = self
            .slice
            .rows
            .iter()
            .filter(|row| row.kind == ProcessRowKind::Process)
            .map(|row| {
                visible_cols
                    .iter()
                    .map(|spec| self.format_cell_text(row, spec.id))
                    .collect()
            })
            .collect();
        (headers, rows)
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
            .on_hover(cx.listener(|table, hovered: &bool, _, cx| {
                table.delegate_mut().set_header_hovered(*hovered, cx);
            }))
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
        let is_sort_paused = is_active_sort && self.order_freeze.is_frozen();
        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let sort_paused_color = LoomSemanticColors::from_theme(theme).sort_paused;
        let store = self.store.clone();
        let table_weak = cx.entity().downgrade();

        h_flex()
            .id(("proc-th", col_ix))
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .gap_1()
            .when(col.is_heatmap(), |d| d.px_2())
            .when(col.right_aligned(), |d| d.justify_end())
            .text_xs()
            .text_color(if is_active_sort { fg } else { muted_fg })
            .when(is_active_sort, |d| d.font_semibold())
            .cursor_pointer()
            .on_click(cx.listener(move |table, _, _, cx| {
                table.delegate().trigger_sort_col(col_ix, cx);
            }))
            .when(is_sort_paused && col.right_aligned(), |d| {
                d.child(sort_paused_indicator(
                    ("proc-sort-paused", col_ix),
                    sort_paused_color,
                ))
            })
            .child(div().min_w_0().truncate().child(col.label()))
            .when(is_sort_paused && !col.right_aligned(), |d| {
                d.child(sort_paused_indicator(
                    ("proc-sort-paused", col_ix),
                    sort_paused_color,
                ))
            })
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
                let muted_bg = theme.muted;
                let radius = theme.radius;
                let semantic = LoomSemanticColors::from_theme(theme);
                let key = row.key;
                let row_id = stable_row_id(key);

                let cell_content = match spec.id {
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
                        let wv2_label = row
                            .webview2_role
                            .map(|role| self.locale.webview2_label(role));
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
                                    })
                                    .when_some(wv2_label, |this, role_label| {
                                        this.child(webview2_role_badge(
                                            role_label, muted_bg, muted_fg, radius,
                                        ))
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
                        .gap_1()
                        .overflow_hidden()
                        .child(status_badges(
                            row_id,
                            row.status,
                            &self.locale.status_labels,
                            &semantic,
                        ))
                        .when_some(row.leak_warning.as_ref(), |el, warning| {
                            el.child(leak_warning_badge(
                                ("proc-leak", row_id),
                                warning,
                                semantic.leak_warning,
                            ))
                        })
                        .into_any_element(),
                    ProcessColumn::Cpu => heatmap_cell(
                        format_permille_shared(row.cpu_permille),
                        HeatmapLevel::from_permille(row.cpu_permille),
                        &semantic,
                    )
                    .into_any_element(),
                    ProcessColumn::Memory => heatmap_cell(
                        format_memory_cell_shared(
                            row.memory_bytes,
                            row.memory_permille,
                            self.mem_mode,
                        ),
                        HeatmapLevel::from_permille(row.memory_permille),
                        &semantic,
                    )
                    .into_any_element(),
                    ProcessColumn::Disk => heatmap_cell(
                        format_disk_cell_shared(
                            row.disk_bytes_per_sec,
                            row.disk_permille,
                            self.disk_mode,
                        ),
                        HeatmapLevel::from_bytes_per_sec(row.disk_bytes_per_sec),
                        &semantic,
                    )
                    .into_any_element(),
                    ProcessColumn::Network => heatmap_cell(
                        format_network_cell_shared(
                            row.net_bytes_per_sec,
                            row.net_permille,
                            self.net_mode,
                        ),
                        HeatmapLevel::from_bytes_per_sec(row.net_bytes_per_sec),
                        &semantic,
                    )
                    .into_any_element(),
                    ProcessColumn::Gpu => heatmap_cell(
                        format_permille_shared(row.gpu_util_permille),
                        HeatmapLevel::from_permille(row.gpu_util_permille),
                        &semantic,
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
                };

                div()
                    .size_full()
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |table, _, _, _| {
                            table.delegate_mut().last_right_clicked_col = Some(col_ix);
                        }),
                    )
                    .child(cell_content)
                    .into_any_element()
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
        let Some(row) = self.slice.rows.get(row_ix).cloned() else {
            return menu;
        };
        if row.kind != ProcessRowKind::Process {
            return menu;
        }
        let clicked_column = self
            .last_right_clicked_col
            .and_then(|col_ix| self.column_layout.visible_column(col_ix))
            .map(|spec| (spec.id.label(), self.format_cell_text(&row, spec.id)));
        let copy_fields = CopyRowFields {
            name: if !row.name.trim().is_empty() {
                row.name.to_string()
            } else {
                row.display_name.to_string()
            },
            pid: row.key.pid,
            exe_path: row.exe_path.as_deref().map(str::to_owned),
            command_line: row.command_line.as_deref().map(str::to_owned),
            row_summary: self.format_row_summary(&row),
            clicked_column,
        };

        self.set_context_menu_open(true, cx);

        let store = self.store.clone();
        let shell = self.shell.clone();
        let root_focus = menu.focus_handle(cx);
        let (built_menu, submenu_handles) =
            build_row_context_menu(menu, store, shell, row, copy_fields, window, cx);

        let mut tracked_handles = Vec::with_capacity(1 + submenu_handles.len());
        tracked_handles.push(root_focus);
        tracked_handles.extend(submenu_handles);
        let tracked = Rc::new(tracked_handles);

        self.menu_focus_subs.clear();
        for handle in tracked.iter() {
            let all_handles = Rc::clone(&tracked);
            let sub = cx.on_focus_out(handle, window, move |table, _, window, cx| {
                let still_in_menu = all_handles
                    .iter()
                    .any(|h| h.is_focused(window) || h.contains_focused(window, cx));
                if !still_in_menu {
                    table.delegate_mut().set_context_menu_open(false, cx);
                }
            });
            self.menu_focus_subs.push(sub);
        }

        built_menu
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
    coordinator: Arc<EngineCoordinator<NativeBackend>>,
    table: Entity<TableState<ProcessesTableDelegate>>,
    table_pane: Entity<DataTablePane<ProcessesTableDelegate>>,
    process_net_lease: Option<ScopedMonitorLease>,
    view_mode: ProcessViewMode,
    hide_system_processes: bool,
    end_task_enabled: bool,
    last_locale: LanguagePreference,
    type_to_search: TypeToSearchState,
    _subscriptions: Vec<Subscription>,
}

impl Drop for ProcessesView {
    fn drop(&mut self) {
        let _ = self
            .coordinator
            .send_command(EngineCommand::SetProcessOrderFreeze(false));
    }
}

impl Focusable for ProcessesView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.table.focus_handle(cx)
    }
}

impl EntityInputHandler for ProcessesView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let query = self.store.read(cx).search_query().to_owned();
        self.type_to_search.sync_committed(&query);
        let (adj, text) = self.type_to_search.text_for_utf16_range(range);
        adjusted_range.replace(adj);
        Some(text)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let query = self.store.read(cx).search_query().to_owned();
        self.type_to_search.sync_committed(&query);
        Some(UTF16Selection {
            range: self.type_to_search.selected_utf16_range(),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.type_to_search.marked_utf16_range()
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.type_to_search.is_composing() {
            self.type_to_search.unmark_text();
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_composing = self.type_to_search.is_composing();
        let current = self.store.read(cx).search_query().to_owned();
        if let Some(next_query) = self.type_to_search.replace_text(range, text, &current) {
            UiCommands::set_search_query(&self.store, next_query, cx);
        }
        if was_composing {
            cx.notify();
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.store.read(cx).search_query().to_owned();
        self.type_to_search
            .replace_and_mark(range, new_text, &current);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(Bounds::new(
            element_bounds.origin,
            gpui_kit::size(px(1.0), px(20.0)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.type_to_search.utf16_len())
    }

    fn text_length_utf16(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Option<usize> {
        let query = self.store.read(cx).search_query().to_owned();
        self.type_to_search.sync_committed(&query);
        Some(self.type_to_search.utf16_len())
    }

    fn accepts_text_input(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.table.focus_handle(cx).is_focused(window)
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
        let (coordinator, view_mode, hide_system_processes, last_locale, initial_query) = {
            let s = store.read(cx);
            (
                Arc::clone(s.coordinator()),
                s.snapshot().processes.view_mode,
                s.settings().hide_windows_system_processes,
                s.settings().language,
                s.search_query().to_owned(),
            )
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
                    this.sync_process_network_lease(cx);

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
                    let (next_locale, mem_mode, disk_mode, net_mode, hide_sys) = {
                        let s = store.read(cx).settings();
                        (
                            s.language,
                            s.memory_display_mode,
                            s.disk_display_mode,
                            s.network_display_mode,
                            s.hide_windows_system_processes,
                        )
                    };
                    let locale_changed = this.last_locale != next_locale;
                    let hide_sys_changed = this.hide_system_processes != hide_sys;
                    this.last_locale = next_locale;
                    this.hide_system_processes = hide_sys;

                    this.sync_process_network_lease(cx);

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
                    }
                    if locale_changed || hide_sys_changed {
                        cx.notify();
                    }
                }
                UiStoreEvent::SearchQueryChanged => {
                    let query = store.read(cx).search_query().to_owned();
                    this.type_to_search.sync_committed(&query);
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

        let mut type_to_search = TypeToSearchState::default();
        type_to_search.sync_committed(&initial_query);

        let mut view = Self {
            store,
            coordinator,
            table,
            table_pane,
            process_net_lease: None,
            view_mode,
            hide_system_processes,
            end_task_enabled: false,
            last_locale,
            type_to_search,
            _subscriptions: subscriptions,
        };
        view.sync_process_network_lease(cx);
        view
    }

    /// Acquires or releases `MonitorDomain::ProcessNetwork` strictly when the Network column is
    /// visible, the backend advertises `ELEVATED_PROCESS_NET`, and Loom is running elevated.
    fn sync_process_network_lease(&mut self, cx: &App) {
        let should_acquire = {
            let store_ref = self.store.read(cx);
            let snap = store_ref.snapshot();
            let network_visible = self
                .table
                .read(cx)
                .delegate()
                .column_layout
                .is_visible(ProcessColumn::Network);
            network_visible
                && snap.is_elevated
                && snap
                    .capabilities
                    .contains(CapabilityFlags::ELEVATED_PROCESS_NET)
        };
        if should_acquire {
            if self.process_net_lease.is_none() {
                self.process_net_lease = Some(
                    self.store
                        .read(cx)
                        .acquire_lease(MonitorDomain::ProcessNetwork),
                );
            }
        } else {
            self.process_net_lease = None;
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

    fn trigger_export_csv(&self, window: &mut Window, cx: &mut Context<Self>) {
        let (headers, rows) = self.table.read(cx).delegate().export_visible_csv_payload();
        spawn_csv_export(
            self.store.clone(),
            "loom-processes",
            headers,
            rows,
            window,
            cx,
        );
    }

    fn copy_selected_short(&self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.table.read(cx).delegate().selected_copy_short() else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    fn copy_selected_summary(&self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.table.read(cx).delegate().selected_copy_summary() else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
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
    copy_fields: CopyRowFields,
    window: &mut Window,
    cx: &mut Context<TableState<ProcessesTableDelegate>>,
) -> (PopupMenu, Vec<FocusHandle>) {
    let key = fallback_row.key;
    let snapshot = store.read(cx).snapshot().clone();
    let caps = snapshot.capabilities;
    let eco_supported = caps.contains(CapabilityFlags::ECO_MODE);
    let suspend_supported = caps.contains(CapabilityFlags::SUSPEND_RESUME);
    let minidump_supported = caps.contains(CapabilityFlags::MINIDUMP);
    let debugger_supported = caps.contains(CapabilityFlags::DEBUGGER_ATTACH);
    let row = find_selected_row(&snapshot.processes.rows, Some(key))
        .cloned()
        .unwrap_or(fallback_row);

    let has_children = row.has_children;
    let is_expanded = row.is_expanded;
    let can_end = can_terminate_process(&row);
    let is_kernel_or_idle = row
        .status
        .intersects(ProcessStatusFlags::SYSTEM_IDLE | ProcessStatusFlags::KERNEL);
    let eco_eligible = is_efficiency_mode_eligible(&row, caps);
    let eco_active = row.status.contains(ProcessStatusFlags::EFFICIENCY_MODE);
    let is_suspended = row.status.contains(ProcessStatusFlags::SUSPENDED);
    let suspend_eligible = can_suspend_or_resume_process(key.pid, &row.name, row.status, caps);
    let is_explorer = is_explorer_image(&row.name);
    let leak_ignore = row
        .leak_warning
        .as_ref()
        .and_then(|_| leak_ignore_target(row.exe_path.as_deref(), &row.name));
    let exe_path = row.exe_path.clone();
    let query = search_online_query(&row);
    let mut submenu_handles = Vec::with_capacity(2);

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

    if suspend_supported {
        let store_suspend = store.clone();
        let (label, icon) = if is_suspended {
            (i18n::tr("proc_ux_menu_resume"), IconName::Play)
        } else {
            (i18n::tr("proc_ux_menu_suspend"), IconName::Pause)
        };
        menu = menu.item(
            PopupMenuItem::new(label)
                .icon(icon)
                .disabled(!suspend_eligible)
                .on_click(move |_, _, cx| {
                    dispatch_suspend_or_resume(&store_suspend, key, is_suspended, cx);
                }),
        );

        if is_explorer {
            let store_restart = store.clone();
            menu = menu.item(
                PopupMenuItem::new(i18n::tr("proc_ux_menu_restart_explorer"))
                    .icon(Icon::new(AssetIconName::RotateCw))
                    .on_click(move |_, _, cx| {
                        dispatch_restart_explorer(&store_restart, cx);
                    }),
            );
        }
    }

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

    if let Some(ignore_target) = leak_ignore {
        let store_leak = store.clone();
        menu = menu.item(
            PopupMenuItem::new(i18n::tr("leak_warning_ignore_menu"))
                .icon(IconName::TriangleAlert)
                .on_click(move |_, _, cx| {
                    dispatch_ignore_leak_path(&store_leak, ignore_target.clone(), cx);
                }),
        );
    }

    if minidump_supported || debugger_supported {
        menu = menu.separator();
        if minidump_supported {
            let store_dump = store.clone();
            let dump_name = row.name.to_string();
            menu = menu.item(
                PopupMenuItem::new(i18n::tr("processes.menu_create_dump"))
                    .icon(IconName::File)
                    .disabled(is_kernel_or_idle)
                    .on_click(move |_, window, cx| {
                        dialogs::open_minidump_dialog(
                            store_dump.clone(),
                            key,
                            dump_name.clone(),
                            window,
                            cx,
                        );
                    }),
            );
        }
        if debugger_supported {
            let store_dbg = store.clone();
            menu = menu.item(
                PopupMenuItem::new(i18n::tr("processes.menu_attach_debugger"))
                    .icon(IconName::SquareTerminal)
                    .disabled(is_kernel_or_idle)
                    .on_click(move |_, _, cx| {
                        UiCommands::attach_debugger(&store_dbg, key, cx);
                    }),
            );
        }
    }

    let copy_submenu = build_copy_submenu(copy_fields, window, cx);
    submenu_handles.push(copy_submenu.focus_handle(cx));

    let store_res = store.clone();
    let res_submenu = PopupMenu::build(window, cx, move |sub, window, cx| {
        build_resource_values_submenu(sub, store_res.clone(), window, cx)
    });
    submenu_handles.push(res_submenu.focus_handle(cx));

    menu = menu
        .separator()
        .item(
            PopupMenuItem::submenu(i18n::tr("proc_ux_copy_submenu"), copy_submenu)
                .icon(Icon::new(AssetIconName::Copy)),
        )
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
    let built = menu.item(
        PopupMenuItem::new(i18n::tr("processes.menu_properties"))
            .icon(IconName::Info)
            .disabled(path_props.is_none())
            .on_click(move |_, _, cx| {
                if let Some(ref p) = path_props {
                    UiCommands::show_file_properties(&store_props, PathBuf::from(p.as_ref()), cx);
                }
            }),
    );
    (built, submenu_handles)
}

impl Render for ProcessesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view_mode = self.view_mode;
        let hide_system = self.hide_system_processes;
        let end_task_enabled = self.end_task_enabled;
        let preedit_badge: Option<String> = self
            .type_to_search
            .preedit_text()
            .map(|pre| format!("{}: {pre}", i18n::tr("proc_ux_ime_searching")));

        let theme = cx.theme();
        let border = theme.border;
        let fg = theme.foreground;
        let primary = theme.primary;
        let muted_bg = theme.muted;
        let radius = theme.radius;

        let store_grouped = self.store.clone();
        let store_tree = self.store.clone();
        let store_hide_sys = self.store.clone();
        let store_run_task = self.store.clone();

        let grouped_label = i18n::tr("processes.view_grouped");
        let tree_label = i18n::tr("processes.view_tree");
        let hide_sys_label = i18n::tr("proc_ux_hide_system_tooltip");
        let export_csv_label = i18n::tr("csv_export_button");

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
                    )
                    .child(
                        Button::new("processes-hide-system-toggle")
                            .xsmall()
                            .icon(Icon::new(AssetIconName::EyeOff))
                            .tooltip(hide_sys_label.clone())
                            .accessibility_label(hide_sys_label)
                            .map(|btn| {
                                if hide_system {
                                    btn.primary()
                                } else {
                                    btn.ghost()
                                }
                            })
                            .on_click(move |_, _, cx| {
                                dispatch_hide_system_processes(&store_hide_sys, !hide_system, cx);
                            }),
                    )
                    .when_some(preedit_badge, |el, text| {
                        el.child(
                            div()
                                .px_2()
                                .py_0p5()
                                .rounded(radius)
                                .bg(primary.opacity(0.14))
                                .text_xs()
                                .text_color(primary)
                                .child(text),
                        )
                    }),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1p5()
                    .child(
                        Button::new("processes-export-csv")
                            .small()
                            .outline()
                            .icon(Icon::new(AssetIconName::Download))
                            .tooltip(export_csv_label.clone())
                            .accessibility_label(export_csv_label)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.trigger_export_csv(window, cx);
                            })),
                    )
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

        let table_focus = self.table.focus_handle(cx);
        let view_entity = cx.entity();

        v_flex()
            .id("processes-view-root")
            .size_full()
            .min_h_0()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let m = &event.keystroke.modifiers;
                let primary_mod = m.control || m.platform;
                let no_modifiers = !m.control && !m.alt && !m.platform && !m.shift && !m.function;

                if primary_mod && !m.alt && event.keystroke.key.eq_ignore_ascii_case("c") {
                    let copied = if m.shift {
                        this.copy_selected_summary(cx)
                    } else {
                        this.copy_selected_short(cx)
                    };
                    if copied {
                        cx.stop_propagation();
                    }
                    return;
                }

                if no_modifiers && event.keystroke.key == "delete" && this.end_task_enabled {
                    this.trigger_end_task_for_selected(window, cx);
                    cx.stop_propagation();
                    return;
                }

                if !m.control && !m.alt && !m.platform && event.keystroke.key == "backspace" {
                    let current = this.store.read(cx).search_query().to_owned();
                    if let Some(next) = this.type_to_search.handle_backspace(&current) {
                        UiCommands::set_search_query(&this.store, next, cx);
                        cx.stop_propagation();
                    }
                    return;
                }

                if no_modifiers && event.keystroke.key == "escape" {
                    let was_composing = this.type_to_search.is_composing();
                    let current = this.store.read(cx).search_query().to_owned();
                    if let Some(next) = this.type_to_search.handle_escape(&current) {
                        UiCommands::set_search_query(&this.store, next, cx);
                        cx.stop_propagation();
                    } else if was_composing {
                        cx.notify();
                        cx.stop_propagation();
                    }
                }
            }))
            .child(toolbar)
            .child(
                div()
                    .id("processes-table-hover-zone")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        this.table.update(cx, |table, cx| {
                            table.delegate_mut().set_table_hovered(*hovered, cx);
                        });
                    }))
                    .child(TableInputBridge::new(table_focus, view_entity))
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

    use super::{ProcessColumn, ProcessesView, build_process_row_div};
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

    #[gpui_kit::test]
    fn processes_view_hover_freeze_ime_search_copy_export_and_diagnostics(cx: &mut TestAppContext) {
        use std::sync::Arc;

        use gpui_kit::EntityInputHandler as _;
        use loom_config::{AppSettings, ConfigStore};
        use loom_engine::EngineCoordinator;
        use loom_pal::{NativeBackend, PlatformBackend as _};
        use loom_types::{
            LeakKind, LeakWarning, ProcessRowProjection, ProcessStatusFlags, WebView2Role,
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

        let store =
            cx.new(|cx| UiStore::new(coordinator, config_store, AppSettings::default(), cx));
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

        // Inject deterministic rows including a WebView2 subprocess and a leak-warning process.
        let key_wv2 = ProcessKey::new(4242, 100_000);
        let key_exp = ProcessKey::new(5151, 200_000);
        let synthetic_slice = Arc::new(super::ProcessTableSlice {
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
            // Cancel composition with empty string -> previous query "edge" remains intact.
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
        let (headers, rows) =
            table.read_with(&vcx, |t, _| t.delegate().export_visible_csv_payload());
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
            // Duplicate with different casing must not create a second entry.
            dispatch_ignore_leak_path(&store, r"c:\webview2\msedgewebview2.exe".to_owned(), cx);
        });
        assert!(store.read_with(&vcx, |s, _| s.settings().hide_windows_system_processes));
        assert_eq!(
            store.read_with(&vcx, |s, _| s.settings().leak_ignored_paths.clone()),
            vec![r"C:\WebView2\msedgewebview2.exe".to_owned()]
        );

        coordinator_for_shutdown.shutdown();
    }
}
