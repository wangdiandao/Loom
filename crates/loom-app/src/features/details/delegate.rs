//! 2D-virtualized `gpui-kit` [`TableDelegate`] backing [`super::DetailsView`].

use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui_kit::component::menu::PopupMenu;
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Size, StyledExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Div, Empty, Entity, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, SharedString, Stateful, Styled as _, Subscription, WeakEntity,
    Window, div, img, prelude::FluentBuilder as _, px,
};
use loom_engine::EngineCommand;
use loom_types::{
    CapabilityFlags, DetailsRowProjection, DetailsTableSlice, DetailsViewMode, PlatformProfile,
    ProcessKey, ProcessStatusFlags,
};
use loom_ui_kit::{
    CachedSemanticColors, HeatmapLevel, RowTextCache, format_bytes, format_cpu_time,
    format_permille, format_permille_metric, format_signed_bytes, heatmap_cell_metric,
};

use crate::bridge::{UiCommands, UiStore};
use crate::features::table_columns::{
    ColumnKey, ColumnToggleItem, CopyRowFields, HeaderMenuAction, IconRenderCache,
    ListViewTelemetryKind, OrderFreezeState, ScrollBurstCoalescer, SortPausedBadge,
    TableColumnLayout, build_column_header_menu, expand_chevron_button,
    leak_warning_badge_with_prefix, list_telemetry, open_select_columns_dialog, render_metric_cell,
    render_mono_numeric_cell, render_mono_preview_cell, render_table_empty_state,
    render_table_header_row, render_table_th_cell, render_text_cell, resolve_budgeted_icon,
    to_column_sort, track_context_menu_focus, webview2_role_badge,
};
use crate::shell::ShellHandle;

use super::columns::{DetailsColumn, DetailsLocaleCache, compute_details_viewport_fingerprint};
use super::context_menu::build_row_context_menu;
use super::state::{
    ProcessActionTarget, cached_user_name, can_terminate_details_row, display_user_name,
    find_row_index_by_key_fast, format_details_copy_short, format_details_row_summary,
    memory_heatmap_level, next_sort_direction, process_key_element_id,
};

/// 2D-virtualized `gpui-kit` [`TableDelegate`] backing the live Details table.
pub(super) struct DetailsTableDelegate {
    pub(super) store: Entity<UiStore>,
    shell_handle: ShellHandle,
    pub(super) details: Arc<DetailsTableSlice>,
    pub(super) capabilities: CapabilityFlags,
    pub(super) platform: PlatformProfile,
    pub(super) total_ram_bytes: u64,
    pub(super) last_rev: u64,
    pub(super) selected_key: Option<ProcessKey>,
    pub(super) selected_row_hint: Option<usize>,
    pub(super) reanchored_selection: Option<Option<usize>>,
    pub(super) highlighted_key: Option<ProcessKey>,
    pub(super) pending_scroll_key: Option<ProcessKey>,
    pub(super) last_right_clicked_col: Option<usize>,
    pub(super) order_freeze: OrderFreezeState,
    pub(super) sort_paused_badge: Entity<SortPausedBadge>,
    menu_focus_subs: Vec<Subscription>,
    pub(super) column_layout: TableColumnLayout<DetailsColumn>,
    pub(super) icon_cache: IconRenderCache,
    pub(super) icon_catchup_scheduled: bool,
    semantic_colors: CachedSemanticColors,
    pub(super) row_cache: RowTextCache,
    user_name_cache: HashMap<String, SharedString>,
    pub(super) locale: DetailsLocaleCache,
    pub(super) visible_rows: Range<usize>,
    pub(super) visible_cols: Range<usize>,
    last_rows_len: usize,
    pub(super) last_viewport_fingerprint: Option<u64>,
    pub(super) scroll_coalescer: ScrollBurstCoalescer,
    user_initiated_pending: bool,
}

impl DetailsTableDelegate {
    pub(super) fn new(
        store: Entity<UiStore>,
        highlighted_key: Option<ProcessKey>,
        shell_handle: ShellHandle,
        cx: &mut App,
    ) -> Self {
        let (details, capabilities, platform, total_ram_bytes, last_rev, saved_cols) = {
            let store_ref = store.read(cx);
            let snap = store_ref.snapshot();
            (
                Arc::clone(&snap.details),
                snap.capabilities,
                snap.platform,
                snap.performance.latest.mem_total_bytes,
                snap.revisions.details,
                store_ref.details_columns().to_vec(),
            )
        };

        let mut column_layout = TableColumnLayout::new(DetailsColumn::default_specs(capabilities));
        if !saved_cols.is_empty() {
            column_layout.apply_saved(&saved_cols);
        }
        let initial_rows_len = details.rows.len();
        let locale = DetailsLocaleCache::build(platform);
        let sort_paused_tip = locale.sort_paused_tooltip.clone();
        let sort_paused_badge =
            cx.new(|_| SortPausedBadge::new("details-sort-paused", sort_paused_tip));

        Self {
            store,
            shell_handle,
            details,
            capabilities,
            platform,
            total_ram_bytes,
            last_rev,
            selected_key: highlighted_key,
            selected_row_hint: None,
            reanchored_selection: None,
            highlighted_key,
            pending_scroll_key: highlighted_key,
            last_right_clicked_col: None,
            order_freeze: OrderFreezeState::default(),
            sort_paused_badge,
            menu_focus_subs: Vec::new(),
            column_layout,
            icon_cache: IconRenderCache::new(),
            icon_catchup_scheduled: false,
            semantic_colors: CachedSemanticColors::new(),
            row_cache: RowTextCache::new(),
            user_name_cache: HashMap::new(),
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
        compute_details_viewport_fingerprint(
            &self.details,
            &self.column_layout,
            &self.visible_rows,
            &self.visible_cols,
            selected_idx,
            self.highlighted_key,
            self.total_ram_bytes,
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
                .dispatch_command(EngineCommand::SetDetailsOrderFreeze(frozen));
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

    fn format_cell_text(&self, row: &DetailsRowProjection, col: DetailsColumn) -> String {
        match col {
            DetailsColumn::Name => row.name.to_string(),
            DetailsColumn::Pid => row.key.pid.to_string(),
            DetailsColumn::Status => {
                if row.status.contains(ProcessStatusFlags::SUSPENDED) {
                    self.locale.suspended.to_string()
                } else {
                    self.locale.running.to_string()
                }
            }
            DetailsColumn::UserName => display_user_name(row.user_name.as_deref()).to_owned(),
            DetailsColumn::SessionId => row.session_id.to_string(),
            DetailsColumn::Cpu => format_permille(row.cpu_permille),
            DetailsColumn::CpuTime => format_cpu_time(row.cpu_time_100ns),
            DetailsColumn::Memory => format_bytes(row.private_working_set_bytes),
            DetailsColumn::WorkingSet => format_bytes(row.working_set_bytes),
            DetailsColumn::WorkingSetDelta => format_signed_bytes(row.working_set_delta_bytes),
            DetailsColumn::CommitSize => format_bytes(row.commit_bytes),
            DetailsColumn::Priority => self.locale.priority_label(row.priority).to_string(),
            DetailsColumn::Handles => row.handle_count.to_string(),
            DetailsColumn::Threads => row.thread_count.to_string(),
            DetailsColumn::Platform => self.locale.arch_label(row.arch).to_string(),
            DetailsColumn::UacVirtualization => {
                self.locale.uac_label(row.uac_virtualization).to_string()
            }
            DetailsColumn::Elevated => {
                if row.status.contains(ProcessStatusFlags::ELEVATED) {
                    self.locale.elevated_yes.to_string()
                } else {
                    self.locale.elevated_no.to_string()
                }
            }
            DetailsColumn::GpuMemory => format_bytes(row.gpu_dedicated_bytes),
            DetailsColumn::Publisher => row
                .publisher
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("")
                .to_owned(),
            DetailsColumn::Description => row
                .description
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("")
                .to_owned(),
            DetailsColumn::ExePath => row
                .exe_path
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("")
                .to_owned(),
            DetailsColumn::CommandLine => row
                .command_line
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("")
                .to_owned(),
        }
    }

    fn format_row_summary(&self, row: &DetailsRowProjection) -> String {
        let status_text = self.format_cell_text(row, DetailsColumn::Status);
        let priority_text = self.locale.priority_label(row.priority);
        let arch_text = self.locale.arch_label(row.arch);
        format_details_row_summary(
            row,
            &status_text,
            priority_text.as_ref(),
            arch_text.as_ref(),
        )
    }

    pub(super) fn selected_row(&self) -> Option<&DetailsRowProjection> {
        let sel = self.selected_key?;
        find_row_index_by_key_fast(&self.details.rows, sel, self.selected_row_hint)
            .and_then(|idx| self.details.rows.get(idx))
    }

    pub(super) fn selected_copy_short(&self) -> Option<String> {
        self.selected_row().map(format_details_copy_short)
    }

    pub(super) fn selected_copy_summary(&self) -> Option<String> {
        self.selected_row().map(|row| self.format_row_summary(row))
    }

    pub(super) fn export_visible_csv_payload(&self) -> (Vec<String>, Vec<Vec<String>>) {
        let visible_cols = self.column_layout.visible_columns();
        let headers: Vec<String> = visible_cols
            .iter()
            .map(|spec| spec.id.label(self.platform))
            .collect();
        let rows: Vec<Vec<String>> = self
            .details
            .rows
            .iter()
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
        UiCommands::set_details_columns(&self.store, snapshot, cx);
    }

    fn trigger_sort_col(&mut self, col_ix: usize, cx: &mut App) {
        let Some(spec) = self.column_layout.visible_column(col_ix) else {
            return;
        };
        let Some(target) = spec.id.sort_column() else {
            return;
        };
        self.mark_user_initiated();
        let next_dir = next_sort_direction(
            self.details.sort_column,
            self.details.sort_direction,
            target,
        );
        UiCommands::set_details_sort(&self.store, target, next_dir, cx);
    }

    fn hide_column(&mut self, col: DetailsColumn, cx: &mut App) -> bool {
        if self.column_layout.hide_column(col) {
            self.invalidate_viewport_fingerprint();
            self.persist_column_layout(cx);
            true
        } else {
            false
        }
    }

    fn reset_columns(&mut self, cx: &mut App) {
        self.column_layout
            .reset(DetailsColumn::default_specs(self.capabilities));
        self.invalidate_viewport_fingerprint();
        UiCommands::set_details_columns(&self.store, Vec::new(), cx);
    }

    fn open_select_columns_modal(
        &self,
        table_weak: WeakEntity<TableState<Self>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let items = DetailsColumn::selectable_order(self.capabilities)
            .into_iter()
            .map(|col| ColumnToggleItem {
                slug: col.slug(),
                label: col.label(self.platform),
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
        clicked_col: Option<DetailsColumn>,
        table_weak: WeakEntity<TableState<Self>>,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let hide_action: Option<HeaderMenuAction> = clicked_col
            .filter(|&col| col != DetailsColumn::Name)
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
            |sub, _window, _cx| sub,
            on_reset_defaults,
            window,
            cx,
        )
    }

    pub(super) fn selected_target(&self) -> Option<ProcessActionTarget> {
        self.selected_row().map(ProcessActionTarget::from_row)
    }

    pub(super) fn can_end_selected(&self) -> bool {
        self.selected_row()
            .is_some_and(|row| can_terminate_details_row(row.status))
    }
}

impl TableDelegate for DetailsTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.column_layout.visible_count()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.details.rows.len()
    }

    fn loading(&self, _cx: &App) -> bool {
        self.details.revision == 0
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
            if sort_col == self.details.sort_column {
                column = column.sort(to_column_sort(self.details.sort_direction));
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
        let rows_len = self.details.rows.len();
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
        let table_weak = cx.entity().downgrade();
        render_table_header_row(
            "details-column-header",
            "details-header-bg-menu",
            cx,
            |table, hovered, cx| table.delegate_mut().set_header_hovered(hovered, cx),
            move |menu, window, cx| {
                Self::build_header_context_menu(menu, None, table_weak.clone(), window, cx)
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
        let is_active_sort = col.sort_column() == Some(self.details.sort_column);
        let col_label = self.locale.column_label(col);
        let table_weak = cx.entity().downgrade();

        render_table_th_cell(
            "details-th",
            col_ix,
            col_label,
            is_active_sort,
            col.is_heatmap(),
            col.right_aligned(),
            &self.sort_paused_badge,
            cx,
            move |table, cx| table.delegate_mut().trigger_sort_col(col_ix, cx),
            move |menu, window, cx| {
                Self::build_header_context_menu(menu, Some(col), table_weak.clone(), window, cx)
            },
        )
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let Some(row) = self.details.rows.get(row_ix) else {
            return div().id(("details-empty", row_ix));
        };
        let is_highlighted = self.highlighted_key == Some(row.key);
        let primary = cx.theme().primary;

        div()
            .id(("details-row", process_key_element_id(row.key)))
            .text_xs()
            .when(is_highlighted, |s| {
                s.bg(primary.opacity(0.18)).font_medium()
            })
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        list_telemetry().record_render_td(ListViewTelemetryKind::Details);
        let Some(row) = self.details.rows.get(row_ix) else {
            return Empty.into_any_element();
        };
        let Some(spec) = self.column_layout.visible_column(col_ix) else {
            return Empty.into_any_element();
        };

        let theme = cx.theme();
        let muted_fg = theme.muted_foreground;
        let muted_bg = theme.muted;
        let hover_bg = theme.list_hover;
        let radius = theme.radius;
        let mono_font = theme.mono_font_family.clone();
        let semantic = self.semantic_colors.get_copy(theme);
        let key = row.key;
        let row_id = process_key_element_id(key);

        match spec.id {
            DetailsColumn::Name => {
                let is_tree_mode = self.details.view_mode == DetailsViewMode::Tree;
                let indent = if is_tree_mode {
                    f32::from(row.depth.min(10)) * 16.0
                } else {
                    0.0
                };
                let has_children = is_tree_mode && row.has_children;
                let is_expanded = row.is_expanded;
                let icon_image = resolve_budgeted_icon(
                    row.icon.as_ref(),
                    &mut self.icon_cache,
                    &mut self.icon_catchup_scheduled,
                    ListViewTelemetryKind::Details,
                    window,
                    cx,
                    |table, _| {
                        let d = table.delegate_mut();
                        d.icon_catchup_scheduled = false;
                        d.icon_cache.begin_frame();
                    },
                );
                let toggle_store = self.store.clone();
                let name = SharedString::from(Arc::clone(&row.name));
                let wv2_label = row
                    .webview2_role
                    .map(|role| self.locale.webview2_label(role));

                let name_el = if let Some(role_label) = wv2_label {
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .items_center()
                        .gap_1p5()
                        .overflow_hidden()
                        .child(div().min_w_0().truncate().text_xs().child(name))
                        .child(webview2_role_badge(role_label, muted_bg, muted_fg, radius))
                        .into_any_element()
                } else {
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .child(name)
                        .into_any_element()
                };

                h_flex()
                    .size_full()
                    .items_center()
                    .gap_1p5()
                    .pl(px(indent))
                    .overflow_hidden()
                    .when(is_tree_mode, |el| {
                        el.child(if has_children {
                            expand_chevron_button(
                                ("details-expand", row_id),
                                is_expanded,
                                hover_bg,
                                muted_fg,
                                cx.listener(move |table, _, _, cx| {
                                    table.delegate_mut().mark_user_initiated();
                                    UiCommands::toggle_details_expanded(&toggle_store, key, cx);
                                }),
                            )
                            .into_any_element()
                        } else {
                            div().w(px(20.)).flex_shrink_0().into_any_element()
                        })
                    })
                    .child(if let Some(image) = icon_image {
                        img(image).size(px(16.)).flex_shrink_0().into_any_element()
                    } else {
                        Icon::new(IconName::File)
                            .size(px(15.))
                            .flex_shrink_0()
                            .text_color(muted_fg)
                            .into_any_element()
                    })
                    .child(name_el)
                    .into_any_element()
            }
            DetailsColumn::Pid => {
                render_mono_numeric_cell(self.row_cache.pid_text(key), mono_font, muted_fg)
            }
            DetailsColumn::Status => {
                let status_text = if row.status.contains(ProcessStatusFlags::SUSPENDED) {
                    self.locale.suspended.clone()
                } else {
                    self.locale.running.clone()
                };
                h_flex()
                    .size_full()
                    .items_center()
                    .gap_1()
                    .overflow_hidden()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(muted_fg)
                            .child(status_text),
                    )
                    .when_some(row.leak_warning.as_ref(), |el, warning| {
                        el.child(leak_warning_badge_with_prefix(
                            ("details-leak", row_id),
                            self.locale.leak_warning_prefix.as_ref(),
                            warning,
                            semantic.leak_warning,
                        ))
                    })
                    .into_any_element()
            }
            DetailsColumn::UserName => {
                let user_text = cached_user_name(&mut self.user_name_cache, row.user_name.as_ref());
                render_text_cell(user_text, muted_fg)
            }
            DetailsColumn::SessionId => render_mono_numeric_cell(
                self.row_cache.u32_slot(key, 0, row.session_id),
                mono_font,
                muted_fg,
            ),
            DetailsColumn::Cpu => heatmap_cell_metric(
                format_permille_metric(row.cpu_permille),
                HeatmapLevel::from_permille(row.cpu_permille),
                &semantic,
                mono_font,
            )
            .into_any_element(),
            DetailsColumn::CpuTime => render_mono_numeric_cell(
                self.row_cache.cpu_time_text(key, row.cpu_time_100ns),
                mono_font,
                muted_fg,
            ),
            DetailsColumn::Memory => heatmap_cell_metric(
                self.row_cache
                    .bytes_metric_slot(key, 0, row.private_working_set_bytes),
                memory_heatmap_level(row.private_working_set_bytes, self.total_ram_bytes),
                &semantic,
                mono_font,
            )
            .into_any_element(),
            DetailsColumn::WorkingSet => render_metric_cell(
                self.row_cache
                    .bytes_metric_slot(key, 1, row.working_set_bytes),
                mono_font,
                muted_fg,
            ),
            DetailsColumn::WorkingSetDelta => render_metric_cell(
                self.row_cache
                    .signed_bytes_metric_slot(key, 0, row.working_set_delta_bytes),
                mono_font,
                muted_fg,
            ),
            DetailsColumn::CommitSize => render_metric_cell(
                self.row_cache.bytes_metric_slot(key, 2, row.commit_bytes),
                mono_font,
                muted_fg,
            ),
            DetailsColumn::Priority => {
                render_text_cell(self.locale.priority_label(row.priority), muted_fg)
            }
            DetailsColumn::Handles => render_mono_numeric_cell(
                self.row_cache.u32_slot(key, 1, row.handle_count),
                mono_font,
                muted_fg,
            ),
            DetailsColumn::Threads => render_mono_numeric_cell(
                self.row_cache.u32_slot(key, 2, row.thread_count),
                mono_font,
                muted_fg,
            ),
            DetailsColumn::Platform => render_text_cell(self.locale.arch_label(row.arch), muted_fg),
            DetailsColumn::UacVirtualization => {
                render_text_cell(self.locale.uac_label(row.uac_virtualization), muted_fg)
            }
            DetailsColumn::Elevated => {
                let text = if row.status.contains(ProcessStatusFlags::ELEVATED) {
                    self.locale.elevated_yes.clone()
                } else {
                    self.locale.elevated_no.clone()
                };
                render_text_cell(text, muted_fg)
            }
            DetailsColumn::GpuMemory => render_metric_cell(
                self.row_cache
                    .bytes_metric_slot(key, 3, row.gpu_dedicated_bytes),
                mono_font,
                muted_fg,
            ),
            DetailsColumn::Publisher => {
                let text = row
                    .publisher
                    .as_ref()
                    .map(|p| SharedString::from(Arc::clone(p)))
                    .unwrap_or_else(|| SharedString::new_static("—"));
                render_text_cell(text, muted_fg)
            }
            DetailsColumn::Description => {
                let text = row
                    .description
                    .as_ref()
                    .filter(|s| !s.is_empty())
                    .map(|s| SharedString::from(Arc::clone(s)))
                    .unwrap_or_else(|| SharedString::new_static("—"));
                render_text_cell(text, muted_fg)
            }
            DetailsColumn::ExePath => render_mono_preview_cell(
                "details-exe",
                row_id,
                key,
                0,
                row.exe_path.as_ref(),
                spec.width,
                &mut self.row_cache,
                mono_font,
                muted_fg,
            ),
            DetailsColumn::CommandLine => render_mono_preview_cell(
                "details-cmd",
                row_id,
                key,
                1,
                row.command_line.as_ref(),
                spec.width,
                &mut self.row_cache,
                mono_font,
                muted_fg,
            ),
        }
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(row) = self.details.rows.get(row_ix).cloned() else {
            return menu;
        };
        let target = ProcessActionTarget::from_row(&row);
        let clicked_column = self
            .last_right_clicked_col
            .and_then(|col_ix| self.column_layout.visible_column(col_ix))
            .map(|spec| {
                (
                    spec.id.label(self.platform),
                    self.format_cell_text(&row, spec.id),
                )
            });
        let copy_fields = CopyRowFields {
            name: row.name.to_string(),
            pid: row.key.pid,
            exe_path: row.exe_path.as_deref().map(str::to_owned),
            command_line: row.command_line.as_deref().map(str::to_owned),
            row_summary: self.format_row_summary(&row),
            clicked_column,
        };

        self.set_context_menu_open(true, cx);

        let root_focus = menu.focus_handle(cx);
        let (built_menu, submenu_handles) = build_row_context_menu(
            menu,
            &self.store,
            &self.shell_handle,
            &target,
            copy_fields,
            self.capabilities,
            window,
            cx,
        );

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
        let row = self.details.rows.get(row_ix)?;
        Some(format!("{} (PID {})", row.name, row.key.pid).into())
    }

    fn render_loading(
        &mut self,
        _size: Size,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let muted_fg = cx.theme().muted_foreground;
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .p_8()
            .child(
                div()
                    .text_sm()
                    .text_color(muted_fg)
                    .child(self.locale.loading.clone()),
            )
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
