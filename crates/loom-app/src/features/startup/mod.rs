//! On-demand Startup Applications vertical feature slice gated by an RAII [`ScopedMonitorLease`].
//!
//! Splits the static toolbar + Last BIOS time indicator ([`StartupView`]) from the
//! 2D-virtualized `gpui-kit` `DataTable` (`Entity<TableState<StartupTableDelegate>>`)
//! so hidden routes incur zero registry/folder/UWP polling overhead.

use std::cmp::Ordering;
use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableEvent, TableState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, Size, StyledExt as _, h_flex,
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Empty, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use loom_types::{
    CapabilityFlags, LanguagePreference, MonitorDomain, SortDirection, StartupEntry, StartupImpact,
    StartupSlice,
};

use crate::bridge::{ScopedMonitorLease, UiCommands, UiStore, UiStoreEvent};
use crate::features::table_columns::DataTablePane;
use crate::i18n;

const STARTUP_HEADER_HEIGHT: f32 = 32.0;

/// Sortable columns displayed by [`StartupView`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StartupSortColumn {
    Name,
    Publisher,
    Status,
    Impact,
}

impl StartupSortColumn {
    const ALL: [Self; 4] = [Self::Name, Self::Publisher, Self::Status, Self::Impact];

    #[inline]
    const fn slug(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Publisher => "publisher",
            Self::Status => "status",
            Self::Impact => "impact",
        }
    }

    #[inline]
    const fn width(self) -> f32 {
        match self {
            Self::Name => 280.0,
            Self::Publisher => 220.0,
            Self::Status => 120.0,
            Self::Impact => 150.0,
        }
    }

    #[inline]
    const fn min_width(self) -> f32 {
        match self {
            Self::Name => 140.0,
            Self::Publisher => 110.0,
            Self::Status => 84.0,
            Self::Impact => 96.0,
        }
    }

    #[inline]
    const fn default_direction(self) -> SortDirection {
        match self {
            Self::Status | Self::Impact => SortDirection::Descending,
            Self::Name | Self::Publisher => SortDirection::Ascending,
        }
    }

    fn label(self) -> String {
        match self {
            Self::Name => i18n::tr("startup.col_name"),
            Self::Publisher => i18n::tr("startup.col_publisher"),
            Self::Status => i18n::tr("startup.col_status"),
            Self::Impact => i18n::tr("startup.col_impact"),
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

#[inline]
const fn startup_impact_rank(impact: StartupImpact) -> u8 {
    match impact {
        StartupImpact::High => 4,
        StartupImpact::Medium => 3,
        StartupImpact::Low => 2,
        StartupImpact::NotMeasured => 1,
        StartupImpact::None => 0,
    }
}

/// Formats firmware POST duration in milliseconds into a tenths-of-second decimal string
/// (e.g., `4250` -> `"4.3"`, `0` -> `"0.0"`).
#[inline]
pub(super) fn format_bios_time_seconds(ms: u64) -> String {
    let tenths = ms.saturating_add(50) / 100;
    format!("{}.{}", tenths / 10, tenths % 10)
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

/// Returns `true` when `entry` matches the case-insensitive search query across
/// `name`, `publisher`, and `command`.
pub(super) fn matches_startup_query(entry: &StartupEntry, query_lower: &str) -> bool {
    if query_lower.is_empty() {
        return true;
    }
    if contains_icase(&entry.name, query_lower) {
        return true;
    }
    if entry
        .publisher
        .as_deref()
        .is_some_and(|p| contains_icase(p, query_lower))
    {
        return true;
    }
    if entry
        .command
        .as_deref()
        .is_some_and(|c| contains_icase(c, query_lower))
    {
        return true;
    }
    false
}

/// Filters and sorts startup entries into visible item indices.
pub(super) fn filter_and_sort_startup(
    items: &[StartupEntry],
    search_query: &str,
    sort_col: StartupSortColumn,
    sort_dir: SortDirection,
) -> Vec<usize> {
    let query_lower = search_query.trim().to_lowercase();
    let mut indices: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, entry)| matches_startup_query(entry, &query_lower))
        .map(|(idx, _)| idx)
        .collect();

    indices.sort_by(|&a_idx, &b_idx| {
        let a = &items[a_idx];
        let b = &items[b_idx];
        let ord = match sort_col {
            StartupSortColumn::Name => cmp_icase(&a.name, &b.name),
            StartupSortColumn::Publisher => cmp_icase(
                a.publisher.as_deref().unwrap_or(""),
                b.publisher.as_deref().unwrap_or(""),
            ),
            StartupSortColumn::Status => a.enabled.cmp(&b.enabled),
            StartupSortColumn::Impact => {
                startup_impact_rank(a.impact).cmp(&startup_impact_rank(b.impact))
            }
        };
        let ordered = match sort_dir {
            SortDirection::Ascending => ord,
            SortDirection::Descending => ord.reverse(),
        };
        ordered
            .then_with(|| cmp_icase(&a.name, &b.name))
            .then_with(|| a.id.cmp(&b.id))
    });

    indices
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct StartupToolbarState {
    can_enable: bool,
    can_disable: bool,
}

/// Cached localized strings for [`StartupTableDelegate`].
struct StartupLocaleCache {
    enabled: SharedString,
    disabled: SharedString,
    impact_high: SharedString,
    impact_medium: SharedString,
    impact_low: SharedString,
    impact_not_measured: SharedString,
    impact_none: SharedString,
    loading: SharedString,
    empty: SharedString,
}

impl StartupLocaleCache {
    fn build() -> Self {
        Self {
            enabled: i18n::tr("startup.status_enabled").into(),
            disabled: i18n::tr("startup.status_disabled").into(),
            impact_high: i18n::startup_impact_label(StartupImpact::High).into(),
            impact_medium: i18n::startup_impact_label(StartupImpact::Medium).into(),
            impact_low: i18n::startup_impact_label(StartupImpact::Low).into(),
            impact_not_measured: i18n::startup_impact_label(StartupImpact::NotMeasured).into(),
            impact_none: i18n::startup_impact_label(StartupImpact::None).into(),
            loading: i18n::tr("startup.loading").into(),
            empty: i18n::tr("startup.empty").into(),
        }
    }

    #[inline]
    fn impact_label(&self, impact: StartupImpact) -> SharedString {
        match impact {
            StartupImpact::High => self.impact_high.clone(),
            StartupImpact::Medium => self.impact_medium.clone(),
            StartupImpact::Low => self.impact_low.clone(),
            StartupImpact::NotMeasured => self.impact_not_measured.clone(),
            StartupImpact::None => self.impact_none.clone(),
        }
    }
}

/// 2D-virtualized `gpui-kit` [`TableDelegate`] backing [`StartupView`].
struct StartupTableDelegate {
    store: Entity<UiStore>,
    slice: Arc<StartupSlice>,
    capabilities: CapabilityFlags,
    last_rev: u64,
    search_query: String,
    sort_col: StartupSortColumn,
    sort_dir: SortDirection,
    visible_indices: Vec<usize>,
    selected_id: Option<Arc<str>>,
    locale: StartupLocaleCache,
}

impl StartupTableDelegate {
    fn new(store: Entity<UiStore>, cx: &App) -> Self {
        let store_ref = store.read(cx);
        let snap = store_ref.snapshot();
        let slice = Arc::clone(&snap.startup);
        let capabilities = snap.capabilities;
        let search_query = store_ref.search_query().to_owned();
        let sort_col = StartupSortColumn::Name;
        let sort_dir = SortDirection::Ascending;
        let visible_indices =
            filter_and_sort_startup(&slice.items, &search_query, sort_col, sort_dir);

        Self {
            store: store.clone(),
            last_rev: snap.revisions.startup,
            slice,
            capabilities,
            search_query,
            sort_col,
            sort_dir,
            visible_indices,
            selected_id: None,
            locale: StartupLocaleCache::build(),
        }
    }

    fn rebuild_rows(&mut self) {
        self.visible_indices = filter_and_sort_startup(
            &self.slice.items,
            &self.search_query,
            self.sort_col,
            self.sort_dir,
        );
    }

    fn row_at(&self, row_ix: usize) -> Option<&StartupEntry> {
        let item_idx = *self.visible_indices.get(row_ix)?;
        self.slice.items.get(item_idx)
    }

    fn find_visible_index_by_id(&self, id: &str) -> Option<usize> {
        self.visible_indices
            .iter()
            .position(|&idx| self.slice.items[idx].id.as_ref() == id)
    }

    fn selected_entry(&self) -> Option<&StartupEntry> {
        let id = self.selected_id.as_deref()?;
        let row_ix = self.find_visible_index_by_id(id)?;
        self.row_at(row_ix)
    }

    fn toolbar_state(&self) -> StartupToolbarState {
        if !self.capabilities.contains(CapabilityFlags::STARTUP_APPS) {
            return StartupToolbarState::default();
        }
        let Some(entry) = self.selected_entry() else {
            return StartupToolbarState::default();
        };
        StartupToolbarState {
            can_enable: !entry.enabled,
            can_disable: entry.enabled,
        }
    }

    fn toggle_sort(&mut self, col_ix: usize) {
        let Some(&target) = StartupSortColumn::ALL.get(col_ix) else {
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

impl TableDelegate for StartupTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        StartupSortColumn::ALL.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.visible_indices.len()
    }

    fn loading(&self, _cx: &App) -> bool {
        self.slice.revision == 0
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        let Some(&col) = StartupSortColumn::ALL.get(col_ix) else {
            return Column::default();
        };
        let mut column = Column::new(col.slug(), col.label())
            .width(px(col.width()))
            .min_width(px(col.min_width()))
            .selectable(false)
            .movable(false);

        if col == StartupSortColumn::Name {
            column = column.fixed_left();
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
            .id("startup-column-header")
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
        let Some(&col) = StartupSortColumn::ALL.get(col_ix) else {
            return Empty.into_any_element();
        };
        let is_active = col == self.sort_col;
        let fg = cx.theme().foreground;
        let muted_fg = cx.theme().muted_foreground;

        h_flex()
            .id(("startup-th", col_ix))
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .text_xs()
            .text_color(if is_active { fg } else { muted_fg })
            .when(is_active, |d| d.font_semibold())
            .cursor_pointer()
            .on_click(cx.listener(move |table, _, _, cx| {
                table.delegate_mut().toggle_sort(col_ix);
                let sel = table
                    .delegate()
                    .selected_id
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
        div().id(("startup-row", row_ix)).text_xs()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(entry) = self.row_at(row_ix) else {
            return Empty.into_any_element();
        };
        let Some(&col) = StartupSortColumn::ALL.get(col_ix) else {
            return Empty.into_any_element();
        };

        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let primary = theme.primary;
        let warning = theme.warning;
        let danger = theme.danger;

        match col {
            StartupSortColumn::Name => h_flex()
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
                        .child(SharedString::from(Arc::clone(&entry.name))),
                )
                .into_any_element(),
            StartupSortColumn::Publisher => {
                let text = entry
                    .publisher
                    .as_ref()
                    .filter(|s| !s.is_empty())
                    .map(|s| SharedString::from(Arc::clone(s)))
                    .unwrap_or_else(|| SharedString::new_static("—"));
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
                            .child(text),
                    )
                    .into_any_element()
            }
            StartupSortColumn::Status => {
                let (label, color) = if entry.enabled {
                    (self.locale.enabled.clone(), primary)
                } else {
                    (self.locale.disabled.clone(), muted_fg)
                };
                h_flex()
                    .size_full()
                    .items_center()
                    .overflow_hidden()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(color)
                            .child(label),
                    )
                    .into_any_element()
            }
            StartupSortColumn::Impact => {
                let color = match entry.impact {
                    StartupImpact::High => danger,
                    StartupImpact::Medium => warning,
                    StartupImpact::Low | StartupImpact::NotMeasured | StartupImpact::None => {
                        muted_fg
                    }
                };
                h_flex()
                    .size_full()
                    .items_center()
                    .overflow_hidden()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(color)
                            .child(self.locale.impact_label(entry.impact)),
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
        let Some(entry) = self.row_at(row_ix).cloned() else {
            return menu;
        };

        let supported = self.capabilities.contains(CapabilityFlags::STARTUP_APPS);
        let has_file_path = entry
            .file_path
            .as_deref()
            .is_some_and(|p| !p.trim().is_empty());

        let store_enable = self.store.clone();
        let id_enable = entry.id.to_string();

        let store_disable = self.store.clone();
        let id_disable = entry.id.to_string();

        let store_open_loc = self.store.clone();
        let path_open_loc = entry.file_path.clone();

        let store_props = self.store.clone();
        let path_props = entry.file_path.clone();

        let store_search = self.store.clone();
        let search_query = entry.name.to_string();

        menu.item(
            PopupMenuItem::new(i18n::tr("startup.menu_enable"))
                .icon(IconName::CircleCheck)
                .disabled(!supported || entry.enabled)
                .on_click(move |_, _, cx| {
                    UiCommands::set_startup_enabled(&store_enable, id_enable.clone(), true, cx);
                }),
        )
        .item(
            PopupMenuItem::new(i18n::tr("startup.menu_disable"))
                .icon(IconName::Ban)
                .disabled(!supported || !entry.enabled)
                .on_click(move |_, _, cx| {
                    UiCommands::set_startup_enabled(&store_disable, id_disable.clone(), false, cx);
                }),
        )
        .separator()
        .item(
            PopupMenuItem::new(i18n::tr("startup.menu_open_file_location"))
                .icon(IconName::FolderOpen)
                .disabled(!has_file_path)
                .on_click(move |_, _, cx| {
                    if let Some(ref path) = path_open_loc {
                        UiCommands::open_file_location(
                            &store_open_loc,
                            PathBuf::from(path.as_ref()),
                            cx,
                        );
                    }
                }),
        )
        .item(
            PopupMenuItem::new(i18n::tr("startup.menu_properties"))
                .icon(IconName::Info)
                .disabled(!has_file_path)
                .on_click(move |_, _, cx| {
                    if let Some(ref path) = path_props {
                        UiCommands::show_file_properties(
                            &store_props,
                            PathBuf::from(path.as_ref()),
                            cx,
                        );
                    }
                }),
        )
        .separator()
        .item(
            PopupMenuItem::new(i18n::tr("startup.menu_search_online"))
                .icon(IconName::Globe)
                .on_click(move |_, _, cx| {
                    UiCommands::search_online(&store_search, search_query.clone(), cx);
                }),
        )
    }

    fn row_label(&self, row_ix: usize, _cx: &App) -> Option<SharedString> {
        let entry = self.row_at(row_ix)?;
        let status = if entry.enabled {
            &self.locale.enabled
        } else {
            &self.locale.disabled
        };
        Some(format!("{} ({})", entry.name, status).into())
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

/// On-demand startup applications manager holding an RAII `MonitorDomain::Startup` lease.
pub(crate) struct StartupView {
    store: Entity<UiStore>,
    _lease: ScopedMonitorLease,
    table: Entity<TableState<StartupTableDelegate>>,
    table_pane: Entity<DataTablePane<StartupTableDelegate>>,
    last_bios_time_ms: Option<u64>,
    toolbar_state: StartupToolbarState,
    last_locale: LanguagePreference,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for StartupView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.table.focus_handle(cx)
    }
}

impl StartupView {
    /// Creates a new [`StartupView`], immediately acquiring `MonitorDomain::Startup`.
    pub(crate) fn new(store: Entity<UiStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let lease = store.read(cx).acquire_lease(MonitorDomain::Startup);
        let (last_bios_time_ms, last_locale) = {
            let s = store.read(cx);
            (
                s.snapshot().startup.last_bios_time_ms,
                s.settings().language,
            )
        };

        let table = cx.new(|cx| {
            let delegate = StartupTableDelegate::new(store.clone(), cx);
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
                px(STARTUP_HEADER_HEIGHT),
                false,
                i18n::tr("startup.title"),
            )
        });
        let toolbar_state = table.read(cx).delegate().toolbar_state();

        let mut subscriptions = Vec::with_capacity(2);

        let table_for_store = table.clone();
        subscriptions.push(
            cx.subscribe(&store, move |this, store, event, cx| match event {
                UiStoreEvent::SnapshotUpdated(revs) => {
                    let current_rev = table_for_store.read(cx).delegate().last_rev;
                    if revs.startup == current_rev {
                        return;
                    }
                    let (next_slice, next_caps) = {
                        let snap = store.read(cx).snapshot();
                        (Arc::clone(&snap.startup), snap.capabilities)
                    };
                    let next_bios = next_slice.last_bios_time_ms;
                    let next_toolbar = table_for_store.update(cx, |table, cx| {
                        let d = table.delegate_mut();
                        d.slice = next_slice;
                        d.capabilities = next_caps;
                        d.last_rev = revs.startup;
                        d.rebuild_rows();
                        Self::sync_table_selection(table, cx);
                        cx.notify();
                        table.delegate().toolbar_state()
                    });
                    if this.toolbar_state != next_toolbar || this.last_bios_time_ms != next_bios {
                        this.toolbar_state = next_toolbar;
                        this.last_bios_time_ms = next_bios;
                        cx.notify();
                    }
                }
                UiStoreEvent::SearchQueryChanged => {
                    let next_query = store.read(cx).search_query().to_owned();
                    let next_toolbar = table_for_store.update(cx, |table, cx| {
                        let d = table.delegate_mut();
                        d.search_query = next_query;
                        d.rebuild_rows();
                        Self::sync_table_selection(table, cx);
                        cx.notify();
                        table.delegate().toolbar_state()
                    });
                    if this.toolbar_state != next_toolbar {
                        this.toolbar_state = next_toolbar;
                        cx.notify();
                    }
                }
                UiStoreEvent::SettingsChanged => {
                    let next_locale = store.read(cx).settings().language;
                    if this.last_locale != next_locale {
                        this.last_locale = next_locale;
                        table_for_store.update(cx, |table, cx| {
                            table.delegate_mut().locale = StartupLocaleCache::build();
                            table.refresh(cx);
                            cx.notify();
                        });
                        this.table_pane.update(cx, |pane, cx| {
                            pane.set_accessibility_label(i18n::tr("startup.title"), cx);
                        });
                        cx.notify();
                    }
                }
                UiStoreEvent::ElevationRequired { .. } | UiStoreEvent::CommandFailed { .. } => {}
            }),
        );

        subscriptions.push(cx.subscribe(
            &table,
            |this, table, event: &TableEvent, cx| match event {
                TableEvent::SelectRow(row_ix) => {
                    let next_toolbar = table.update(cx, |table, _| {
                        let id = table.delegate().row_at(*row_ix).map(|e| Arc::clone(&e.id));
                        table.delegate_mut().selected_id = id;
                        table.delegate().toolbar_state()
                    });
                    if this.toolbar_state != next_toolbar {
                        this.toolbar_state = next_toolbar;
                        cx.notify();
                    }
                }
                TableEvent::RightClickedRow(Some(row_ix)) => {
                    let next_toolbar = table.update(cx, |table, cx| {
                        let id = table.delegate().row_at(*row_ix).map(|e| Arc::clone(&e.id));
                        table.delegate_mut().selected_id = id.clone();
                        if id.is_some() && table.selected_row() != Some(*row_ix) {
                            table.set_selected_row(*row_ix, cx);
                        }
                        table.delegate().toolbar_state()
                    });
                    if this.toolbar_state != next_toolbar {
                        this.toolbar_state = next_toolbar;
                        cx.notify();
                    }
                }
                TableEvent::ClearSelection => {
                    let next_toolbar = table.update(cx, |table, _| {
                        table.delegate_mut().selected_id = None;
                        table.delegate().toolbar_state()
                    });
                    if this.toolbar_state != next_toolbar {
                        this.toolbar_state = next_toolbar;
                        cx.notify();
                    }
                }
                _ => {}
            },
        ));

        Self {
            store,
            _lease: lease,
            table,
            table_pane,
            last_bios_time_ms,
            toolbar_state,
            last_locale,
            _subscriptions: subscriptions,
        }
    }

    fn sync_table_selection(
        table: &mut TableState<StartupTableDelegate>,
        cx: &mut Context<TableState<StartupTableDelegate>>,
    ) {
        let selected_idx = table
            .delegate()
            .selected_id
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

    fn selected_entry_id(&self, cx: &App) -> Option<String> {
        self.table
            .read(cx)
            .delegate()
            .selected_entry()
            .map(|e| e.id.to_string())
    }
}

impl Render for StartupView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        debug_assert_eq!(self._lease.domain(), MonitorDomain::Startup);

        let theme = cx.theme();
        let border = theme.border;
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let toolbar_state = self.toolbar_state;
        let bios_label = self
            .last_bios_time_ms
            .map(|ms| i18n::startup_last_bios_time(&format_bios_time_seconds(ms)));

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
                div()
                    .text_lg()
                    .font_semibold()
                    .text_color(fg)
                    .child(i18n::tr("startup.title")),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2p5()
                    .flex_shrink_0()
                    .when_some(bios_label, |row, label| {
                        row.child(div().text_xs().text_color(muted_fg).child(label))
                    })
                    .child(
                        Button::new("startup-enable")
                            .small()
                            .primary()
                            .icon(IconName::CircleCheck)
                            .label(i18n::tr("startup.enable"))
                            .disabled(!toolbar_state.can_enable)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(id) = this.selected_entry_id(cx) {
                                    UiCommands::set_startup_enabled(&this.store, id, true, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("startup-disable")
                            .small()
                            .outline()
                            .icon(IconName::Ban)
                            .label(i18n::tr("startup.disable"))
                            .disabled(!toolbar_state.can_disable)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(id) = this.selected_entry_id(cx) {
                                    UiCommands::set_startup_enabled(&this.store, id, false, cx);
                                }
                            })),
                    ),
            );

        v_flex()
            .id("startup-view-root")
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
    use loom_types::{MonitorDomain, SortDirection, StartupEntry, StartupImpact};

    use super::*;

    fn sample_entry(
        id: &str,
        name: &str,
        publisher: Option<&str>,
        command: Option<&str>,
        enabled: bool,
        impact: StartupImpact,
    ) -> StartupEntry {
        StartupEntry {
            id: Arc::from(id),
            name: Arc::from(name),
            publisher: publisher.map(Arc::from),
            command: command.map(Arc::from),
            file_path: command.map(Arc::from),
            enabled,
            impact,
        }
    }

    #[test]
    fn bios_time_formats_to_one_decimal_second() {
        assert_eq!(format_bios_time_seconds(0), "0.0");
        assert_eq!(format_bios_time_seconds(49), "0.0");
        assert_eq!(format_bios_time_seconds(50), "0.1");
        assert_eq!(format_bios_time_seconds(4_200), "4.2");
        assert_eq!(format_bios_time_seconds(4_250), "4.3");
        assert_eq!(format_bios_time_seconds(12_940), "12.9");
        assert_eq!(format_bios_time_seconds(12_950), "13.0");
    }

    #[test]
    fn startup_filter_and_sort_cover_name_publisher_command_status_and_impact() {
        let items = vec![
            sample_entry(
                "hkcu:onedrive",
                "OneDrive",
                Some("Microsoft Corporation"),
                Some(r"C:\Program Files\Microsoft OneDrive\OneDrive.exe /background"),
                true,
                StartupImpact::High,
            ),
            sample_entry(
                "hkcu:discord",
                "Discord",
                Some("Discord Inc."),
                Some(r"C:\Users\me\AppData\Local\Discord\Update.exe --processStart Discord.exe"),
                false,
                StartupImpact::Low,
            ),
            sample_entry(
                "hklm:security",
                "SecurityHealth",
                Some("Microsoft Corporation"),
                Some(r"C:\Windows\System32\SecurityHealthSystray.exe"),
                true,
                StartupImpact::NotMeasured,
            ),
        ];

        // Sort by Name ascending.
        assert_eq!(
            filter_and_sort_startup(
                &items,
                "",
                StartupSortColumn::Name,
                SortDirection::Ascending
            ),
            vec![1, 0, 2]
        );

        // Sort by Impact descending (High -> Low -> NotMeasured).
        assert_eq!(
            filter_and_sort_startup(
                &items,
                "",
                StartupSortColumn::Impact,
                SortDirection::Descending
            ),
            vec![0, 1, 2]
        );

        // Sort by Status descending (Enabled first, tie-broken by Name).
        assert_eq!(
            filter_and_sort_startup(
                &items,
                "",
                StartupSortColumn::Status,
                SortDirection::Descending
            ),
            vec![0, 2, 1]
        );

        // Filter across publisher and command line.
        assert_eq!(
            filter_and_sort_startup(
                &items,
                "microsoft",
                StartupSortColumn::Name,
                SortDirection::Ascending
            ),
            vec![0, 2]
        );
        assert_eq!(
            filter_and_sort_startup(
                &items,
                "processStart",
                StartupSortColumn::Name,
                SortDirection::Ascending
            ),
            vec![1]
        );
    }

    #[gpui_kit::test]
    fn startup_view_acquires_and_releases_monitor_lease_on_mount_and_drop(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().expect("tempdir");
        let config_store = ConfigStore::new(dir.path().join("settings.toml"));
        let settings = AppSettings::default();
        let backend = NativeBackend::initialize().expect("initialize native backend");
        let coordinator =
            Arc::new(EngineCoordinator::new(backend, settings.clone()).expect("start coordinator"));

        assert!(!coordinator.leases().is_active(MonitorDomain::Startup));

        let store = cx.new(|cx| UiStore::new(coordinator.clone(), config_store, settings, cx));

        let mut view_handle = None;
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                let view = cx.new(|cx| StartupView::new(store.clone(), window, cx));
                view_handle = Some(view.clone());
                cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
            })
            .expect("open startup test window")
        });
        let view = view_handle.expect("StartupView constructed");

        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.update(|window, cx| window.draw(cx).clear(cx));

        assert!(
            coordinator.leases().is_active(MonitorDomain::Startup),
            "mounting StartupView must acquire MonitorDomain::Startup lease"
        );

        drop(view);
        vcx.update(|window, _| window.remove_window());
        vcx.run_until_parked();

        assert!(
            !coordinator.leases().is_active(MonitorDomain::Startup),
            "dropping StartupView must release MonitorDomain::Startup lease"
        );

        coordinator.shutdown();
    }
}
