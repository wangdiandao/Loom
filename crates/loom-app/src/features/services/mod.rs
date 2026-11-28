//! On-demand Services vertical feature slice gated by an RAII [`ScopedMonitorLease`].
//!
//! Splits the static toolbar + PID filter banner ([`ServicesView`]) from the
//! 2D-virtualized `gpui-kit` `DataTable` (`Entity<TableState<ServicesTableDelegate>>`)
//! so live snapshot ticks refresh only the table entity while hidden routes incur
//! zero SCM polling overhead. Filtering and sorting are offloaded completely to
//! `loom-engine`, keeping the GPUI UI thread focused on O(1) rendering.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};
use std::ops::Range;
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
    Styled as _, Subscription, Task, Window, div, prelude::FluentBuilder as _, px,
};
use loom_engine::{EngineCommand, EngineCoordinator};
use loom_pal::NativeBackend;
use loom_types::{
    AppSnapshot, CapabilityFlags, LanguagePreference, MonitorDomain, ProcessKey, ProcessRowKind,
    ServiceRecord, ServiceState, ServicesSlice, ServicesSortColumn, SortDirection,
};
use loom_ui_kit::{SMALL_U32_TABLE_MAX, UNAVAILABLE_LABEL, format_u32_shared};

use crate::bridge::{ScopedMonitorLease, UiCommands, UiStore, UiStoreEvent};
use crate::features::table_columns::DataTablePane;
use crate::i18n;
use crate::shell::ShellHandle;

const SERVICES_HEADER_HEIGHT: f32 = 32.0;
const SCROLL_COALESCE_QUIET_WINDOW: Duration = Duration::from_millis(150);
const SCROLL_COALESCE_MAX_DEFER: Duration = Duration::from_millis(500);
const PID_TEXT_CACHE_LIMIT: usize = 1024;
const DEFAULT_FINGERPRINT_WINDOW: usize = 64;

const SERVICES_COLUMNS: [ServicesSortColumn; 5] = [
    ServicesSortColumn::Name,
    ServicesSortColumn::Pid,
    ServicesSortColumn::Description,
    ServicesSortColumn::Status,
    ServicesSortColumn::Group,
];

#[inline]
const fn column_slug(col: ServicesSortColumn) -> &'static str {
    match col {
        ServicesSortColumn::Name => "name",
        ServicesSortColumn::Pid => "pid",
        ServicesSortColumn::Description => "description",
        ServicesSortColumn::Status => "status",
        ServicesSortColumn::Group => "group",
    }
}

#[inline]
const fn column_width(col: ServicesSortColumn) -> f32 {
    match col {
        ServicesSortColumn::Name => 200.0,
        ServicesSortColumn::Pid => 84.0,
        ServicesSortColumn::Description => 340.0,
        ServicesSortColumn::Status => 120.0,
        ServicesSortColumn::Group => 180.0,
    }
}

#[inline]
const fn column_min_width(col: ServicesSortColumn) -> f32 {
    match col {
        ServicesSortColumn::Name => 120.0,
        ServicesSortColumn::Pid => 64.0,
        ServicesSortColumn::Description => 140.0,
        ServicesSortColumn::Status => 84.0,
        ServicesSortColumn::Group => 96.0,
    }
}

#[inline]
const fn column_right_aligned(col: ServicesSortColumn) -> bool {
    matches!(col, ServicesSortColumn::Pid)
}

#[inline]
const fn column_default_direction(col: ServicesSortColumn) -> SortDirection {
    match col {
        ServicesSortColumn::Status => SortDirection::Descending,
        ServicesSortColumn::Name
        | ServicesSortColumn::Pid
        | ServicesSortColumn::Description
        | ServicesSortColumn::Group => SortDirection::Ascending,
    }
}

#[inline]
const fn to_column_sort(dir: SortDirection) -> ColumnSort {
    match dir {
        SortDirection::Ascending => ColumnSort::Ascending,
        SortDirection::Descending => ColumnSort::Descending,
    }
}

/// Returns `true` when a service in `state` can be started.
#[inline]
pub(super) const fn can_start_service(state: ServiceState) -> bool {
    matches!(
        state,
        ServiceState::Stopped | ServiceState::Paused | ServiceState::Unknown
    )
}

/// Returns `true` when a service in `state` can be stopped or restarted.
#[inline]
pub(super) const fn can_stop_or_restart_service(state: ServiceState) -> bool {
    matches!(state, ServiceState::Running | ServiceState::Paused)
}

/// Returns the best human-readable description text for `record`.
#[inline]
pub(super) fn service_description_text(record: &ServiceRecord) -> &str {
    if let Some(desc) = record
        .description
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return desc;
    }
    let display = record.display_name.trim();
    if !display.is_empty() {
        return display;
    }
    UNAVAILABLE_LABEL
}

/// Returns the best human-readable description as a [`SharedString`], reusing the
/// backing `Arc<str>` with zero heap allocation when the text is already trimmed.
#[inline]
pub(super) fn service_description_shared(record: &ServiceRecord) -> SharedString {
    if let Some(ref desc) = record.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            return if trimmed.len() == desc.len() {
                SharedString::from(Arc::clone(desc))
            } else {
                SharedString::from(trimmed.to_owned())
            };
        }
    }
    let display = record.display_name.trim();
    if !display.is_empty() {
        return if display.len() == record.display_name.len() {
            SharedString::from(Arc::clone(&record.display_name))
        } else {
            SharedString::from(display.to_owned())
        };
    }
    SharedString::new_static(UNAVAILABLE_LABEL)
}

/// Resolves a live [`ProcessKey`] for a service host `pid` from the latest [`AppSnapshot`].
pub(super) fn resolve_process_key_by_pid(snapshot: &AppSnapshot, pid: u32) -> Option<ProcessKey> {
    if pid == 0 {
        return None;
    }
    snapshot
        .processes
        .rows
        .iter()
        .find(|row| row.kind == ProcessRowKind::Process && row.key.pid == pid)
        .map(|row| row.key)
        .or_else(|| {
            snapshot
                .details
                .rows
                .iter()
                .find(|row| row.key.pid == pid)
                .map(|row| row.key)
        })
}

/// Outcome of the scroll-burst coalescing gate for periodic snapshot updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoalesceDecision {
    ApplyNow,
    Defer(Duration),
}

/// Coalesces periodic snapshot ticks during active scrolling to keep GPU row layers
/// on their fast composite path, while enforcing a bounded maximum defer cap.
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

    fn reset(&mut self) {
        self.deferred_since = None;
    }
}

/// Computes a lightweight content fingerprint over only the currently visible rows.
pub(super) fn compute_services_viewport_fingerprint(
    slice: &ServicesSlice,
    visible_rows: &Range<usize>,
    selected_row: Option<usize>,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    let total = slice.visible_indices.len();
    (slice.revision == 0).hash(&mut hasher);
    total.hash(&mut hasher);
    slice.sort_column.hash(&mut hasher);
    slice.sort_direction.hash(&mut hasher);
    slice.filter_pid.hash(&mut hasher);
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
            record.name.hash(&mut hasher);
            record.pid.hash(&mut hasher);
            record.state.hash(&mut hasher);
            service_description_text(record).hash(&mut hasher);
            record.group.as_deref().hash(&mut hasher);
        }
    }

    hasher.finish()
}

/// Action eligibility derived from the currently selected service row.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ServicesToolbarState {
    can_start: bool,
    can_stop: bool,
    can_restart: bool,
}

/// Cached localized strings for [`ServicesTableDelegate`].
struct ServicesLocaleCache {
    col_name: SharedString,
    col_pid: SharedString,
    col_description: SharedString,
    col_status: SharedString,
    col_group: SharedString,
    running: SharedString,
    stopped: SharedString,
    start_pending: SharedString,
    stop_pending: SharedString,
    paused: SharedString,
    pause_pending: SharedString,
    continue_pending: SharedString,
    unknown: SharedString,
    loading: SharedString,
    empty: SharedString,
}

impl ServicesLocaleCache {
    fn build() -> Self {
        Self {
            col_name: i18n::tr("services.col_name").into(),
            col_pid: i18n::tr("services.col_pid").into(),
            col_description: i18n::tr("services.col_description").into(),
            col_status: i18n::tr("services.col_status").into(),
            col_group: i18n::tr("services.col_group").into(),
            running: i18n::service_state_label(ServiceState::Running).into(),
            stopped: i18n::service_state_label(ServiceState::Stopped).into(),
            start_pending: i18n::service_state_label(ServiceState::StartPending).into(),
            stop_pending: i18n::service_state_label(ServiceState::StopPending).into(),
            paused: i18n::service_state_label(ServiceState::Paused).into(),
            pause_pending: i18n::service_state_label(ServiceState::PausePending).into(),
            continue_pending: i18n::service_state_label(ServiceState::ContinuePending).into(),
            unknown: i18n::service_state_label(ServiceState::Unknown).into(),
            loading: i18n::tr("services.loading").into(),
            empty: i18n::tr("services.empty").into(),
        }
    }

    #[inline]
    fn column_label(&self, col: ServicesSortColumn) -> SharedString {
        match col {
            ServicesSortColumn::Name => self.col_name.clone(),
            ServicesSortColumn::Pid => self.col_pid.clone(),
            ServicesSortColumn::Description => self.col_description.clone(),
            ServicesSortColumn::Status => self.col_status.clone(),
            ServicesSortColumn::Group => self.col_group.clone(),
        }
    }

    #[inline]
    fn state_label(&self, state: ServiceState) -> SharedString {
        match state {
            ServiceState::Running => self.running.clone(),
            ServiceState::Stopped => self.stopped.clone(),
            ServiceState::StartPending => self.start_pending.clone(),
            ServiceState::StopPending => self.stop_pending.clone(),
            ServiceState::Paused => self.paused.clone(),
            ServiceState::PausePending => self.pause_pending.clone(),
            ServiceState::ContinuePending => self.continue_pending.clone(),
            ServiceState::Unknown => self.unknown.clone(),
        }
    }
}

/// 2D-virtualized `gpui-kit` [`TableDelegate`] backing [`ServicesView`].
struct ServicesTableDelegate {
    store: Entity<UiStore>,
    shell_handle: ShellHandle,
    slice: Arc<ServicesSlice>,
    capabilities: CapabilityFlags,
    last_rev: u64,
    selected_name: Option<Arc<str>>,
    reanchored_selection: Option<Option<usize>>,
    visible_rows: Range<usize>,
    last_scroll_at: Option<Instant>,
    last_viewport_fp: Option<u64>,
    user_action_pending: bool,
    pid_text_cache: HashMap<u32, SharedString>,
    locale: ServicesLocaleCache,
}

impl ServicesTableDelegate {
    fn new(store: Entity<UiStore>, shell_handle: ShellHandle, cx: &App) -> Self {
        let store_ref = store.read(cx);
        let snap = store_ref.snapshot();
        let slice = Arc::clone(&snap.services);
        let capabilities = snap.capabilities;

        Self {
            store: store.clone(),
            shell_handle,
            last_rev: snap.revisions.services,
            slice,
            capabilities,
            selected_name: None,
            reanchored_selection: None,
            visible_rows: 0..0,
            last_scroll_at: None,
            last_viewport_fp: None,
            user_action_pending: false,
            pid_text_cache: HashMap::new(),
            locale: ServicesLocaleCache::build(),
        }
    }

    #[inline]
    fn row_at(&self, row_ix: usize) -> Option<&ServiceRecord> {
        let item_idx = *self.slice.visible_indices.get(row_ix)?;
        self.slice.items.get(item_idx)
    }

    /// Locates `name` in `slice.visible_indices`, checking `hint_row` in O(1) before scanning.
    #[inline]
    fn find_visible_index_by_name(&self, name: &str, hint_row: Option<usize>) -> Option<usize> {
        if let Some(hint) = hint_row
            && self
                .row_at(hint)
                .is_some_and(|record| record.name.as_ref() == name)
        {
            return Some(hint);
        }
        self.slice.visible_indices.iter().position(|&idx| {
            self.slice
                .items
                .get(idx)
                .is_some_and(|record| record.name.as_ref() == name)
        })
    }

    fn selected_record(&self, hint_row: Option<usize>) -> Option<&ServiceRecord> {
        let sel = self.selected_name.as_deref()?;
        let row_ix = self.find_visible_index_by_name(sel, hint_row)?;
        self.row_at(row_ix)
    }

    fn toolbar_state(&self, hint_row: Option<usize>) -> ServicesToolbarState {
        if !self.capabilities.contains(CapabilityFlags::SERVICES) {
            return ServicesToolbarState::default();
        }
        let Some(record) = self.selected_record(hint_row) else {
            return ServicesToolbarState::default();
        };
        ServicesToolbarState {
            can_start: can_start_service(record.state),
            can_stop: can_stop_or_restart_service(record.state),
            can_restart: can_stop_or_restart_service(record.state),
        }
    }

    #[inline]
    fn pid_shared(&mut self, pid: u32) -> SharedString {
        if pid <= SMALL_U32_TABLE_MAX {
            return format_u32_shared(pid);
        }
        if self.pid_text_cache.len() >= PID_TEXT_CACHE_LIMIT
            && !self.pid_text_cache.contains_key(&pid)
        {
            self.pid_text_cache.clear();
        }
        self.pid_text_cache
            .entry(pid)
            .or_insert_with(|| format_u32_shared(pid))
            .clone()
    }

    fn trigger_sort(&mut self, col_ix: usize, cx: &mut Context<TableState<Self>>) {
        let Some(&target) = SERVICES_COLUMNS.get(col_ix) else {
            return;
        };
        let next_dir = if self.slice.sort_column == target {
            self.slice.sort_direction.toggle()
        } else {
            column_default_direction(target)
        };
        self.user_action_pending = true;
        let store = self.store.clone();
        UiCommands::set_services_sort(&store, target, next_dir, cx);
    }
}

impl TableDelegate for ServicesTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        SERVICES_COLUMNS.len()
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
        let Some(&col) = SERVICES_COLUMNS.get(col_ix) else {
            return Column::default();
        };
        let mut column = Column::new(column_slug(col), self.locale.column_label(col))
            .width(px(column_width(col)))
            .min_width(px(column_min_width(col)))
            .selectable(false)
            .movable(false);

        if col == ServicesSortColumn::Name {
            column = column.fixed_left();
        }
        if column_right_aligned(col) {
            column = column.text_right();
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
            .id("services-column-header")
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
        let Some(&col) = SERVICES_COLUMNS.get(col_ix) else {
            return Empty.into_any_element();
        };
        let is_active = col == self.slice.sort_column;
        let fg = cx.theme().foreground;
        let muted_fg = cx.theme().muted_foreground;
        let label = self.locale.column_label(col);

        h_flex()
            .id(("services-th", col_ix))
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
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
        div().id(("services-row", row_ix)).text_xs()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(&col) = SERVICES_COLUMNS.get(col_ix) else {
            return Empty.into_any_element();
        };
        let Some(&item_idx) = self.slice.visible_indices.get(row_ix) else {
            return Empty.into_any_element();
        };
        if item_idx >= self.slice.items.len() {
            return Empty.into_any_element();
        }

        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let primary = theme.primary;
        let mono_font = theme.mono_font_family.clone();

        match col {
            ServicesSortColumn::Name => {
                let text = SharedString::from(Arc::clone(&self.slice.items[item_idx].name));
                h_flex()
                    .size_full()
                    .items_center()
                    .truncate()
                    .text_xs()
                    .font_medium()
                    .text_color(fg)
                    .child(text)
                    .into_any_element()
            }
            ServicesSortColumn::Pid => {
                let pid = self.slice.items[item_idx].pid;
                let text = match pid {
                    Some(pid) => self.pid_shared(pid),
                    None => SharedString::new_static(UNAVAILABLE_LABEL),
                };
                h_flex()
                    .size_full()
                    .items_center()
                    .justify_end()
                    .truncate()
                    .text_xs()
                    .font_family(mono_font)
                    .text_color(muted_fg)
                    .child(text)
                    .into_any_element()
            }
            ServicesSortColumn::Description => {
                let text = service_description_shared(&self.slice.items[item_idx]);
                h_flex()
                    .size_full()
                    .items_center()
                    .truncate()
                    .text_xs()
                    .text_color(muted_fg)
                    .child(text)
                    .into_any_element()
            }
            ServicesSortColumn::Status => {
                let state = self.slice.items[item_idx].state;
                let color = if state == ServiceState::Running {
                    primary
                } else {
                    muted_fg
                };
                h_flex()
                    .size_full()
                    .items_center()
                    .truncate()
                    .text_xs()
                    .text_color(color)
                    .child(self.locale.state_label(state))
                    .into_any_element()
            }
            ServicesSortColumn::Group => {
                let text = self.slice.items[item_idx]
                    .group
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
        }
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(record) = self.row_at(row_ix).cloned() else {
            return menu;
        };

        let supported = self.capabilities.contains(CapabilityFlags::SERVICES);
        let can_start = supported && can_start_service(record.state);
        let can_stop = supported && can_stop_or_restart_service(record.state);
        let can_restart = supported && can_stop_or_restart_service(record.state);
        let resolved_key = record
            .pid
            .and_then(|pid| resolve_process_key_by_pid(self.store.read(cx).snapshot(), pid));

        let store_start = self.store.clone();
        let name_start = record.name.to_string();

        let store_stop = self.store.clone();
        let name_stop = record.name.to_string();

        let store_restart = self.store.clone();
        let name_restart = record.name.to_string();

        let store_console = self.store.clone();
        let shell_details = self.shell_handle.clone();

        let store_search = self.store.clone();
        let search_query = format!("{} {}", record.name, record.display_name);

        menu.item(
            PopupMenuItem::new(i18n::tr("services.menu_start"))
                .icon(IconName::Play)
                .disabled(!can_start)
                .on_click(move |_, _, cx| {
                    UiCommands::start_service(&store_start, name_start.clone(), cx);
                }),
        )
        .item(
            PopupMenuItem::new(i18n::tr("services.menu_stop"))
                .icon(IconName::Square)
                .disabled(!can_stop)
                .on_click(move |_, _, cx| {
                    UiCommands::stop_service(&store_stop, name_stop.clone(), cx);
                }),
        )
        .item(
            PopupMenuItem::new(i18n::tr("services.menu_restart"))
                .icon(IconName::RotateCw)
                .disabled(!can_restart)
                .on_click(move |_, _, cx| {
                    UiCommands::restart_service(&store_restart, name_restart.clone(), cx);
                }),
        )
        .separator()
        .item(
            PopupMenuItem::new(i18n::tr("services.menu_open_services"))
                .icon(IconName::ExternalLink)
                .disabled(!supported)
                .on_click(move |_, _, cx| {
                    UiCommands::open_services_console(&store_console, cx);
                }),
        )
        .item(
            PopupMenuItem::new(i18n::tr("services.menu_go_to_details"))
                .icon(IconName::ArrowRight)
                .disabled(resolved_key.is_none())
                .on_click(move |_, window, cx| {
                    if let Some(key) = resolved_key {
                        shell_details.jump_to_details(key, window, cx);
                    }
                }),
        )
        .separator()
        .item(
            PopupMenuItem::new(i18n::tr("services.menu_search_online"))
                .icon(IconName::Globe)
                .on_click(move |_, _, cx| {
                    UiCommands::search_online(&store_search, search_query.clone(), cx);
                }),
        )
    }

    fn row_label(&self, row_ix: usize, _cx: &App) -> Option<SharedString> {
        let record = self.row_at(row_ix)?;
        let status = self.locale.state_label(record.state);
        if let Some(pid) = record.pid {
            Some(format!("{} (PID {}, {})", record.name, pid, status).into())
        } else {
            Some(format!("{} ({})", record.name, status).into())
        }
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

/// On-demand system services manager holding an RAII `MonitorDomain::Services` lease.
pub(crate) struct ServicesView {
    store: Entity<UiStore>,
    coordinator: Arc<EngineCoordinator<NativeBackend>>,
    _lease: ScopedMonitorLease,
    table: Entity<TableState<ServicesTableDelegate>>,
    table_pane: Entity<DataTablePane<ServicesTableDelegate>>,
    filter_pid: Option<u32>,
    toolbar_state: ServicesToolbarState,
    services_supported: bool,
    last_locale: LanguagePreference,
    scroll_coalescer: ScrollCoalescer,
    coalesce_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Drop for ServicesView {
    fn drop(&mut self) {
        if self.filter_pid.is_some() {
            let _ = self
                .coordinator
                .send_command(EngineCommand::SetServicesFilterPid(None));
        }
    }
}

impl Focusable for ServicesView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.table.focus_handle(cx)
    }
}

impl ServicesView {
    /// Creates a new [`ServicesView`], immediately acquiring `MonitorDomain::Services`
    /// and syncing the backend PID filter.
    pub(crate) fn new(
        store: Entity<UiStore>,
        filter_pid: Option<u32>,
        shell_handle: ShellHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (coordinator, lease, services_supported, last_locale) = {
            let s = store.read(cx);
            (
                Arc::clone(s.coordinator()),
                s.acquire_lease(MonitorDomain::Services),
                s.snapshot()
                    .capabilities
                    .contains(CapabilityFlags::SERVICES),
                s.settings().language,
            )
        };

        // Always synchronize the backend PID filter on mount (including `None`) so
        // stale filters from previous navigations never leak into a fresh view.
        UiCommands::set_services_filter_pid(&store, filter_pid, cx);

        let table = cx.new(|cx| {
            let mut delegate = ServicesTableDelegate::new(store.clone(), shell_handle, cx);
            delegate.user_action_pending = filter_pid != delegate.slice.filter_pid;
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
                px(SERVICES_HEADER_HEIGHT),
                false,
                i18n::tr("services.title"),
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
                    if revs.services == current_rev {
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
                            d.locale = ServicesLocaleCache::build();
                            d.last_viewport_fp = None;
                            table.refresh(cx);
                            cx.notify();
                        });
                        this.table_pane.update(cx, |pane, cx| {
                            pane.set_accessibility_label(i18n::tr("services.title"), cx);
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
                        let name = d.row_at(*row_ix).map(|r| Arc::clone(&r.name));
                        d.selected_name = name;
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
                        let name = table
                            .delegate()
                            .row_at(*row_ix)
                            .map(|r| Arc::clone(&r.name));
                        {
                            let d = table.delegate_mut();
                            d.selected_name = name.clone();
                            d.reanchored_selection = None;
                            d.last_viewport_fp = None;
                        }
                        if name.is_some() && table.selected_row() != Some(*row_ix) {
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
                        d.selected_name = None;
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
            coordinator,
            _lease: lease,
            table,
            table_pane,
            filter_pid,
            toolbar_state,
            services_supported,
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
                snap.revisions.services,
                Arc::clone(&snap.services),
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
                || d.slice.sort_direction != next_slice.sort_direction
                || d.slice.filter_pid != next_slice.filter_pid;
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
                        compute_services_viewport_fingerprint(
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
                let supported = next_caps.contains(CapabilityFlags::SERVICES);
                if self.toolbar_state != next_toolbar || self.services_supported != supported {
                    self.toolbar_state = next_toolbar;
                    self.services_supported = supported;
                    cx.notify();
                }
            }
        }
    }

    fn sync_table_selection(
        table: &mut TableState<ServicesTableDelegate>,
        cx: &mut Context<TableState<ServicesTableDelegate>>,
    ) {
        let hint_row = table.selected_row();
        let selected_idx = table
            .delegate()
            .selected_name
            .as_deref()
            .and_then(|name| table.delegate().find_visible_index_by_name(name, hint_row));
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

    /// Updates or clears the active host PID filter (e.g., from `NavigationIntent::JumpToServices`).
    pub(crate) fn set_filter_pid(&mut self, pid: Option<u32>, cx: &mut Context<Self>) {
        if self.filter_pid == pid {
            return;
        }
        self.filter_pid = pid;
        self.scroll_coalescer.reset();
        self.coalesce_task = None;
        self.table.update(cx, |table, _| {
            table.delegate_mut().user_action_pending = true;
        });
        UiCommands::set_services_filter_pid(&self.store, pid, cx);
        cx.notify();
    }

    fn selected_service_name(&self, cx: &App) -> Option<String> {
        let table = self.table.read(cx);
        table
            .delegate()
            .selected_record(table.selected_row())
            .map(|r| r.name.to_string())
    }
}

impl Render for ServicesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        debug_assert_eq!(self._lease.domain(), MonitorDomain::Services);

        let theme = cx.theme();
        let border = theme.border;
        let fg = theme.foreground;
        let primary = theme.primary;
        let toolbar_state = self.toolbar_state;
        let supported = self.services_supported;
        let filter_pid = self.filter_pid;
        let store_console = self.store.clone();

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
                    .child(i18n::tr("services.title")),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        Button::new("services-start")
                            .small()
                            .primary()
                            .icon(IconName::Play)
                            .label(i18n::tr("services.start"))
                            .disabled(!toolbar_state.can_start)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(name) = this.selected_service_name(cx) {
                                    UiCommands::start_service(&this.store, name, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("services-stop")
                            .small()
                            .outline()
                            .icon(IconName::Square)
                            .label(i18n::tr("services.stop"))
                            .disabled(!toolbar_state.can_stop)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(name) = this.selected_service_name(cx) {
                                    UiCommands::stop_service(&this.store, name, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("services-restart")
                            .small()
                            .outline()
                            .icon(IconName::RotateCw)
                            .label(i18n::tr("services.restart"))
                            .disabled(!toolbar_state.can_restart)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(name) = this.selected_service_name(cx) {
                                    UiCommands::restart_service(&this.store, name, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("services-open-console")
                            .small()
                            .ghost()
                            .icon(IconName::ExternalLink)
                            .label(i18n::tr("services.open_services"))
                            .disabled(!supported)
                            .on_click(move |_, _, cx| {
                                UiCommands::open_services_console(&store_console, cx);
                            }),
                    ),
            );

        let pid_banner = filter_pid.map(|pid| {
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .px_4()
                .py_1p5()
                .bg(primary.opacity(0.10))
                .border_b_1()
                .border_color(primary.opacity(0.25))
                .child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .font_medium()
                        .text_color(primary)
                        .child(Icon::new(IconName::Search).xsmall().text_color(primary))
                        .child(format!("{}: {}", i18n::tr("services.filtered_pid"), pid)),
                )
                .child(
                    Button::new("services-clear-pid-filter")
                        .xsmall()
                        .ghost()
                        .label(i18n::tr("services.clear_filter"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.set_filter_pid(None, cx);
                        })),
                )
        });

        v_flex()
            .id("services-view-root")
            .size_full()
            .min_h_0()
            .child(toolbar)
            .when_some(pid_banner, |this, banner| this.child(banner))
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
        AppSnapshot, MonitorDomain, ProcessCategory, ProcessKey, ProcessRowKind,
        ProcessRowProjection, ProcessStatusFlags, ServiceRecord, ServiceState, ServicesSlice,
        ServicesSortColumn, SortDirection,
    };

    use super::*;

    fn sample_service(
        name: &str,
        display_name: &str,
        description: Option<&str>,
        pid: Option<u32>,
        state: ServiceState,
        group: Option<&str>,
    ) -> ServiceRecord {
        ServiceRecord {
            name: Arc::from(name),
            display_name: Arc::from(display_name),
            description: description.map(Arc::from),
            pid,
            state,
            group: group.map(Arc::from),
        }
    }

    #[test]
    fn service_description_shared_reuses_trimmed_arc_and_falls_back() {
        let with_desc = sample_service(
            "Spooler",
            "Print Spooler",
            Some("Loads files to memory"),
            Some(2040),
            ServiceState::Running,
            None,
        );
        assert_eq!(
            service_description_shared(&with_desc).as_ref(),
            "Loads files to memory"
        );

        let padded_desc = sample_service(
            "Spooler",
            "Print Spooler",
            Some("  Trimmed description  "),
            None,
            ServiceState::Stopped,
            None,
        );
        assert_eq!(
            service_description_shared(&padded_desc).as_ref(),
            "Trimmed description"
        );

        let fallback_display = sample_service(
            "Dhcp",
            "DHCP Client",
            Some("   "),
            Some(1200),
            ServiceState::Running,
            None,
        );
        assert_eq!(
            service_description_shared(&fallback_display).as_ref(),
            "DHCP Client"
        );

        let empty = sample_service("Empty", "   ", None, None, ServiceState::Unknown, None);
        assert_eq!(
            service_description_shared(&empty).as_ref(),
            UNAVAILABLE_LABEL
        );
    }

    #[test]
    fn services_viewport_fingerprint_detects_visible_changes_and_ignores_offscreen_edits() {
        let mut slice = ServicesSlice {
            revision: 1,
            sort_column: ServicesSortColumn::Name,
            sort_direction: SortDirection::Ascending,
            filter_pid: None,
            items: vec![
                sample_service(
                    "Alpha",
                    "Alpha",
                    None,
                    Some(100),
                    ServiceState::Running,
                    None,
                ),
                sample_service("Beta", "Beta", None, Some(200), ServiceState::Stopped, None),
                sample_service(
                    "Gamma",
                    "Gamma",
                    None,
                    Some(300),
                    ServiceState::Running,
                    None,
                ),
            ],
            visible_indices: vec![0, 1, 2],
            ..ServicesSlice::default()
        };

        let fp_initial = compute_services_viewport_fingerprint(&slice, &(0..2), Some(0));

        // Mutating an off-screen row (index 2) keeps the visible fingerprint identical.
        slice.items[2].state = ServiceState::Stopped;
        let fp_offscreen = compute_services_viewport_fingerprint(&slice, &(0..2), Some(0));
        assert_eq!(fp_initial, fp_offscreen);

        // Mutating a visible row (index 1) changes the fingerprint.
        slice.items[1].state = ServiceState::Running;
        let fp_visible = compute_services_viewport_fingerprint(&slice, &(0..2), Some(0));
        assert_ne!(fp_initial, fp_visible);

        // Changing sort direction changes the fingerprint even if visible rows match.
        let fp_before_sort = fp_visible;
        slice.sort_direction = SortDirection::Descending;
        let fp_after_sort = compute_services_viewport_fingerprint(&slice, &(0..2), Some(0));
        assert_ne!(fp_before_sort, fp_after_sort);
    }

    #[test]
    fn services_scroll_coalescer_enforces_quiet_window_max_defer_and_bypass() {
        let mut coalescer = ScrollCoalescer::default();
        let t0 = Instant::now();

        // No scroll history -> apply immediately.
        assert_eq!(
            coalescer.decide(t0, None, false),
            CoalesceDecision::ApplyNow
        );

        // Active scroll (40 ms ago) -> defer by remaining 110 ms quiet window.
        let t1 = t0 + Duration::from_millis(40);
        assert_eq!(
            coalescer.decide(t1, Some(t0), false),
            CoalesceDecision::Defer(Duration::from_millis(110))
        );

        // User-initiated bypass applies immediately and clears deferred timer.
        assert_eq!(
            coalescer.decide(t1, Some(t0), true),
            CoalesceDecision::ApplyNow
        );

        // Continuous scroll hits the 500 ms max defer cap.
        let start = t0 + Duration::from_secs(1);
        assert!(matches!(
            coalescer.decide(start, Some(start), false),
            CoalesceDecision::Defer(_)
        ));
        let near_cap = start + Duration::from_millis(450);
        assert_eq!(
            coalescer.decide(near_cap, Some(near_cap), false),
            CoalesceDecision::Defer(Duration::from_millis(50))
        );
        let at_cap = start + Duration::from_millis(500);
        assert_eq!(
            coalescer.decide(at_cap, Some(at_cap), false),
            CoalesceDecision::ApplyNow
        );
    }

    #[test]
    fn resolve_process_key_by_pid_finds_live_process_row() {
        let mut snapshot = AppSnapshot::default();
        let target_key = ProcessKey::new(4321, 99_000);
        Arc::make_mut(&mut snapshot.processes)
            .rows
            .push(ProcessRowProjection {
                kind: ProcessRowKind::Process,
                key: target_key,
                depth: 0,
                has_children: false,
                is_expanded: false,
                child_count: 0,
                name: Arc::from("svchost.exe"),
                display_name: Arc::from("Host Process for Windows Services"),
                window_title: None,
                publisher: None,
                exe_path: None,
                command_line: None,
                icon: None,
                category: ProcessCategory::System,
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
            });

        assert_eq!(
            resolve_process_key_by_pid(&snapshot, 4321),
            Some(target_key)
        );
        assert_eq!(resolve_process_key_by_pid(&snapshot, 0), None);
        assert_eq!(resolve_process_key_by_pid(&snapshot, 9999), None);
    }

    #[gpui_kit::test]
    fn services_view_acquires_lease_dispatches_sort_and_clears_pid_filter_on_drop(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().expect("tempdir");
        let config_store = ConfigStore::new(dir.path().join("settings.toml"));
        let settings = AppSettings::default();
        let backend = NativeBackend::initialize().expect("initialize native backend");
        let coordinator =
            Arc::new(EngineCoordinator::new(backend, settings.clone()).expect("start coordinator"));

        assert!(!coordinator.leases().is_active(MonitorDomain::Services));

        let store = cx.new(|cx| UiStore::new(coordinator.clone(), config_store, settings, cx));
        let shell_handle = ShellHandle::new(|_, _, _| {});

        let mut view_handle = None;
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                let view = cx.new(|cx| {
                    ServicesView::new(store.clone(), Some(1234), shell_handle.clone(), window, cx)
                });
                view_handle = Some(view.clone());
                cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
            })
            .expect("open services test window")
        });
        let view = view_handle.expect("ServicesView constructed");

        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.update(|window, cx| window.draw(cx).clear(cx));

        assert!(
            coordinator.leases().is_active(MonitorDomain::Services),
            "mounting ServicesView must acquire MonitorDomain::Services lease"
        );

        // Trigger backend sort dispatch and clear PID filter cleanly.
        view.update(&mut vcx, |view, cx| {
            view.table.update(cx, |table, cx| {
                table.delegate_mut().trigger_sort(1, cx);
            });
            view.set_filter_pid(Some(5678), cx);
        });
        vcx.run_until_parked();

        drop(view);
        vcx.update(|window, _| window.remove_window());
        vcx.run_until_parked();

        assert!(
            !coordinator.leases().is_active(MonitorDomain::Services),
            "dropping ServicesView must release MonitorDomain::Services lease"
        );

        coordinator.shutdown();
    }
}
