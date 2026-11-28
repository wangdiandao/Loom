//! On-demand Startup Applications vertical feature slice gated by an RAII [`ScopedMonitorLease`].
//!
//! Splits the static toolbar + Last BIOS time indicator ([`StartupView`]) from the
//! 2D-virtualized `gpui-kit` `DataTable` (`Entity<TableState<StartupTableDelegate>>`)
//! so hidden routes incur zero registry/folder/UWP polling overhead. Filtering and
//! sorting are offloaded completely to `loom-engine`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    Styled as _, Subscription, Task, Window, div, img, prelude::FluentBuilder as _, px,
};
use loom_types::{
    CapabilityFlags, LanguagePreference, MonitorDomain, SortDirection, StartupEntry, StartupImpact,
    StartupSlice, StartupSortColumn,
};
use loom_ui_kit::UNAVAILABLE_LABEL;

use crate::bridge::{ScopedMonitorLease, UiCommands, UiStore, UiStoreEvent};
use crate::features::table_columns::{BudgetedIconLookup, DataTablePane, IconRenderCache};
use crate::i18n;

const STARTUP_HEADER_HEIGHT: f32 = 32.0;
const SCROLL_COALESCE_QUIET_WINDOW: Duration = Duration::from_millis(150);
const SCROLL_COALESCE_MAX_DEFER: Duration = Duration::from_millis(500);
const DEFAULT_FINGERPRINT_WINDOW: usize = 64;

const STARTUP_COLUMNS: [StartupSortColumn; 4] = [
    StartupSortColumn::Name,
    StartupSortColumn::Publisher,
    StartupSortColumn::Status,
    StartupSortColumn::Impact,
];

#[inline]
const fn column_slug(col: StartupSortColumn) -> &'static str {
    match col {
        StartupSortColumn::Name => "name",
        StartupSortColumn::Publisher => "publisher",
        StartupSortColumn::Status => "status",
        StartupSortColumn::Impact => "impact",
    }
}

#[inline]
const fn column_width(col: StartupSortColumn) -> f32 {
    match col {
        StartupSortColumn::Name => 280.0,
        StartupSortColumn::Publisher => 220.0,
        StartupSortColumn::Status => 120.0,
        StartupSortColumn::Impact => 150.0,
    }
}

#[inline]
const fn column_min_width(col: StartupSortColumn) -> f32 {
    match col {
        StartupSortColumn::Name => 140.0,
        StartupSortColumn::Publisher => 110.0,
        StartupSortColumn::Status => 84.0,
        StartupSortColumn::Impact => 96.0,
    }
}

#[inline]
const fn column_default_direction(col: StartupSortColumn) -> SortDirection {
    match col {
        StartupSortColumn::Status | StartupSortColumn::Impact => SortDirection::Descending,
        StartupSortColumn::Name | StartupSortColumn::Publisher => SortDirection::Ascending,
    }
}

#[inline]
const fn to_column_sort(dir: SortDirection) -> ColumnSort {
    match dir {
        SortDirection::Ascending => ColumnSort::Ascending,
        SortDirection::Descending => ColumnSort::Descending,
    }
}

/// Formats firmware POST duration in milliseconds into a tenths-of-second decimal string
/// (e.g., `4250` -> `"4.3"`, `0` -> `"0.0"`).
#[inline]
pub(super) fn format_bios_time_seconds(ms: u64) -> String {
    let tenths = ms.saturating_add(50) / 100;
    format!("{}.{}", tenths / 10, tenths % 10)
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

/// Computes a lightweight content fingerprint over only the currently visible startup rows.
pub(super) fn compute_startup_viewport_fingerprint(
    slice: &StartupSlice,
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
        if let Some(entry) = slice.items.get(item_idx) {
            entry.id.hash(&mut hasher);
            entry.name.hash(&mut hasher);
            entry.publisher.as_deref().hash(&mut hasher);
            entry
                .icon
                .as_ref()
                .map(|ic| ic.rgba.as_ptr() as usize)
                .hash(&mut hasher);
            entry.enabled.hash(&mut hasher);
            entry.impact.hash(&mut hasher);
        }
    }

    hasher.finish()
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct StartupToolbarState {
    can_enable: bool,
    can_disable: bool,
}

/// Cached localized strings for [`StartupTableDelegate`].
struct StartupLocaleCache {
    col_name: SharedString,
    col_publisher: SharedString,
    col_status: SharedString,
    col_impact: SharedString,
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
            col_name: i18n::tr("startup.col_name").into(),
            col_publisher: i18n::tr("startup.col_publisher").into(),
            col_status: i18n::tr("startup.col_status").into(),
            col_impact: i18n::tr("startup.col_impact").into(),
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
    fn column_label(&self, col: StartupSortColumn) -> SharedString {
        match col {
            StartupSortColumn::Name => self.col_name.clone(),
            StartupSortColumn::Publisher => self.col_publisher.clone(),
            StartupSortColumn::Status => self.col_status.clone(),
            StartupSortColumn::Impact => self.col_impact.clone(),
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
    selected_id: Option<Arc<str>>,
    reanchored_selection: Option<Option<usize>>,
    visible_rows: Range<usize>,
    last_scroll_at: Option<Instant>,
    last_viewport_fp: Option<u64>,
    user_action_pending: bool,
    locale: StartupLocaleCache,
    icon_cache: IconRenderCache,
    icon_catchup_scheduled: bool,
}

impl StartupTableDelegate {
    fn new(store: Entity<UiStore>, cx: &App) -> Self {
        let store_ref = store.read(cx);
        let snap = store_ref.snapshot();
        let slice = Arc::clone(&snap.startup);
        let capabilities = snap.capabilities;

        Self {
            store: store.clone(),
            last_rev: snap.revisions.startup,
            slice,
            capabilities,
            selected_id: None,
            reanchored_selection: None,
            visible_rows: 0..0,
            last_scroll_at: None,
            last_viewport_fp: None,
            user_action_pending: false,
            locale: StartupLocaleCache::build(),
            icon_cache: IconRenderCache::new(),
            icon_catchup_scheduled: false,
        }
    }

    #[inline]
    fn row_at(&self, row_ix: usize) -> Option<&StartupEntry> {
        let item_idx = *self.slice.visible_indices.get(row_ix)?;
        self.slice.items.get(item_idx)
    }

    /// Locates `id` in `slice.visible_indices`, checking `hint_row` in O(1) before scanning.
    #[inline]
    fn find_visible_index_by_id(&self, id: &str, hint_row: Option<usize>) -> Option<usize> {
        if let Some(hint) = hint_row
            && self
                .row_at(hint)
                .is_some_and(|entry| entry.id.as_ref() == id)
        {
            return Some(hint);
        }
        self.slice.visible_indices.iter().position(|&idx| {
            self.slice
                .items
                .get(idx)
                .is_some_and(|entry| entry.id.as_ref() == id)
        })
    }

    fn selected_entry(&self, hint_row: Option<usize>) -> Option<&StartupEntry> {
        let id = self.selected_id.as_deref()?;
        let row_ix = self.find_visible_index_by_id(id, hint_row)?;
        self.row_at(row_ix)
    }

    fn toolbar_state(&self, hint_row: Option<usize>) -> StartupToolbarState {
        if !self.capabilities.contains(CapabilityFlags::STARTUP_APPS) {
            return StartupToolbarState::default();
        }
        let Some(entry) = self.selected_entry(hint_row) else {
            return StartupToolbarState::default();
        };
        StartupToolbarState {
            can_enable: !entry.enabled,
            can_disable: entry.enabled,
        }
    }

    fn trigger_sort(&mut self, col_ix: usize, cx: &mut Context<TableState<Self>>) {
        let Some(&target) = STARTUP_COLUMNS.get(col_ix) else {
            return;
        };
        let next_dir = if self.slice.sort_column == target {
            self.slice.sort_direction.toggle()
        } else {
            column_default_direction(target)
        };
        self.user_action_pending = true;
        let store = self.store.clone();
        UiCommands::set_startup_sort(&store, target, next_dir, cx);
    }
}

impl TableDelegate for StartupTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        STARTUP_COLUMNS.len()
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
        let Some(&col) = STARTUP_COLUMNS.get(col_ix) else {
            return Column::default();
        };
        let mut column = Column::new(column_slug(col), self.locale.column_label(col))
            .width(px(column_width(col)))
            .min_width(px(column_min_width(col)))
            .selectable(false)
            .movable(false);

        if col == StartupSortColumn::Name {
            column = column.fixed_left();
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
        self.icon_cache.begin_frame();
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
        let Some(&col) = STARTUP_COLUMNS.get(col_ix) else {
            return Empty.into_any_element();
        };
        let is_active = col == self.slice.sort_column;
        let fg = cx.theme().foreground;
        let muted_fg = cx.theme().muted_foreground;
        let label = self.locale.column_label(col);

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
        div().id(("startup-row", row_ix)).text_xs()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(&item_idx) = self.slice.visible_indices.get(row_ix) else {
            return Empty.into_any_element();
        };
        let Some(entry) = self.slice.items.get(item_idx) else {
            return Empty.into_any_element();
        };
        let Some(&col) = STARTUP_COLUMNS.get(col_ix) else {
            return Empty.into_any_element();
        };

        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let primary = theme.primary;
        let warning = theme.warning;
        let danger = theme.danger;

        match col {
            StartupSortColumn::Name => {
                let name = SharedString::from(Arc::clone(&entry.name));
                let rendered_icon = entry.icon.as_ref().and_then(|icon| {
                    match self.icon_cache.get_or_insert_budgeted(icon) {
                        BudgetedIconLookup::Hit(img) | BudgetedIconLookup::Uploaded(img) => {
                            Some(img)
                        }
                        BudgetedIconLookup::Deferred => {
                            if !self.icon_catchup_scheduled {
                                self.icon_catchup_scheduled = true;
                                let table_weak = cx.entity().downgrade();
                                window.on_next_frame(move |_window, cx| {
                                    let _ = table_weak.update(cx, |table, cx| {
                                        let d = table.delegate_mut();
                                        d.icon_catchup_scheduled = false;
                                        d.icon_cache.begin_frame();
                                        cx.notify();
                                    });
                                });
                            }
                            None
                        }
                        BudgetedIconLookup::Invalid => None,
                    }
                });
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
                    .gap_2()
                    .overflow_hidden()
                    .child(icon_el)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .font_medium()
                            .text_color(fg)
                            .child(name),
                    )
                    .into_any_element()
            }
            StartupSortColumn::Publisher => {
                let text = entry
                    .publisher
                    .as_ref()
                    .filter(|s| !s.is_empty())
                    .map(|s| SharedString::from(Arc::clone(s)))
                    .unwrap_or_else(|| SharedString::new_static(UNAVAILABLE_LABEL));
                h_flex()
                    .size_full()
                    .items_center()
                    .truncate()
                    .text_xs()
                    .text_color(muted_fg)
                    .child(text)
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
                    .truncate()
                    .text_xs()
                    .text_color(color)
                    .child(label)
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
                    .truncate()
                    .text_xs()
                    .text_color(color)
                    .child(self.locale.impact_label(entry.impact))
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
    scroll_coalescer: ScrollCoalescer,
    coalesce_task: Option<Task<()>>,
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
        let toolbar_state = {
            let t = table.read(cx);
            t.delegate().toolbar_state(t.selected_row())
        };

        let mut subscriptions = Vec::with_capacity(2);

        let table_for_store = table.clone();
        subscriptions.push(
            cx.subscribe(&store, move |this, store, event, cx| match event {
                UiStoreEvent::SnapshotUpdated(revs) => {
                    let current_rev = table_for_store.read(cx).delegate().last_rev;
                    if revs.startup == current_rev {
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
                            d.locale = StartupLocaleCache::build();
                            d.last_viewport_fp = None;
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
                        let d = table.delegate_mut();
                        if d.reanchored_selection == Some(Some(*row_ix)) {
                            d.reanchored_selection = None;
                            return d.toolbar_state(Some(*row_ix));
                        }
                        d.reanchored_selection = None;
                        let id = d.row_at(*row_ix).map(|e| Arc::clone(&e.id));
                        d.selected_id = id;
                        d.last_viewport_fp = None;
                        d.toolbar_state(Some(*row_ix))
                    });
                    if this.toolbar_state != next_toolbar {
                        this.toolbar_state = next_toolbar;
                        cx.notify();
                    }
                }
                TableEvent::RightClickedRow(Some(row_ix)) => {
                    let next_toolbar = table.update(cx, |table, cx| {
                        let id = table.delegate().row_at(*row_ix).map(|e| Arc::clone(&e.id));
                        {
                            let d = table.delegate_mut();
                            d.selected_id = id.clone();
                            d.reanchored_selection = None;
                            d.last_viewport_fp = None;
                        }
                        if id.is_some() && table.selected_row() != Some(*row_ix) {
                            table.set_selected_row(*row_ix, cx);
                        }
                        table.delegate().toolbar_state(table.selected_row())
                    });
                    if this.toolbar_state != next_toolbar {
                        this.toolbar_state = next_toolbar;
                        cx.notify();
                    }
                }
                TableEvent::ClearSelection => {
                    let (was_reanchor, next_toolbar) = table.update(cx, |table, _| {
                        let d = table.delegate_mut();
                        if d.reanchored_selection == Some(None) {
                            d.reanchored_selection = None;
                            return (true, d.toolbar_state(None));
                        }
                        d.reanchored_selection = None;
                        d.selected_id = None;
                        d.last_viewport_fp = None;
                        (false, d.toolbar_state(None))
                    });
                    if !was_reanchor && this.toolbar_state != next_toolbar {
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
            scroll_coalescer: ScrollCoalescer::default(),
            coalesce_task: None,
            _subscriptions: subscriptions,
        }
    }

    fn handle_snapshot_updated(&mut self, cx: &mut Context<Self>) {
        let (next_rev, next_slice, next_caps) = {
            let snap = self.store.read(cx).snapshot();
            (
                snap.revisions.startup,
                Arc::clone(&snap.startup),
                snap.capabilities,
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
                let next_bios = next_slice.last_bios_time_ms;
                let next_toolbar = self.table.update(cx, |table, cx| {
                    let prev_fp = table.delegate().last_viewport_fp;
                    let sort_changed = {
                        let d = table.delegate_mut();
                        let changed = d.slice.sort_column != next_slice.sort_column
                            || d.slice.sort_direction != next_slice.sort_direction;
                        d.slice = next_slice;
                        d.capabilities = next_caps;
                        d.last_rev = next_rev;
                        d.user_action_pending = false;
                        changed
                    };

                    Self::sync_table_selection(table, cx);

                    let selected_row = table.selected_row();
                    let next_fp = {
                        let d = table.delegate();
                        compute_startup_viewport_fingerprint(
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
                    table.delegate().toolbar_state(selected_row)
                });
                if self.toolbar_state != next_toolbar || self.last_bios_time_ms != next_bios {
                    self.toolbar_state = next_toolbar;
                    self.last_bios_time_ms = next_bios;
                    cx.notify();
                }
            }
        }
    }

    fn sync_table_selection(
        table: &mut TableState<StartupTableDelegate>,
        cx: &mut Context<TableState<StartupTableDelegate>>,
    ) {
        let hint_row = table.selected_row();
        let selected_idx = table
            .delegate()
            .selected_id
            .as_deref()
            .and_then(|id| table.delegate().find_visible_index_by_id(id, hint_row));
        if let Some(idx) = selected_idx {
            if hint_row != Some(idx) {
                table.delegate_mut().reanchored_selection = Some(Some(idx));
                table.set_selected_row(idx, cx);
            }
        } else if hint_row.is_some() {
            table.delegate_mut().reanchored_selection = Some(None);
            table.clear_selection(cx);
        }
    }

    fn selected_entry_id(&self, cx: &App) -> Option<String> {
        let table = self.table.read(cx);
        table
            .delegate()
            .selected_entry(table.selected_row())
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
    use std::time::{Duration, Instant};

    use gpui_kit::{TestAppContext, VisualTestContext};
    use loom_config::{AppSettings, ConfigStore};
    use loom_engine::EngineCoordinator;
    use loom_pal::{NativeBackend, PlatformBackend};
    use loom_types::{
        MonitorDomain, SortDirection, StartupEntry, StartupImpact, StartupSlice, StartupSortColumn,
    };

    use super::*;

    fn sample_entry(
        id: &str,
        name: &str,
        publisher: Option<&str>,
        enabled: bool,
        impact: StartupImpact,
    ) -> StartupEntry {
        StartupEntry {
            id: Arc::from(id),
            name: Arc::from(name),
            publisher: publisher.map(Arc::from),
            command: None,
            file_path: None,
            icon: None,
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
    fn startup_viewport_fingerprint_detects_visible_changes_and_ignores_offscreen() {
        let mut slice = StartupSlice {
            revision: 1,
            sort_column: StartupSortColumn::Name,
            sort_direction: SortDirection::Ascending,
            items: vec![
                sample_entry(
                    "id:1",
                    "Discord",
                    Some("Discord Inc."),
                    false,
                    StartupImpact::Low,
                ),
                sample_entry(
                    "id:2",
                    "OneDrive",
                    Some("Microsoft"),
                    true,
                    StartupImpact::High,
                ),
                sample_entry(
                    "id:3",
                    "Security",
                    Some("Microsoft"),
                    true,
                    StartupImpact::NotMeasured,
                ),
            ],
            visible_indices: vec![0, 1, 2],
            ..StartupSlice::default()
        };

        let fp_initial = compute_startup_viewport_fingerprint(&slice, &(0..2), Some(0));

        // Off-screen row toggle (index 2) keeps visible fingerprint identical.
        slice.items[2].enabled = false;
        assert_eq!(
            fp_initial,
            compute_startup_viewport_fingerprint(&slice, &(0..2), Some(0))
        );

        // Visible row toggle (index 0) updates the fingerprint.
        slice.items[0].enabled = true;
        assert_ne!(
            fp_initial,
            compute_startup_viewport_fingerprint(&slice, &(0..2), Some(0))
        );
    }

    #[test]
    fn startup_scroll_coalescer_enforces_quiet_window_cap_and_bypass() {
        let mut coalescer = ScrollCoalescer::default();
        let t0 = Instant::now();
        assert_eq!(
            coalescer.decide(t0 + Duration::from_millis(50), Some(t0), false),
            CoalesceDecision::Defer(Duration::from_millis(100))
        );
        assert_eq!(
            coalescer.decide(
                t0 + Duration::from_millis(550),
                Some(t0 + Duration::from_millis(540)),
                false
            ),
            CoalesceDecision::ApplyNow
        );
        assert_eq!(
            coalescer.decide(t0 + Duration::from_millis(10), Some(t0), true),
            CoalesceDecision::ApplyNow
        );
    }

    #[gpui_kit::test]
    fn startup_view_acquires_lease_dispatches_sort_and_releases_on_drop(cx: &mut TestAppContext) {
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

        view.update(&mut vcx, |view, cx| {
            view.table.update(cx, |table, cx| {
                table.delegate_mut().trigger_sort(3, cx);
            });
        });
        vcx.run_until_parked();

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
