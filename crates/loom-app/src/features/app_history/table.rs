//! 2D-virtualized `gpui-kit` [`TableDelegate`] implementation for [`super::AppHistoryView`].

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::time::Instant;

use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Size, StyledExt as _, h_flex, v_flex};
use gpui_kit::{
    App, Context, Empty, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use loom_types::{AppHistoryRecord, AppHistorySlice, AppHistorySortColumn};
use loom_ui_kit::{
    CachedSemanticColors, format_bytes_metric, format_cpu_time_shared, heatmap_cell_metric,
    heatmap_cell_mono, quantize_cpu_time_secs,
};

use crate::bridge::{UiCommands, UiStore};
use crate::i18n;

use super::state::{
    APP_HISTORY_COLUMNS, AppHistoryLocaleCache, CPU_TIME_TEXT_CACHE_LIMIT,
    column_default_direction, column_is_heatmap, column_min_width, column_right_aligned,
    column_slug, column_width, cpu_time_heatmap_level, cumulative_bytes_heatmap_level,
    to_column_sort,
};

/// 2D-virtualized `gpui-kit` [`TableDelegate`] backing [`super::AppHistoryView`].
pub(super) struct AppHistoryTableDelegate {
    store: Entity<UiStore>,
    pub(super) slice: Arc<AppHistorySlice>,
    pub(super) last_rev: u64,
    pub(super) selected_app_id: Option<Arc<str>>,
    pub(super) reanchored_selection: Option<Option<usize>>,
    pub(super) visible_rows: Range<usize>,
    pub(super) last_scroll_at: Option<Instant>,
    pub(super) last_viewport_fp: Option<u64>,
    pub(super) user_action_pending: bool,
    semantic_colors: CachedSemanticColors,
    cpu_time_text_cache: HashMap<u64, SharedString>,
    pub(super) locale: AppHistoryLocaleCache,
}

impl AppHistoryTableDelegate {
    pub(super) fn new(store: Entity<UiStore>, cx: &App) -> Self {
        let store_ref = store.read(cx);
        let snap = store_ref.snapshot();
        let slice = Arc::clone(&snap.app_history);

        Self {
            store: store.clone(),
            last_rev: snap.revisions.app_history,
            slice,
            selected_app_id: None,
            reanchored_selection: None,
            visible_rows: 0..0,
            last_scroll_at: None,
            last_viewport_fp: None,
            user_action_pending: false,
            semantic_colors: CachedSemanticColors::new(),
            cpu_time_text_cache: HashMap::new(),
            locale: AppHistoryLocaleCache::build(),
        }
    }

    #[inline]
    pub(super) fn row_at(&self, row_ix: usize) -> Option<&AppHistoryRecord> {
        let item_idx = *self.slice.visible_indices.get(row_ix)?;
        self.slice.items.get(item_idx)
    }

    /// Locates `app_id` in `slice.visible_indices`, checking `hint_row` in O(1) before scanning.
    #[inline]
    pub(super) fn find_visible_index_by_id(
        &self,
        app_id: &str,
        hint_row: Option<usize>,
    ) -> Option<usize> {
        if let Some(hint) = hint_row
            && self
                .row_at(hint)
                .is_some_and(|record| record.app_id.as_ref() == app_id)
        {
            return Some(hint);
        }
        self.slice.visible_indices.iter().position(|&idx| {
            self.slice
                .items
                .get(idx)
                .is_some_and(|record| record.app_id.as_ref() == app_id)
        })
    }

    #[inline]
    fn cpu_time_shared(&mut self, cpu_time_100ns: u64) -> SharedString {
        let secs = quantize_cpu_time_secs(cpu_time_100ns);
        if self.cpu_time_text_cache.len() >= CPU_TIME_TEXT_CACHE_LIMIT
            && !self.cpu_time_text_cache.contains_key(&secs)
        {
            self.cpu_time_text_cache.clear();
        }
        self.cpu_time_text_cache
            .entry(secs)
            .or_insert_with(|| format_cpu_time_shared(cpu_time_100ns))
            .clone()
    }

    pub(super) fn trigger_sort(&mut self, col_ix: usize, cx: &mut Context<TableState<Self>>) {
        let Some(&target) = APP_HISTORY_COLUMNS.get(col_ix) else {
            return;
        };
        let next_dir = if self.slice.sort_column == target {
            self.slice.sort_direction.toggle()
        } else {
            column_default_direction(target)
        };
        self.user_action_pending = true;
        let store = self.store.clone();
        UiCommands::set_app_history_sort(&store, target, next_dir, cx);
    }
}

impl TableDelegate for AppHistoryTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        APP_HISTORY_COLUMNS.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.slice.visible_indices.len()
    }

    fn loading(&self, _cx: &App) -> bool {
        self.slice.revision == 0
    }

    fn visible_rows_changed(
        &mut self,
        visible_range: Range<usize>,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) {
        if self.visible_rows != visible_range {
            let scrolled = !self.visible_rows.is_empty();
            self.visible_rows = visible_range;
            self.last_viewport_fp = None;
            if scrolled {
                self.last_scroll_at = Some(Instant::now());
            }
        }
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        let Some(&col) = APP_HISTORY_COLUMNS.get(col_ix) else {
            return Column::default();
        };
        let mut column = Column::new(column_slug(col), self.locale.column_label(col))
            .width(px(column_width(col)))
            .min_width(px(column_min_width(col)))
            .selectable(false)
            .movable(false);

        if col == AppHistorySortColumn::Name {
            column = column.fixed_left();
        }
        if column_right_aligned(col) {
            column = column.text_right();
        }
        if column_is_heatmap(col) {
            column = column.p_0();
        }
        if col == self.slice.sort_column {
            column = column.sort(to_column_sort(self.slice.sort_direction));
        } else {
            column = column.sortable();
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
        self.trigger_sort(col_ix, cx);
    }

    fn render_header(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        div()
            .id("app-history-column-header")
            .bg(cx.theme().secondary.opacity(0.45))
            .text_xs()
            .font_medium()
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(&col) = APP_HISTORY_COLUMNS.get(col_ix) else {
            return Empty.into_any_element();
        };
        let is_active = col == self.slice.sort_column;
        let fg = cx.theme().foreground;
        let muted_fg = cx.theme().muted_foreground;
        let label = self.locale.column_label(col);

        h_flex()
            .id(("app-history-th", col_ix))
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .when(column_is_heatmap(col), |d| d.px_2())
            .when(column_right_aligned(col), |d| d.justify_end())
            .text_xs()
            .text_color(if is_active { fg } else { muted_fg })
            .when(is_active, |d| d.font_semibold())
            .cursor_pointer()
            .on_click(cx.listener(move |table, _, _, cx| {
                table.delegate_mut().trigger_sort(col_ix, cx);
            }))
            .child(div().min_w_0().truncate().child(label))
            .into_any_element()
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        div().id(("app-history-row", row_ix)).text_xs()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(&col) = APP_HISTORY_COLUMNS.get(col_ix) else {
            return Empty.into_any_element();
        };
        let Some(&item_idx) = self.slice.visible_indices.get(row_ix) else {
            return Empty.into_any_element();
        };
        if item_idx >= self.slice.items.len() {
            return Empty.into_any_element();
        }

        let theme = cx.theme();
        let colors = self.semantic_colors.get_copy(theme);
        let fg = theme.foreground;
        let mono_font = theme.mono_font_family.clone();

        match col {
            AppHistorySortColumn::Name => h_flex()
                .size_full()
                .items_center()
                .truncate()
                .text_xs()
                .font_medium()
                .text_color(fg)
                .child(SharedString::from(Arc::clone(
                    &self.slice.items[item_idx].name,
                )))
                .into_any_element(),
            AppHistorySortColumn::CpuTime => {
                let cpu_time_100ns = self.slice.items[item_idx].cpu_time_100ns;
                let text = self.cpu_time_shared(cpu_time_100ns);
                heatmap_cell_mono(
                    text,
                    cpu_time_heatmap_level(cpu_time_100ns),
                    &colors,
                    mono_font,
                )
                .into_any_element()
            }
            AppHistorySortColumn::Network => {
                let network_bytes = self.slice.items[item_idx].network_bytes;
                heatmap_cell_metric(
                    format_bytes_metric(network_bytes),
                    cumulative_bytes_heatmap_level(network_bytes),
                    &colors,
                    mono_font,
                )
                .into_any_element()
            }
            AppHistorySortColumn::MeteredNetwork => {
                let metered_network_bytes = self.slice.items[item_idx].metered_network_bytes;
                heatmap_cell_metric(
                    format_bytes_metric(metered_network_bytes),
                    cumulative_bytes_heatmap_level(metered_network_bytes),
                    &colors,
                    mono_font,
                )
                .into_any_element()
            }
        }
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(record) = self.row_at(row_ix).cloned() else {
            return menu;
        };
        let store_search = self.store.clone();
        let query = record.name.to_string();

        menu.item(
            PopupMenuItem::new(i18n::tr("app_history.menu_search_online"))
                .icon(IconName::Globe)
                .on_click(move |_, _, cx| {
                    UiCommands::search_online(&store_search, query.clone(), cx);
                }),
        )
    }

    fn row_label(&self, row_ix: usize, _cx: &App) -> Option<SharedString> {
        let record = self.row_at(row_ix)?;
        Some(
            format!(
                "{} ({})",
                record.name,
                format_cpu_time_shared(record.cpu_time_100ns)
            )
            .into(),
        )
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
                    .child(self.locale.empty.clone()),
            )
    }
}
