//! On-demand App History vertical feature slice gated by an RAII [`ScopedMonitorLease`].
//!
//! Splits the static toolbar + collection start date indicator ([`AppHistoryView`])
//! from the 2D-virtualized `gpui-kit` `DataTable` (`Entity<TableState<AppHistoryTableDelegate>>`)
//! so hidden routes incur zero background polling or history aggregation overhead.

use std::cmp::Ordering;
use std::sync::Arc;

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
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use loom_types::{
    AppHistoryRecord, AppHistorySlice, CapabilityFlags, LanguagePreference, MonitorDomain,
    SortDirection,
};
use loom_ui_kit::{
    HeatmapLevel, LoomSemanticColors, format_bytes_shared, format_cpu_time_shared, heatmap_cell,
};

use crate::bridge::{ScopedMonitorLease, UiCommands, UiStore, UiStoreEvent};
use crate::features::table_columns::DataTablePane;
use crate::i18n;

const APP_HISTORY_HEADER_HEIGHT: f32 = 32.0;

/// Sortable columns displayed by [`AppHistoryView`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AppHistorySortColumn {
    Name,
    CpuTime,
    Network,
    MeteredNetwork,
}

impl AppHistorySortColumn {
    const ALL: [Self; 4] = [
        Self::Name,
        Self::CpuTime,
        Self::Network,
        Self::MeteredNetwork,
    ];

    #[inline]
    const fn slug(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::CpuTime => "cpu_time",
            Self::Network => "network",
            Self::MeteredNetwork => "metered_network",
        }
    }

    #[inline]
    const fn width(self) -> f32 {
        match self {
            Self::Name => 320.0,
            Self::CpuTime => 150.0,
            Self::Network => 150.0,
            Self::MeteredNetwork => 170.0,
        }
    }

    #[inline]
    const fn min_width(self) -> f32 {
        match self {
            Self::Name => 150.0,
            Self::CpuTime => 100.0,
            Self::Network => 100.0,
            Self::MeteredNetwork => 116.0,
        }
    }

    #[inline]
    const fn right_aligned(self) -> bool {
        matches!(self, Self::CpuTime | Self::Network | Self::MeteredNetwork)
    }

    #[inline]
    const fn is_heatmap(self) -> bool {
        matches!(self, Self::CpuTime | Self::Network | Self::MeteredNetwork)
    }

    #[inline]
    const fn default_direction(self) -> SortDirection {
        match self {
            Self::CpuTime | Self::Network | Self::MeteredNetwork => SortDirection::Descending,
            Self::Name => SortDirection::Ascending,
        }
    }

    fn label(self) -> String {
        match self {
            Self::Name => i18n::tr("app_history.col_name"),
            Self::CpuTime => i18n::tr("app_history.col_cpu_time"),
            Self::Network => i18n::tr("app_history.col_network"),
            Self::MeteredNetwork => i18n::tr("app_history.col_metered_network"),
        }
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

#[inline]
fn contains_icase(haystack: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if haystack.is_ascii() && needle_lower.is_ascii() {
        let h = haystack.as_bytes();
        let n = needle_lower.as_bytes();
        if n.len() > h.len() {
            return false;
        }
        return h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n));
    }
    haystack.to_lowercase().contains(needle_lower)
}

#[inline]
fn cmp_icase(a: &str, b: &str) -> Ordering {
    if a.is_ascii() && b.is_ascii() {
        for (&ab, &bb) in a.as_bytes().iter().zip(b.as_bytes().iter()) {
            let c = ab.to_ascii_lowercase().cmp(&bb.to_ascii_lowercase());
            if c != Ordering::Equal {
                return c;
            }
        }
        return a.len().cmp(&b.len());
    }
    a.to_lowercase().cmp(&b.to_lowercase())
}

/// Filters and sorts cumulative application history records into visible item indices.
pub(super) fn filter_and_sort_app_history(
    items: &[AppHistoryRecord],
    search_query: &str,
    sort_col: AppHistorySortColumn,
    sort_dir: SortDirection,
) -> Vec<usize> {
    let query_lower = search_query.trim().to_lowercase();
    let mut indices: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, record)| {
            query_lower.is_empty()
                || contains_icase(&record.name, &query_lower)
                || contains_icase(&record.app_id, &query_lower)
        })
        .map(|(idx, _)| idx)
        .collect();

    indices.sort_by(|&a_idx, &b_idx| {
        let a = &items[a_idx];
        let b = &items[b_idx];
        let ord = match sort_col {
            AppHistorySortColumn::Name => cmp_icase(&a.name, &b.name),
            AppHistorySortColumn::CpuTime => a.cpu_time_100ns.cmp(&b.cpu_time_100ns),
            AppHistorySortColumn::Network => a.network_bytes.cmp(&b.network_bytes),
            AppHistorySortColumn::MeteredNetwork => {
                a.metered_network_bytes.cmp(&b.metered_network_bytes)
            }
        };
        let ordered = match sort_dir {
            SortDirection::Ascending => ord,
            SortDirection::Descending => ord.reverse(),
        };
        ordered
            .then_with(|| cmp_icase(&a.name, &b.name))
            .then_with(|| a.app_id.cmp(&b.app_id))
    });

    indices
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
    loading: SharedString,
    empty: SharedString,
}

impl AppHistoryLocaleCache {
    fn build() -> Self {
        Self {
            loading: i18n::tr("app_history.loading").into(),
            empty: i18n::tr("app_history.empty").into(),
        }
    }
}

/// 2D-virtualized `gpui-kit` [`TableDelegate`] backing [`AppHistoryView`].
struct AppHistoryTableDelegate {
    store: Entity<UiStore>,
    slice: Arc<AppHistorySlice>,
    last_rev: u64,
    search_query: String,
    sort_col: AppHistorySortColumn,
    sort_dir: SortDirection,
    visible_indices: Vec<usize>,
    selected_app_id: Option<Arc<str>>,
    locale: AppHistoryLocaleCache,
}

impl AppHistoryTableDelegate {
    fn new(store: Entity<UiStore>, cx: &App) -> Self {
        let store_ref = store.read(cx);
        let snap = store_ref.snapshot();
        let slice = Arc::clone(&snap.app_history);
        let search_query = store_ref.search_query().to_owned();
        let sort_col = AppHistorySortColumn::CpuTime;
        let sort_dir = SortDirection::Descending;
        let visible_indices =
            filter_and_sort_app_history(&slice.items, &search_query, sort_col, sort_dir);

        Self {
            store: store.clone(),
            last_rev: snap.revisions.app_history,
            slice,
            search_query,
            sort_col,
            sort_dir,
            visible_indices,
            selected_app_id: None,
            locale: AppHistoryLocaleCache::build(),
        }
    }

    fn rebuild_rows(&mut self) {
        self.visible_indices = filter_and_sort_app_history(
            &self.slice.items,
            &self.search_query,
            self.sort_col,
            self.sort_dir,
        );
    }

    fn row_at(&self, row_ix: usize) -> Option<&AppHistoryRecord> {
        let item_idx = *self.visible_indices.get(row_ix)?;
        self.slice.items.get(item_idx)
    }

    fn find_visible_index_by_id(&self, app_id: &str) -> Option<usize> {
        self.visible_indices
            .iter()
            .position(|&idx| self.slice.items[idx].app_id.as_ref() == app_id)
    }

    fn toggle_sort(&mut self, col_ix: usize) {
        let Some(&target) = AppHistorySortColumn::ALL.get(col_ix) else {
            return;
        };
        if self.sort_col == target {
            self.sort_dir = self.sort_dir.toggle();
        } else {
            self.sort_col = target;
            self.sort_dir = target.default_direction();
        }
        self.rebuild_rows();
    }
}

impl TableDelegate for AppHistoryTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        AppHistorySortColumn::ALL.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.visible_indices.len()
    }

    fn loading(&self, _cx: &App) -> bool {
        self.slice.revision == 0
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        let Some(&col) = AppHistorySortColumn::ALL.get(col_ix) else {
            return Column::default();
        };
        let mut column = Column::new(col.slug(), col.label())
            .width(px(col.width()))
            .min_width(px(col.min_width()))
            .selectable(false)
            .movable(false);

        if col == AppHistorySortColumn::Name {
            column = column.fixed_left();
        }
        if col.right_aligned() {
            column = column.text_right();
        }
        if col.is_heatmap() {
            column = column.p_0();
        }
        if col == self.sort_col {
            column = column.sort(to_column_sort(self.sort_dir));
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
        self.toggle_sort(col_ix);
        cx.notify();
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
        let Some(&col) = AppHistorySortColumn::ALL.get(col_ix) else {
            return Empty.into_any_element();
        };
        let is_active = col == self.sort_col;
        let fg = cx.theme().foreground;
        let muted_fg = cx.theme().muted_foreground;

        h_flex()
            .id(("app-history-th", col_ix))
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .when(col.is_heatmap(), |d| d.px_2())
            .when(col.right_aligned(), |d| d.justify_end())
            .text_xs()
            .text_color(if is_active { fg } else { muted_fg })
            .when(is_active, |d| d.font_semibold())
            .cursor_pointer()
            .on_click(cx.listener(move |table, _, _, cx| {
                table.delegate_mut().toggle_sort(col_ix);
                let sel = table
                    .delegate()
                    .selected_app_id
                    .as_deref()
                    .and_then(|id| table.delegate().find_visible_index_by_id(id));
                if let Some(idx) = sel {
                    table.set_selected_row(idx, cx);
                } else {
                    table.clear_selection(cx);
                }
                table.refresh(cx);
                cx.notify();
            }))
            .child(div().min_w_0().truncate().child(col.label()))
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
        let Some(record) = self.row_at(row_ix) else {
            return Empty.into_any_element();
        };
        let Some(&col) = AppHistorySortColumn::ALL.get(col_ix) else {
            return Empty.into_any_element();
        };

        let theme = cx.theme();
        let colors = LoomSemanticColors::from_theme(theme);
        let fg = theme.foreground;

        match col {
            AppHistorySortColumn::Name => h_flex()
                .size_full()
                .items_center()
                .overflow_hidden()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .font_medium()
                        .text_color(fg)
                        .child(SharedString::from(Arc::clone(&record.name))),
                )
                .into_any_element(),
            AppHistorySortColumn::CpuTime => heatmap_cell(
                format_cpu_time_shared(record.cpu_time_100ns),
                cpu_time_heatmap_level(record.cpu_time_100ns),
                &colors,
            )
            .into_any_element(),
            AppHistorySortColumn::Network => heatmap_cell(
                format_bytes_shared(record.network_bytes),
                cumulative_bytes_heatmap_level(record.network_bytes),
                &colors,
            )
            .into_any_element(),
            AppHistorySortColumn::MeteredNetwork => heatmap_cell(
                format_bytes_shared(record.metered_network_bytes),
                cumulative_bytes_heatmap_level(record.metered_network_bytes),
                &colors,
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
                    let (next_slice, next_supported) = {
                        let snap = store.read(cx).snapshot();
                        (
                            Arc::clone(&snap.app_history),
                            snap.capabilities.contains(CapabilityFlags::APP_HISTORY),
                        )
                    };
                    let next_since = next_slice.since_epoch_secs;
                    table_for_store.update(cx, |table, cx| {
                        let d = table.delegate_mut();
                        d.slice = next_slice;
                        d.last_rev = revs.app_history;
                        d.rebuild_rows();
                        Self::sync_table_selection(table, cx);
                        cx.notify();
                    });
                    if this.since_epoch_secs != next_since
                        || this.history_supported != next_supported
                    {
                        this.since_epoch_secs = next_since;
                        this.history_supported = next_supported;
                        cx.notify();
                    }
                }
                UiStoreEvent::SearchQueryChanged => {
                    let next_query = store.read(cx).search_query().to_owned();
                    table_for_store.update(cx, |table, cx| {
                        let d = table.delegate_mut();
                        d.search_query = next_query;
                        d.rebuild_rows();
                        Self::sync_table_selection(table, cx);
                        cx.notify();
                    });
                }
                UiStoreEvent::SettingsChanged => {
                    let next_locale = store.read(cx).settings().language;
                    if this.last_locale != next_locale {
                        this.last_locale = next_locale;
                        table_for_store.update(cx, |table, cx| {
                            table.delegate_mut().locale = AppHistoryLocaleCache::build();
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
                        table.delegate_mut().selected_app_id = id;
                    });
                }
                TableEvent::RightClickedRow(Some(row_ix)) => {
                    table.update(cx, |table, cx| {
                        let id = table
                            .delegate()
                            .row_at(*row_ix)
                            .map(|r| Arc::clone(&r.app_id));
                        table.delegate_mut().selected_app_id = id.clone();
                        if id.is_some() && table.selected_row() != Some(*row_ix) {
                            table.set_selected_row(*row_ix, cx);
                        }
                    });
                }
                TableEvent::ClearSelection => {
                    table.update(cx, |table, _| {
                        table.delegate_mut().selected_app_id = None;
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
            _subscriptions: subscriptions,
        }
    }

    fn sync_table_selection(
        table: &mut TableState<AppHistoryTableDelegate>,
        cx: &mut Context<TableState<AppHistoryTableDelegate>>,
    ) {
        let selected_idx = table
            .delegate()
            .selected_app_id
            .as_deref()
            .and_then(|id| table.delegate().find_visible_index_by_id(id));
        if let Some(idx) = selected_idx {
            if table.selected_row() != Some(idx) {
                table.set_selected_row(idx, cx);
            }
        } else if table.selected_row().is_some() {
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

    use gpui_kit::{TestAppContext, VisualTestContext};
    use loom_config::{AppSettings, ConfigStore};
    use loom_engine::EngineCoordinator;
    use loom_pal::{NativeBackend, PlatformBackend};
    use loom_types::{AppHistoryRecord, MonitorDomain, SortDirection};

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
    fn app_history_filter_and_sort_cover_all_columns() {
        let items = vec![
            sample_record("app.alpha", "Alpha", 120, 10_000, 500),
            sample_record("app.beta", "Beta", 3_600, 500_000, 10_000),
            sample_record("app.gamma", "Gamma", 30, 2_000_000, 0),
        ];

        // CPU time descending: Beta -> Alpha -> Gamma.
        assert_eq!(
            filter_and_sort_app_history(
                &items,
                "",
                AppHistorySortColumn::CpuTime,
                SortDirection::Descending,
            ),
            vec![1, 0, 2]
        );

        // Network descending: Gamma -> Beta -> Alpha.
        assert_eq!(
            filter_and_sort_app_history(
                &items,
                "",
                AppHistorySortColumn::Network,
                SortDirection::Descending,
            ),
            vec![2, 1, 0]
        );

        // Metered network descending: Beta -> Alpha -> Gamma.
        assert_eq!(
            filter_and_sort_app_history(
                &items,
                "",
                AppHistorySortColumn::MeteredNetwork,
                SortDirection::Descending,
            ),
            vec![1, 0, 2]
        );

        // Search matches across name and app_id.
        assert_eq!(
            filter_and_sort_app_history(
                &items,
                "gam",
                AppHistorySortColumn::Name,
                SortDirection::Ascending,
            ),
            vec![2]
        );
    }

    #[gpui_kit::test]
    fn app_history_view_acquires_and_releases_monitor_lease_on_mount_and_drop(
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
