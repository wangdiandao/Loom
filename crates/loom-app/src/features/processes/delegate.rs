//! 2D-virtualized `gpui-kit` [`TableDelegate`] backing [`super::ProcessesView`].

use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui_kit::component::menu::PopupMenu;
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, StyledExt as _, h_flex};
use gpui_kit::{
    App, AppContext as _, Context, Div, Empty, Entity, Focusable as _, IntoElement,
    ParentElement as _, SharedString, Stateful, Styled as _, Subscription, WeakEntity, Window, div,
    img, prelude::FluentBuilder as _, px,
};
use loom_engine::EngineCommand;
use loom_types::{
    PlatformProfile, ProcessCategory, ProcessKey, ProcessRowKind, ProcessRowProjection,
    ProcessTableSlice, ResourceDisplayMode,
};
use loom_ui_kit::{
    CachedSemanticColors, HeatmapLevel, RowTextCache, format_child_count_shared, format_disk_cell,
    format_memory_cell, format_network_cell, format_permille, format_permille_metric,
    has_visible_status_badges, heatmap_cell_metric, status_badges_with_a11y,
};

use crate::bridge::{UiCommands, UiStore};
use crate::features::table_columns::{
    ColumnKey, ColumnToggleItem, CopyRowFields, HeaderMenuAction, IconRenderCache,
    ListViewTelemetryKind, OrderFreezeState, ScrollBurstCoalescer, SortPausedBadge,
    TableColumnLayout, build_column_header_menu, expand_chevron_button,
    leak_warning_badge_with_prefix, list_telemetry, open_select_columns_dialog,
    render_mono_numeric_cell, render_mono_preview_cell, render_mono_text_cell,
    render_table_empty_state, render_table_header_row, render_table_th_cell, render_text_cell,
    resolve_budgeted_icon, to_column_sort, track_context_menu_focus, webview2_role_badge,
};
use crate::i18n;
use crate::shell::ShellHandle;

use super::columns::{
    ProcessColumn, ProcessesLocaleCache, build_process_row_div,
    compute_processes_viewport_fingerprint,
};
use super::context_menu::{build_resource_values_submenu, build_row_context_menu};
use super::state::{
    find_selected_row_fast, format_process_copy_short, format_process_row_summary,
    format_process_status_text, next_sort_direction, stable_row_id,
};

/// 2D-virtualized `gpui-kit` [`TableDelegate`] backing the live Processes table.
pub(super) struct ProcessesTableDelegate {
    pub(super) store: Entity<UiStore>,
    pub(super) shell: ShellHandle,
    pub(super) slice: Arc<ProcessTableSlice>,
    pub(super) platform: PlatformProfile,
    pub(super) last_rev: u64,
    pub(super) mem_mode: ResourceDisplayMode,
    pub(super) disk_mode: ResourceDisplayMode,
    pub(super) net_mode: ResourceDisplayMode,
    pub(super) column_layout: TableColumnLayout<ProcessColumn>,
    pub(super) selected_key: Option<ProcessKey>,
    pub(super) selected_row_hint: Option<usize>,
    pub(super) reanchored_selection: Option<Option<usize>>,
    pub(super) last_right_clicked_col: Option<usize>,
    pub(super) order_freeze: OrderFreezeState,
    pub(super) sort_paused_badge: Entity<SortPausedBadge>,
    menu_focus_subs: Vec<Subscription>,
    pub(super) icon_cache: IconRenderCache,
    pub(super) icon_catchup_scheduled: bool,
    semantic_colors: CachedSemanticColors,
    pub(super) row_cache: RowTextCache,
    pub(super) section_header_cache: [Option<(usize, SharedString)>; 3],
    pub(super) locale: ProcessesLocaleCache,
    pub(super) visible_rows: Range<usize>,
    pub(super) visible_cols: Range<usize>,
    last_rows_len: usize,
    pub(super) last_viewport_fingerprint: Option<u64>,
    pub(super) scroll_coalescer: ScrollBurstCoalescer,
    user_initiated_pending: bool,
}

impl ProcessesTableDelegate {
    pub(super) fn new(store: Entity<UiStore>, shell: ShellHandle, cx: &mut App) -> Self {
        let (platform, slice, last_rev, mem_mode, disk_mode, net_mode, saved_cols) = {
            let store_ref = store.read(cx);
            let snapshot = store_ref.snapshot();
            let settings = store_ref.settings();
            (
                snapshot.platform,
                Arc::clone(&snapshot.processes),
                snapshot.revisions.processes,
                settings.memory_display_mode,
                settings.disk_display_mode,
                settings.network_display_mode,
                store_ref.processes_columns().to_vec(),
            )
        };

        let mut column_layout = TableColumnLayout::new(ProcessColumn::default_specs());
        if !saved_cols.is_empty() {
            column_layout.apply_saved(&saved_cols);
        }
        let initial_rows_len = slice.rows.len();
        let locale = ProcessesLocaleCache::build(platform);
        let sort_paused_tip = locale.sort_paused_tooltip.clone();
        let sort_paused_badge =
            cx.new(|_| SortPausedBadge::new("proc-sort-paused", sort_paused_tip));

        Self {
            store,
            shell,
            slice,
            platform,
            last_rev,
            mem_mode,
            disk_mode,
            net_mode,
            column_layout,
            selected_key: None,
            selected_row_hint: None,
            reanchored_selection: None,
            last_right_clicked_col: None,
            order_freeze: OrderFreezeState::default(),
            sort_paused_badge,
            menu_focus_subs: Vec::new(),
            icon_cache: IconRenderCache::new(),
            icon_catchup_scheduled: false,
            semantic_colors: CachedSemanticColors::new(),
            row_cache: RowTextCache::new(),
            section_header_cache: [None, None, None],
            locale,
            visible_rows: 0..0,
            visible_cols: 0..0,
            last_rows_len: initial_rows_len,
            last_viewport_fingerprint: None,
            scroll_coalescer: ScrollBurstCoalescer::new(),
            user_initiated_pending: false,
        }
    }

    #[inline]
    fn cached_section_header_label(
        &mut self,
        category: ProcessCategory,
        count: usize,
    ) -> SharedString {
        let slot = match category {
            ProcessCategory::App => 0,
            ProcessCategory::Background => 1,
            ProcessCategory::System => 2,
        };
        if let Some((cached_count, ref cached_label)) = self.section_header_cache[slot]
            && cached_count == count
        {
            return cached_label.clone();
        }
        let label: SharedString =
            format!("{} ({count})", self.locale.category_label(category)).into();
        self.section_header_cache[slot] = Some((count, label.clone()));
        label
    }

    #[inline]
    pub(super) fn mark_user_initiated(&mut self) {
        self.user_initiated_pending = true;
    }

    #[inline]
    pub(super) fn take_user_initiated(&mut self) -> bool {
        std::mem::take(&mut self.user_initiated_pending)
    }

    #[inline]
    pub(super) fn invalidate_viewport_fingerprint(&mut self) {
        self.last_viewport_fingerprint = None;
    }

    #[inline]
    pub(super) fn compute_viewport_fingerprint(&self, selected_idx: Option<usize>) -> u64 {
        compute_processes_viewport_fingerprint(
            &self.slice,
            &self.column_layout,
            &self.visible_rows,
            &self.visible_cols,
            selected_idx,
            self.mem_mode,
            self.disk_mode,
            self.net_mode,
        )
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
            self.sort_paused_badge.update(cx, |badge, cx| {
                badge.set_frozen(frozen, cx);
            });
        }
    }

    pub(super) fn set_table_hovered(&mut self, hovered: bool, cx: &mut Context<TableState<Self>>) {
        let transition = self.order_freeze.set_table_hovered(hovered);
        self.apply_order_freeze_transition(transition, cx);
    }

    pub(super) fn set_header_hovered(&mut self, hovered: bool, cx: &mut Context<TableState<Self>>) {
        let transition = self.order_freeze.set_header_hovered(hovered);
        self.apply_order_freeze_transition(transition, cx);
    }

    pub(super) fn set_context_menu_open(&mut self, open: bool, cx: &mut Context<TableState<Self>>) {
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

    pub(super) fn selected_copy_short(&self) -> Option<String> {
        let row =
            find_selected_row_fast(&self.slice.rows, self.selected_key, self.selected_row_hint)?;
        Some(format_process_copy_short(row))
    }

    pub(super) fn selected_copy_summary(&self) -> Option<String> {
        let row =
            find_selected_row_fast(&self.slice.rows, self.selected_key, self.selected_row_hint)?;
        Some(self.format_row_summary(row))
    }

    pub(super) fn export_visible_csv_payload(&self) -> (Vec<String>, Vec<Vec<String>>) {
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

    pub(super) fn persist_column_layout(&self, cx: &mut App) {
        let snapshot = self.column_layout.snapshot();
        UiCommands::set_processes_columns(&self.store, snapshot, cx);
    }

    fn trigger_sort_col(&mut self, col_ix: usize, cx: &mut App) {
        let Some(spec) = self.column_layout.visible_column(col_ix) else {
            return;
        };
        let Some(target) = spec.id.sort_column() else {
            return;
        };
        self.mark_user_initiated();
        let next_dir =
            next_sort_direction(self.slice.sort_column, self.slice.sort_direction, target);
        UiCommands::set_process_sort(&self.store, target, next_dir, cx);
    }

    fn hide_column(&mut self, col: ProcessColumn, cx: &mut Context<TableState<Self>>) -> bool {
        if self.column_layout.hide_column(col) {
            self.invalidate_viewport_fingerprint();
            self.persist_column_layout(cx);
            true
        } else {
            false
        }
    }

    fn reset_columns(&mut self, cx: &mut Context<TableState<Self>>) {
        self.column_layout.reset(ProcessColumn::default_specs());
        self.invalidate_viewport_fingerprint();
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
                        table.delegate_mut().invalidate_viewport_fingerprint();
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
        let mut column = Column::new(col.slug(), self.locale.column_label(col))
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
            self.invalidate_viewport_fingerprint();
            self.persist_column_layout(cx);
        }
    }

    fn visible_rows_changed(
        &mut self,
        visible_range: Range<usize>,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) {
        let rows_len = self.slice.rows.len();
        if !self.visible_rows.is_empty()
            && self.visible_rows != visible_range
            && rows_len == self.last_rows_len
        {
            self.scroll_coalescer.on_scroll(Instant::now());
        }
        self.visible_rows = visible_range;
        self.last_rows_len = rows_len;
        self.last_viewport_fingerprint = None;
    }

    fn visible_columns_changed(
        &mut self,
        visible_range: Range<usize>,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) {
        if !self.visible_cols.is_empty() && self.visible_cols != visible_range {
            self.scroll_coalescer.on_scroll(Instant::now());
        }
        self.visible_cols = visible_range;
        self.last_viewport_fingerprint = None;
    }

    fn render_header(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        self.icon_cache.begin_frame();
        let store = self.store.clone();
        let table_weak = cx.entity().downgrade();
        render_table_header_row(
            "processes-column-header",
            "processes-header-bg-menu",
            cx,
            |table, hovered, cx| table.delegate_mut().set_header_hovered(hovered, cx),
            move |menu, window, cx| {
                Self::build_header_context_menu(
                    menu,
                    store.clone(),
                    None,
                    table_weak.clone(),
                    window,
                    cx,
                )
            },
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
        let col_label = self.locale.column_label(col);
        let store = self.store.clone();
        let table_weak = cx.entity().downgrade();

        render_table_th_cell(
            "proc-th",
            col_ix,
            col_label,
            is_active_sort,
            col.is_heatmap(),
            col.right_aligned(),
            &self.sort_paused_badge,
            cx,
            move |table, cx| table.delegate_mut().trigger_sort_col(col_ix, cx),
            move |menu, window, cx| {
                Self::build_header_context_menu(
                    menu,
                    store.clone(),
                    Some(col),
                    table_weak.clone(),
                    window,
                    cx,
                )
            },
        )
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let section_bg = cx.theme().muted.opacity(0.6);
        let row = self.slice.rows.get(row_ix).map(|r| (r.kind, r.key));
        build_process_row_div(row, row_ix, section_bg)
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        list_telemetry().record_render_td(ListViewTelemetryKind::Processes);
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
                    let label = self.cached_section_header_label(category, count);
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
                let hover_bg = theme.list_hover;
                let radius = theme.radius;
                let mono_font = theme.mono_font_family.clone();
                let semantic = self.semantic_colors.get_copy(theme);
                let key = row.key;
                let row_id = stable_row_id(key);

                match spec.id {
                    ProcessColumn::Name => {
                        let indent = f32::from(row.depth.min(8)) * 16.0;
                        let has_children = row.has_children;
                        let is_expanded = row.is_expanded;
                        let is_selected = self.selected_key == Some(key);
                        let rendered_icon = resolve_budgeted_icon(
                            row.icon.as_ref(),
                            &mut self.icon_cache,
                            &mut self.icon_catchup_scheduled,
                            ListViewTelemetryKind::Processes,
                            window,
                            cx,
                            |table, _| {
                                let d = table.delegate_mut();
                                d.icon_catchup_scheduled = false;
                                d.icon_cache.begin_frame();
                            },
                        );
                        let display_name = SharedString::from(Arc::clone(&row.display_name));
                        let child_badge: Option<SharedString> = (row.child_count > 0)
                            .then(|| format_child_count_shared(row.child_count));
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

                        let name_el = if child_badge.is_none() && wv2_label.is_none() {
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .when(is_selected || has_children, |d| d.font_medium())
                                .child(display_name)
                                .into_any_element()
                        } else {
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
                                })
                                .into_any_element()
                        };

                        h_flex()
                            .size_full()
                            .items_center()
                            .gap_1p5()
                            .pl(px(indent))
                            .overflow_hidden()
                            .child(if has_children {
                                expand_chevron_button(
                                    ("proc-expand", row_id),
                                    is_expanded,
                                    hover_bg,
                                    muted_fg,
                                    cx.listener(move |table, _, _, cx| {
                                        table.delegate_mut().mark_user_initiated();
                                        UiCommands::toggle_process_expanded(&toggle_store, key, cx);
                                    }),
                                )
                                .into_any_element()
                            } else {
                                div().w(px(20.)).flex_shrink_0().into_any_element()
                            })
                            .child(icon_el)
                            .child(name_el)
                            .into_any_element()
                    }
                    ProcessColumn::Category => {
                        render_text_cell(self.locale.category_label(row.category), muted_fg)
                    }
                    ProcessColumn::Pid => {
                        render_mono_numeric_cell(self.row_cache.pid_text(key), mono_font, muted_fg)
                    }
                    ProcessColumn::ProcessName => render_mono_text_cell(
                        SharedString::from(Arc::clone(&row.name)),
                        mono_font,
                        muted_fg,
                    ),
                    ProcessColumn::Status => {
                        let has_badges = has_visible_status_badges(row.status);
                        if !has_badges && row.leak_warning.is_none() {
                            return Empty.into_any_element();
                        }
                        let a11y_active = window.is_a11y_active();
                        h_flex()
                            .size_full()
                            .items_center()
                            .gap_1()
                            .overflow_hidden()
                            .when(has_badges, |el| {
                                el.child(status_badges_with_a11y(
                                    row_id,
                                    row.status,
                                    &self.locale.status_labels,
                                    &semantic,
                                    a11y_active,
                                ))
                            })
                            .when_some(row.leak_warning.as_ref(), |el, warning| {
                                el.child(leak_warning_badge_with_prefix(
                                    ("proc-leak", row_id),
                                    self.locale.leak_warning_prefix.as_ref(),
                                    warning,
                                    semantic.leak_warning,
                                ))
                            })
                            .into_any_element()
                    }
                    ProcessColumn::Cpu => heatmap_cell_metric(
                        format_permille_metric(row.cpu_permille),
                        HeatmapLevel::from_permille(row.cpu_permille),
                        &semantic,
                        mono_font,
                    )
                    .into_any_element(),
                    ProcessColumn::Memory => heatmap_cell_metric(
                        self.row_cache.memory_metric(
                            key,
                            row.memory_bytes,
                            row.memory_permille,
                            self.mem_mode,
                        ),
                        HeatmapLevel::from_permille(row.memory_permille),
                        &semantic,
                        mono_font,
                    )
                    .into_any_element(),
                    ProcessColumn::Disk => heatmap_cell_metric(
                        self.row_cache.disk_metric(
                            key,
                            row.disk_bytes_per_sec,
                            row.disk_permille,
                            self.disk_mode,
                        ),
                        HeatmapLevel::from_bytes_per_sec(row.disk_bytes_per_sec),
                        &semantic,
                        mono_font,
                    )
                    .into_any_element(),
                    ProcessColumn::Network => heatmap_cell_metric(
                        self.row_cache.network_metric(
                            key,
                            row.net_bytes_per_sec,
                            row.net_permille,
                            self.net_mode,
                        ),
                        HeatmapLevel::from_bytes_per_sec(row.net_bytes_per_sec),
                        &semantic,
                        mono_font,
                    )
                    .into_any_element(),
                    ProcessColumn::Gpu => heatmap_cell_metric(
                        format_permille_metric(row.gpu_util_permille),
                        HeatmapLevel::from_permille(row.gpu_util_permille),
                        &semantic,
                        mono_font,
                    )
                    .into_any_element(),
                    ProcessColumn::Publisher => {
                        let publisher_text = row
                            .publisher
                            .as_ref()
                            .map(|p| SharedString::from(Arc::clone(p)))
                            .unwrap_or_default();
                        render_text_cell(publisher_text, muted_fg)
                    }
                    ProcessColumn::CommandLine => render_mono_preview_cell(
                        "proc-cmd",
                        row_id,
                        key,
                        0,
                        row.command_line.as_ref(),
                        spec.width,
                        &mut self.row_cache,
                        mono_font,
                        muted_fg,
                    ),
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

        track_context_menu_focus(
            &mut self.menu_focus_subs,
            root_focus,
            submenu_handles,
            window,
            cx,
            |table, cx| table.delegate_mut().set_context_menu_open(false, cx),
        );

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
        render_table_empty_state(
            self.locale.empty_search.clone(),
            cx.theme().muted_foreground,
        )
    }
}
