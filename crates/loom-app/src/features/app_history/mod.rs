//! On-demand App History vertical feature slice gated by an RAII [`ScopedMonitorLease`].
//!
//! Splits the static toolbar + collection start date indicator ([`AppHistoryView`])
//! from the 2D-virtualized `gpui-kit` `DataTable` (`Entity<TableState<AppHistoryTableDelegate>>`)
//! so hidden routes incur zero background polling or history aggregation overhead.
//! Filtering and sorting are offloaded completely to `loom-engine`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableEvent, TableState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, Size, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Empty, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, Window, div, prelude::FluentBuilder as _, px,
};
use loom_types::{
    AppHistoryRecord, AppHistorySlice, AppHistorySortColumn, CapabilityFlags, LanguagePreference,
    MonitorDomain, SortDirection,
};
use loom_ui_kit::{
    CachedSemanticColors, HeatmapLevel, format_bytes_metric, format_cpu_time_shared,
    heatmap_cell_metric, heatmap_cell_mono, quantize_bytes_display, quantize_cpu_time_secs,
};

use crate::bridge::{ScopedMonitorLease, UiCommands, UiStore, UiStoreEvent};
use crate::features::table_columns::DataTablePane;
use crate::i18n;

const APP_HISTORY_HEADER_HEIGHT: f32 = 32.0;
const SCROLL_COALESCE_QUIET_WINDOW: Duration = Duration::from_millis(150);
const SCROLL_COALESCE_MAX_DEFER: Duration = Duration::from_millis(500);
const DEFAULT_FINGERPRINT_WINDOW: usize = 64;

const APP_HISTORY_COLUMNS: [AppHistorySortColumn; 4] = [
    AppHistorySortColumn::Name,
    AppHistorySortColumn::CpuTime,
    AppHistorySortColumn::Network,
    AppHistorySortColumn::MeteredNetwork,
];

#[inline]
const fn column_slug(col: AppHistorySortColumn) -> &'static str {
    match col {
        AppHistorySortColumn::Name => "name",
        AppHistorySortColumn::CpuTime => "cpu_time",
        AppHistorySortColumn::Network => "network",
        AppHistorySortColumn::MeteredNetwork => "metered_network",
    }
}

#[inline]
const fn column_width(col: AppHistorySortColumn) -> f32 {
    match col {
        AppHistorySortColumn::Name => 320.0,
        AppHistorySortColumn::CpuTime => 150.0,
        AppHistorySortColumn::Network => 150.0,
        AppHistorySortColumn::MeteredNetwork => 170.0,
    }
}

#[inline]
const fn column_min_width(col: AppHistorySortColumn) -> f32 {
    match col {
        AppHistorySortColumn::Name => 150.0,
        AppHistorySortColumn::CpuTime => 100.0,
        AppHistorySortColumn::Network => 100.0,
        AppHistorySortColumn::MeteredNetwork => 116.0,
    }
}

#[inline]
const fn column_right_aligned(col: AppHistorySortColumn) -> bool {
    matches!(
        col,
        AppHistorySortColumn::CpuTime
            | AppHistorySortColumn::Network
            | AppHistorySortColumn::MeteredNetwork
    )
}

#[inline]
const fn column_is_heatmap(col: AppHistorySortColumn) -> bool {
    matches!(
        col,
        AppHistorySortColumn::CpuTime
            | AppHistorySortColumn::Network
            | AppHistorySortColumn::MeteredNetwork
    )
}

#[inline]
const fn column_default_direction(col: AppHistorySortColumn) -> SortDirection {
    match col {
        AppHistorySortColumn::CpuTime
        | AppHistorySortColumn::Network
        | AppHistorySortColumn::MeteredNetwork => SortDirection::Descending,
        AppHistorySortColumn::Name => SortDirection::Ascending,
    }
}

#[inline]
const fn to_column_sort(dir: SortDirection) -> ColumnSort {
    match dir {
        SortDirection::Ascending => ColumnSort::Ascending,
        SortDirection::Descending => ColumnSort::Descending,
    }
}

/// Formats a Unix epoch timestamp (in seconds) into UTC `YYYY-MM-DD HH:MM`.
pub(super) fn format_epoch_datetime(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let rem_secs = epoch_secs % 86_400;
    let hour = rem_secs / 3_600;
    let minute = (rem_secs % 3_600) / 60;

    // Howard Hinnant's proleptic Gregorian `civil_from_days` algorithm.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

/// Maps cumulative CPU time (in 100-ns ticks) to a quantized [`HeatmapLevel`].
#[inline]
pub(super) const fn cpu_time_heatmap_level(cpu_time_100ns: u64) -> HeatmapLevel {
    const TICKS_PER_SEC: u64 = 10_000_000;
    let secs = cpu_time_100ns / TICKS_PER_SEC;
    match secs {
        0..=9 => HeatmapLevel::None,
        10..=59 => HeatmapLevel::Low,
        60..=599 => HeatmapLevel::Medium,
        600..=3_599 => HeatmapLevel::High,
        _ => HeatmapLevel::Critical,
    }
}

/// Maps cumulative network transfer bytes to a quantized [`HeatmapLevel`].
#[inline]
pub(super) const fn cumulative_bytes_heatmap_level(bytes: u64) -> HeatmapLevel {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    if bytes < MIB {
        HeatmapLevel::None
    } else if bytes < 50 * MIB {
        HeatmapLevel::Low
    } else if bytes < 500 * MIB {
        HeatmapLevel::Medium
    } else if bytes < 2 * GIB {
        HeatmapLevel::High
    } else {
        HeatmapLevel::Critical
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoalesceDecision {
    ApplyNow,
    Defer(Duration),
}

#[derive(Debug, Default)]
struct ScrollCoalescer {
    deferred_since: Option<Instant>,
}

impl ScrollCoalescer {
    fn decide(
        &mut self,
        now: Instant,
        last_scroll_at: Option<Instant>,
        bypass: bool,
    ) -> CoalesceDecision {
        if bypass {
            self.deferred_since = None;
            return CoalesceDecision::ApplyNow;
        }
        let Some(scrolled_at) = last_scroll_at else {
            self.deferred_since = None;
            return CoalesceDecision::ApplyNow;
        };
        let since_scroll = now.saturating_duration_since(scrolled_at);
        if since_scroll >= SCROLL_COALESCE_QUIET_WINDOW {
            self.deferred_since = None;
            return CoalesceDecision::ApplyNow;
        }
        let first_deferred = *self.deferred_since.get_or_insert(now);
        let since_defer = now.saturating_duration_since(first_deferred);
        if since_defer >= SCROLL_COALESCE_MAX_DEFER {
            self.deferred_since = None;
            return CoalesceDecision::ApplyNow;
        }
        let quiet_remaining = SCROLL_COALESCE_QUIET_WINDOW - since_scroll;
        let cap_remaining = SCROLL_COALESCE_MAX_DEFER - since_defer;
        CoalesceDecision::Defer(quiet_remaining.min(cap_remaining))
    }
}

/// Computes a lightweight content fingerprint over only the currently visible rows using
/// display-quantized CPU seconds and byte buckets.
pub(super) fn compute_app_history_viewport_fingerprint(
    slice: &AppHistorySlice,
    visible_rows: &Range<usize>,
    selected_row: Option<usize>,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    let total = slice.visible_indices.len();
    (slice.revision == 0).hash(&mut hasher);
    total.hash(&mut hasher);
    slice.sort_column.hash(&mut hasher);
    slice.sort_direction.hash(&mut hasher);
    selected_row.hash(&mut hasher);

    let range = if visible_rows.is_empty() {
        0..total.min(DEFAULT_FINGERPRINT_WINDOW)
    } else {
        visible_rows.start.min(total)..visible_rows.end.min(total)
    };
    range.start.hash(&mut hasher);
    range.end.hash(&mut hasher);

    for &item_idx in &slice.visible_indices[range] {
        if let Some(record) = slice.items.get(item_idx) {
            record.app_id.hash(&mut hasher);
            record.name.hash(&mut hasher);
            quantize_cpu_time_secs(record.cpu_time_100ns).hash(&mut hasher);
            quantize_bytes_display(record.network_bytes).hash(&mut hasher);
            cumulative_bytes_heatmap_level(record.network_bytes).hash(&mut hasher);
            quantize_bytes_display(record.metered_network_bytes).hash(&mut hasher);
            cumulative_bytes_heatmap_level(record.metered_network_bytes).hash(&mut hasher);
        }
    }

    hasher.finish()
}

/// Opens a confirmation dialog before clearing cumulative application usage history.
fn open_clear_history_dialog(store: &Entity<UiStore>, window: &mut Window, cx: &mut App) {
    let store = store.clone();
    window.open_dialog(cx, move |dialog, _window, cx| {
        let store_confirm = store.clone();
        let muted_fg = cx.theme().muted_foreground;
        let warning = cx.theme().warning;
        let danger = cx.theme().danger;
        let radius = cx.theme().radius;

        dialog
            .w(px(440.))
            .title(i18n::tr("app_history.confirm_delete_title"))
            .child(
                h_flex()
                    .w_full()
                    .items_start()
                    .gap_2p5()
                    .p_3()
                    .rounded(radius)
                    .bg(warning.opacity(0.12))
                    .border_1()
                    .border_color(warning.opacity(0.35))
                    .child(
                        Icon::new(IconName::TriangleAlert)
                            .small()
                            .text_color(danger),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(muted_fg)
                            .child(i18n::tr("app_history.confirm_delete_message")),
                    ),
            )
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("app-history-clear-cancel")
                            .outline()
                            .label(i18n::tr("common.cancel"))
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("app-history-clear-ok")
                            .danger()
                            .label(i18n::tr("app_history.confirm_delete_action"))
                            .on_click(move |_, window, cx| {
                                UiCommands::clear_app_history(&store_confirm, cx);
                                window.close_dialog(cx);
                            }),
                    ),
            )
    });
}

/// Cached localized strings for [`AppHistoryTableDelegate`].
struct AppHistoryLocaleCache {
    col_name: SharedString,
    col_cpu_time: SharedString,
    col_network: SharedString,
    col_metered_network: SharedString,
    loading: SharedString,
    empty: SharedString,
}

impl AppHistoryLocaleCache {
    fn build() -> Self {
        Self {
            col_name: i18n::tr("app_history.col_name").into(),
            col_cpu_time: i18n::tr("app_history.col_cpu_time").into(),
            col_network: i18n::tr("app_history.col_network").into(),
            col_metered_network: i18n::tr("app_history.col_metered_network").into(),
            loading: i18n::tr("app_history.loading").into(),
            empty: i18n::tr("app_history.empty").into(),
        }
    }

    #[inline]
    fn column_label(&self, col: AppHistorySortColumn) -> SharedString {
        match col {
            AppHistorySortColumn::Name => self.col_name.clone(),
            AppHistorySortColumn::CpuTime => self.col_cpu_time.clone(),
            AppHistorySortColumn::Network => self.col_network.clone(),
            AppHistorySortColumn::MeteredNetwork => self.col_metered_network.clone(),
        }
    }
}

/// 2D-virtualized `gpui-kit` [`TableDelegate`] backing [`AppHistoryView`].
struct AppHistoryTableDelegate {
    store: Entity<UiStore>,
    slice: Arc<AppHistorySlice>,
    last_rev: u64,
    selected_app_id: Option<Arc<str>>,
    visible_rows: Range<usize>,
    last_scroll_at: Option<Instant>,
    last_viewport_fp: Option<u64>,
    user_action_pending: bool,
    semantic_colors: CachedSemanticColors,
    locale: AppHistoryLocaleCache,
}

impl AppHistoryTableDelegate {
    fn new(store: Entity<UiStore>, cx: &App) -> Self {
        let store_ref = store.read(cx);
        let snap = store_ref.snapshot();
        let slice = Arc::clone(&snap.app_history);

        Self {
            store: store.clone(),
            last_rev: snap.revisions.app_history,
            slice,
            selected_app_id: None,
            visible_rows: 0..0,
            last_scroll_at: None,
            last_viewport_fp: None,
            user_action_pending: false,
            semantic_colors: CachedSemanticColors::new(),
            locale: AppHistoryLocaleCache::build(),
        }
    }

    #[inline]
    fn row_at(&self, row_ix: usize) -> Option<&AppHistoryRecord> {
        let item_idx = *self.slice.visible_indices.get(row_ix)?;
        self.slice.items.get(item_idx)
    }

    /// Locates `app_id` in `slice.visible_indices`, checking `hint_row` in O(1) before scanning.
    #[inline]
    fn find_visible_index_by_id(&self, app_id: &str, hint_row: Option<usize>) -> Option<usize> {
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

    fn trigger_sort(&mut self, col_ix: usize, cx: &mut Context<TableState<Self>>) {
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
        let record = &self.slice.items[item_idx];

        match col {
            AppHistorySortColumn::Name => h_flex()
                .size_full()
                .items_center()
                .truncate()
                .text_xs()
                .font_medium()
                .text_color(fg)
                .child(SharedString::from(Arc::clone(&record.name)))
                .into_any_element(),
            AppHistorySortColumn::CpuTime => heatmap_cell_mono(
                format_cpu_time_shared(record.cpu_time_100ns),
                cpu_time_heatmap_level(record.cpu_time_100ns),
                &colors,
                mono_font,
            )
            .into_any_element(),
            AppHistorySortColumn::Network => heatmap_cell_metric(
                format_bytes_metric(record.network_bytes),
                cumulative_bytes_heatmap_level(record.network_bytes),
                &colors,
                mono_font,
            )
            .into_any_element(),
            AppHistorySortColumn::MeteredNetwork => heatmap_cell_metric(
                format_bytes_metric(record.metered_network_bytes),
                cumulative_bytes_heatmap_level(record.metered_network_bytes),
                &colors,
                mono_font,
            )
            .into_any_element(),
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

/// On-demand cumulative App History view holding an RAII `MonitorDomain::AppHistory` lease.
pub(crate) struct AppHistoryView {
    store: Entity<UiStore>,
    _lease: ScopedMonitorLease,
    table: Entity<TableState<AppHistoryTableDelegate>>,
    table_pane: Entity<DataTablePane<AppHistoryTableDelegate>>,
    since_epoch_secs: Option<u64>,
    history_supported: bool,
    last_locale: LanguagePreference,
    scroll_coalescer: ScrollCoalescer,
    coalesce_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for AppHistoryView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.table.focus_handle(cx)
    }
}

impl AppHistoryView {
    /// Creates a new [`AppHistoryView`], immediately acquiring `MonitorDomain::AppHistory`.
    pub(crate) fn new(store: Entity<UiStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let lease = store.read(cx).acquire_lease(MonitorDomain::AppHistory);
        let (since_epoch_secs, history_supported, last_locale) = {
            let s = store.read(cx);
            let snap = s.snapshot();
            (
                snap.app_history.since_epoch_secs,
                snap.capabilities.contains(CapabilityFlags::APP_HISTORY),
                s.settings().language,
            )
        };

        let table = cx.new(|cx| {
            let delegate = AppHistoryTableDelegate::new(store.clone(), cx);
            TableState::new(delegate, window, cx)
                .row_selectable(true)
                .col_selectable(false)
                .col_resizable(true)
                .col_movable(false)
                .sortable(true)
        });
        let table_pane = cx.new(|_| {
            DataTablePane::new(
                table.clone(),
                Size::Small,
                px(APP_HISTORY_HEADER_HEIGHT),
                false,
                i18n::tr("app_history.title"),
            )
        });

        let mut subscriptions = Vec::with_capacity(2);

        let table_for_store = table.clone();
        subscriptions.push(
            cx.subscribe(&store, move |this, store, event, cx| match event {
                UiStoreEvent::SnapshotUpdated(revs) => {
                    let current_rev = table_for_store.read(cx).delegate().last_rev;
                    if revs.app_history == current_rev {
                        return;
                    }
                    this.handle_snapshot_updated(cx);
                }
                UiStoreEvent::SearchQueryChanged => {
                    table_for_store.update(cx, |table, _| {
                        table.delegate_mut().user_action_pending = true;
                    });
                }
                UiStoreEvent::SettingsChanged => {
                    let next_locale = store.read(cx).settings().language;
                    if this.last_locale != next_locale {
                        this.last_locale = next_locale;
                        table_for_store.update(cx, |table, cx| {
                            let d = table.delegate_mut();
                            d.locale = AppHistoryLocaleCache::build();
                            d.last_viewport_fp = None;
                            table.refresh(cx);
                            cx.notify();
                        });
                        this.table_pane.update(cx, |pane, cx| {
                            pane.set_accessibility_label(i18n::tr("app_history.title"), cx);
                        });
                        cx.notify();
                    }
                }
                UiStoreEvent::ElevationRequired { .. } | UiStoreEvent::CommandFailed { .. } => {}
            }),
        );

        subscriptions.push(cx.subscribe(
            &table,
            |_this, table, event: &TableEvent, cx| match event {
                TableEvent::SelectRow(row_ix) => {
                    table.update(cx, |table, _| {
                        let id = table
                            .delegate()
                            .row_at(*row_ix)
                            .map(|r| Arc::clone(&r.app_id));
                        let d = table.delegate_mut();
                        d.selected_app_id = id;
                        d.last_viewport_fp = None;
                    });
                }
                TableEvent::RightClickedRow(Some(row_ix)) => {
                    table.update(cx, |table, cx| {
                        let id = table
                            .delegate()
                            .row_at(*row_ix)
                            .map(|r| Arc::clone(&r.app_id));
                        {
                            let d = table.delegate_mut();
                            d.selected_app_id = id.clone();
                            d.last_viewport_fp = None;
                        }
                        if id.is_some() && table.selected_row() != Some(*row_ix) {
                            table.set_selected_row(*row_ix, cx);
                        }
                    });
                }
                TableEvent::ClearSelection => {
                    table.update(cx, |table, _| {
                        let d = table.delegate_mut();
                        d.selected_app_id = None;
                        d.last_viewport_fp = None;
                    });
                }
                _ => {}
            },
        ));

        Self {
            store,
            _lease: lease,
            table,
            table_pane,
            since_epoch_secs,
            history_supported,
            last_locale,
            scroll_coalescer: ScrollCoalescer::default(),
            coalesce_task: None,
            _subscriptions: subscriptions,
        }
    }

    fn handle_snapshot_updated(&mut self, cx: &mut Context<Self>) {
        let (next_rev, next_slice, next_supported) = {
            let snap = self.store.read(cx).snapshot();
            (
                snap.revisions.app_history,
                Arc::clone(&snap.app_history),
                snap.capabilities.contains(CapabilityFlags::APP_HISTORY),
            )
        };

        let (bypass, last_scroll_at) = {
            let d = self.table.read(cx).delegate();
            if next_rev == d.last_rev {
                return;
            }
            let bypass = d.slice.revision == 0
                || d.user_action_pending
                || d.slice.sort_column != next_slice.sort_column
                || d.slice.sort_direction != next_slice.sort_direction;
            (bypass, d.last_scroll_at)
        };

        match self
            .scroll_coalescer
            .decide(Instant::now(), last_scroll_at, bypass)
        {
            CoalesceDecision::Defer(wait) => {
                if self.coalesce_task.is_none() {
                    self.coalesce_task = Some(cx.spawn(async move |weak, cx| {
                        cx.background_executor().timer(wait).await;
                        let _ = weak.update(cx, |this, cx| {
                            this.coalesce_task = None;
                            this.handle_snapshot_updated(cx);
                        });
                    }));
                }
            }
            CoalesceDecision::ApplyNow => {
                self.coalesce_task = None;
                let next_since = next_slice.since_epoch_secs;
                self.table.update(cx, |table, cx| {
                    let prev_fp = table.delegate().last_viewport_fp;
                    let sort_changed = {
                        let d = table.delegate_mut();
                        let changed = d.slice.sort_column != next_slice.sort_column
                            || d.slice.sort_direction != next_slice.sort_direction;
                        d.slice = next_slice;
                        d.last_rev = next_rev;
                        d.user_action_pending = false;
                        changed
                    };

                    Self::sync_table_selection(table, cx);

                    let selected_row = table.selected_row();
                    let next_fp = {
                        let d = table.delegate();
                        compute_app_history_viewport_fingerprint(
                            &d.slice,
                            &d.visible_rows,
                            selected_row,
                        )
                    };
                    table.delegate_mut().last_viewport_fp = Some(next_fp);

                    if sort_changed {
                        table.refresh(cx);
                    }
                    if sort_changed || prev_fp != Some(next_fp) {
                        cx.notify();
                    }
                });
                if self.since_epoch_secs != next_since || self.history_supported != next_supported {
                    self.since_epoch_secs = next_since;
                    self.history_supported = next_supported;
                    cx.notify();
                }
            }
        }
    }

    fn sync_table_selection(
        table: &mut TableState<AppHistoryTableDelegate>,
        cx: &mut Context<TableState<AppHistoryTableDelegate>>,
    ) {
        let hint_row = table.selected_row();
        let selected_idx = table
            .delegate()
            .selected_app_id
            .as_deref()
            .and_then(|id| table.delegate().find_visible_index_by_id(id, hint_row));
        if let Some(idx) = selected_idx {
            if hint_row != Some(idx) {
                table.set_selected_row(idx, cx);
            }
        } else if hint_row.is_some() {
            table.clear_selection(cx);
        }
    }
}

impl Render for AppHistoryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        debug_assert_eq!(self._lease.domain(), MonitorDomain::AppHistory);

        let theme = cx.theme();
        let border = theme.border;
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let supported = self.history_supported;
        let since_label = self
            .since_epoch_secs
            .map(|secs| i18n::app_history_since(&format_epoch_datetime(secs)));
        let store_clear = self.store.clone();

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
                    .items_center()
                    .flex_wrap()
                    .gap_3()
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .text_color(fg)
                            .child(i18n::tr("app_history.title")),
                    )
                    .when_some(since_label, |row, label| {
                        row.child(div().text_xs().text_color(muted_fg).child(label))
                    }),
            )
            .child(
                Button::new("app-history-delete")
                    .small()
                    .danger()
                    .outline()
                    .icon(IconName::Delete)
                    .label(i18n::tr("app_history.delete_history"))
                    .disabled(!supported)
                    .on_click(move |_, window, cx| {
                        open_clear_history_dialog(&store_clear, window, cx);
                    }),
            );

        v_flex()
            .id("app-history-view-root")
            .size_full()
            .min_h_0()
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
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use gpui_kit::{TestAppContext, VisualTestContext};
    use loom_config::{AppSettings, ConfigStore};
    use loom_engine::EngineCoordinator;
    use loom_pal::{NativeBackend, PlatformBackend};
    use loom_types::{
        AppHistoryRecord, AppHistorySlice, AppHistorySortColumn, MonitorDomain, SortDirection,
    };

    use super::*;

    fn sample_record(
        app_id: &str,
        name: &str,
        cpu_secs: u64,
        net_bytes: u64,
        metered_bytes: u64,
    ) -> AppHistoryRecord {
        AppHistoryRecord {
            app_id: Arc::from(app_id),
            name: Arc::from(name),
            cpu_time_100ns: cpu_secs * 10_000_000,
            network_bytes: net_bytes,
            metered_network_bytes: metered_bytes,
        }
    }

    #[test]
    fn format_epoch_datetime_formats_standard_and_leap_dates() {
        assert_eq!(format_epoch_datetime(0), "1970-01-01 00:00");
        // 2024-02-29 12:34:00 UTC
        assert_eq!(format_epoch_datetime(1_709_210_040), "2024-02-29 12:34");
        // 2026-11-27 06:58:30 UTC
        assert_eq!(format_epoch_datetime(1_795_762_710), "2026-11-27 06:58");
    }

    #[test]
    fn app_history_viewport_fingerprint_quantizes_metrics_and_ignores_offscreen() {
        let mut slice = AppHistorySlice {
            revision: 1,
            sort_column: AppHistorySortColumn::CpuTime,
            sort_direction: SortDirection::Descending,
            items: vec![
                sample_record("app.alpha", "Alpha", 120, 10 * 1024 * 1024, 0),
                sample_record("app.beta", "Beta", 3_600, 500_000, 10_000),
                sample_record("app.gamma", "Gamma", 30, 2_000_000, 0),
            ],
            visible_indices: vec![1, 0, 2],
            ..AppHistorySlice::default()
        };

        let fp_initial = compute_app_history_viewport_fingerprint(&slice, &(0..2), Some(0));

        // Sub-second CPU tick delta stays in the same whole-second bucket.
        slice.items[1].cpu_time_100ns += 5_000_000;
        assert_eq!(
            fp_initial,
            compute_app_history_viewport_fingerprint(&slice, &(0..2), Some(0))
        );

        // Off-screen row update (visible index 2 -> item 2) does not invalidate fingerprint.
        slice.items[2].network_bytes += 100 * 1024 * 1024;
        assert_eq!(
            fp_initial,
            compute_app_history_viewport_fingerprint(&slice, &(0..2), Some(0))
        );

        // Whole-second visible CPU delta invalidates the fingerprint.
        slice.items[1].cpu_time_100ns += 10_000_000;
        assert_ne!(
            fp_initial,
            compute_app_history_viewport_fingerprint(&slice, &(0..2), Some(0))
        );
    }

    #[test]
    fn app_history_scroll_coalescer_enforces_quiet_window_cap_and_bypass() {
        let mut coalescer = ScrollCoalescer::default();
        let t0 = Instant::now();
        assert_eq!(
            coalescer.decide(t0 + Duration::from_millis(30), Some(t0), false),
            CoalesceDecision::Defer(Duration::from_millis(120))
        );
        assert_eq!(
            coalescer.decide(t0 + Duration::from_millis(180), Some(t0), false),
            CoalesceDecision::ApplyNow
        );
        assert_eq!(
            coalescer.decide(t0 + Duration::from_millis(10), Some(t0), true),
            CoalesceDecision::ApplyNow
        );
    }

    #[gpui_kit::test]
    fn app_history_view_acquires_lease_dispatches_sort_and_releases_on_drop(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().expect("tempdir");
        let config_store = ConfigStore::new(dir.path().join("settings.toml"));
        let settings = AppSettings::default();
        let backend = NativeBackend::initialize().expect("initialize native backend");
        let coordinator =
            Arc::new(EngineCoordinator::new(backend, settings.clone()).expect("start coordinator"));

        assert!(!coordinator.leases().is_active(MonitorDomain::AppHistory));

        let store = cx.new(|cx| UiStore::new(coordinator.clone(), config_store, settings, cx));

        let mut view_handle = None;
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                let view = cx.new(|cx| AppHistoryView::new(store.clone(), window, cx));
                view_handle = Some(view.clone());
                cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
            })
            .expect("open app history test window")
        });
        let view = view_handle.expect("AppHistoryView constructed");

        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.update(|window, cx| window.draw(cx).clear(cx));

        assert!(
            coordinator.leases().is_active(MonitorDomain::AppHistory),
            "mounting AppHistoryView must acquire MonitorDomain::AppHistory lease"
        );

        view.update(&mut vcx, |view, cx| {
            view.table.update(cx, |table, cx| {
                table.delegate_mut().trigger_sort(2, cx);
            });
        });
        vcx.run_until_parked();

        drop(view);
        vcx.update(|window, _| window.remove_window());
        vcx.run_until_parked();

        assert!(
            !coordinator.leases().is_active(MonitorDomain::AppHistory),
            "dropping AppHistoryView must release MonitorDomain::AppHistory lease"
        );

        coordinator.shutdown();
    }
}
