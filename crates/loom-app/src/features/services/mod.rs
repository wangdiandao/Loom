//! On-demand Services vertical feature slice gated by an RAII [`ScopedMonitorLease`].
//!
//! Splits the static toolbar + PID filter banner ([`ServicesView`]) from the
//! 2D-virtualized `gpui-kit` `DataTable` (`Entity<TableState<ServicesTableDelegate>>`)
//! so live snapshot ticks refresh only the table entity while hidden routes incur
//! zero SCM polling overhead.

use std::cmp::Ordering;
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
    AppSnapshot, CapabilityFlags, LanguagePreference, MonitorDomain, ProcessKey, ProcessRowKind,
    ServiceRecord, ServiceState, ServicesSlice, SortDirection,
};

use crate::bridge::{ScopedMonitorLease, UiCommands, UiStore, UiStoreEvent};
use crate::features::table_columns::DataTablePane;
use crate::i18n;
use crate::shell::ShellHandle;

const SERVICES_HEADER_HEIGHT: f32 = 32.0;

/// Sortable columns displayed by [`ServicesView`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ServicesSortColumn {
    Name,
    Pid,
    Description,
    Status,
    Group,
}

impl ServicesSortColumn {
    const ALL: [Self; 5] = [
        Self::Name,
        Self::Pid,
        Self::Description,
        Self::Status,
        Self::Group,
    ];

    #[inline]
    const fn slug(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Pid => "pid",
            Self::Description => "description",
            Self::Status => "status",
            Self::Group => "group",
        }
    }

    #[inline]
    const fn width(self) -> f32 {
        match self {
            Self::Name => 200.0,
            Self::Pid => 84.0,
            Self::Description => 340.0,
            Self::Status => 120.0,
            Self::Group => 180.0,
        }
    }

    #[inline]
    const fn min_width(self) -> f32 {
        match self {
            Self::Name => 120.0,
            Self::Pid => 64.0,
            Self::Description => 140.0,
            Self::Status => 84.0,
            Self::Group => 96.0,
        }
    }

    #[inline]
    const fn right_aligned(self) -> bool {
        matches!(self, Self::Pid)
    }

    #[inline]
    const fn default_direction(self) -> SortDirection {
        match self {
            Self::Status => SortDirection::Descending,
            Self::Name | Self::Pid | Self::Description | Self::Group => SortDirection::Ascending,
        }
    }

    fn label(self) -> String {
        match self {
            Self::Name => i18n::tr("services.col_name"),
            Self::Pid => i18n::tr("services.col_pid"),
            Self::Description => i18n::tr("services.col_description"),
            Self::Status => i18n::tr("services.col_status"),
            Self::Group => i18n::tr("services.col_group"),
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
const fn service_state_rank(state: ServiceState) -> u8 {
    match state {
        ServiceState::Running => 7,
        ServiceState::StartPending => 6,
        ServiceState::ContinuePending => 5,
        ServiceState::Paused => 4,
        ServiceState::PausePending => 3,
        ServiceState::StopPending => 2,
        ServiceState::Stopped => 1,
        ServiceState::Unknown => 0,
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
    "—"
}

/// Case-insensitive substring check without allocating per-haystack lowercase copies when possible.
#[inline]
fn contains_ascii_or_unicode_icase(haystack: &str, needle_lower: &str) -> bool {
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

/// Returns `true` when `record` matches both the optional PID filter and global search query.
pub(super) fn matches_service_filter(
    record: &ServiceRecord,
    filter_pid: Option<u32>,
    query_lower: &str,
) -> bool {
    if let Some(target_pid) = filter_pid
        && record.pid != Some(target_pid)
    {
        return false;
    }
    if query_lower.is_empty() {
        return true;
    }
    if contains_ascii_or_unicode_icase(&record.name, query_lower)
        || contains_ascii_or_unicode_icase(&record.display_name, query_lower)
    {
        return true;
    }
    if record
        .description
        .as_deref()
        .is_some_and(|d| contains_ascii_or_unicode_icase(d, query_lower))
    {
        return true;
    }
    if record
        .group
        .as_deref()
        .is_some_and(|g| contains_ascii_or_unicode_icase(g, query_lower))
    {
        return true;
    }
    if let Some(pid) = record.pid
        && pid.to_string().contains(query_lower)
    {
        return true;
    }
    false
}

/// Filters and sorts `items` into indices suitable for O(1) virtualized table lookups.
pub(super) fn filter_and_sort_services(
    items: &[ServiceRecord],
    filter_pid: Option<u32>,
    search_query: &str,
    sort_col: ServicesSortColumn,
    sort_dir: SortDirection,
) -> Vec<usize> {
    let query_lower = search_query.trim().to_lowercase();
    let mut indices: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, record)| matches_service_filter(record, filter_pid, &query_lower))
        .map(|(idx, _)| idx)
        .collect();

    indices.sort_by(|&a_idx, &b_idx| {
        let a = &items[a_idx];
        let b = &items[b_idx];
        let ord = match sort_col {
            ServicesSortColumn::Name => cmp_icase(&a.name, &b.name),
            ServicesSortColumn::Pid => a.pid.unwrap_or(0).cmp(&b.pid.unwrap_or(0)),
            ServicesSortColumn::Description => {
                cmp_icase(service_description_text(a), service_description_text(b))
            }
            ServicesSortColumn::Status => service_state_rank(a.state)
                .cmp(&service_state_rank(b.state))
                .then_with(|| cmp_icase(&a.name, &b.name)),
            ServicesSortColumn::Group => cmp_icase(
                a.group.as_deref().unwrap_or(""),
                b.group.as_deref().unwrap_or(""),
            ),
        };
        let ordered = match sort_dir {
            SortDirection::Ascending => ord,
            SortDirection::Descending => ord.reverse(),
        };
        ordered.then_with(|| cmp_icase(&a.name, &b.name))
    });

    indices
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

/// Action eligibility derived from the currently selected service row.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ServicesToolbarState {
    can_start: bool,
    can_stop: bool,
    can_restart: bool,
}

/// Cached localized strings for [`ServicesTableDelegate`].
struct ServicesLocaleCache {
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
    filter_pid: Option<u32>,
    search_query: String,
    sort_col: ServicesSortColumn,
    sort_dir: SortDirection,
    visible_indices: Vec<usize>,
    selected_name: Option<Arc<str>>,
    locale: ServicesLocaleCache,
}

impl ServicesTableDelegate {
    fn new(
        store: Entity<UiStore>,
        filter_pid: Option<u32>,
        shell_handle: ShellHandle,
        cx: &App,
    ) -> Self {
        let store_ref = store.read(cx);
        let snap = store_ref.snapshot();
        let slice = Arc::clone(&snap.services);
        let capabilities = snap.capabilities;
        let search_query = store_ref.search_query().to_owned();
        let sort_col = ServicesSortColumn::Name;
        let sort_dir = SortDirection::Ascending;
        let visible_indices =
            filter_and_sort_services(&slice.items, filter_pid, &search_query, sort_col, sort_dir);

        Self {
            store: store.clone(),
            shell_handle,
            last_rev: snap.revisions.services,
            slice,
            capabilities,
            filter_pid,
            search_query,
            sort_col,
            sort_dir,
            visible_indices,
            selected_name: None,
            locale: ServicesLocaleCache::build(),
        }
    }

    fn rebuild_rows(&mut self) {
        self.visible_indices = filter_and_sort_services(
            &self.slice.items,
            self.filter_pid,
            &self.search_query,
            self.sort_col,
            self.sort_dir,
        );
    }

    fn row_at(&self, row_ix: usize) -> Option<&ServiceRecord> {
        let item_idx = *self.visible_indices.get(row_ix)?;
        self.slice.items.get(item_idx)
    }

    fn find_visible_index_by_name(&self, name: &str) -> Option<usize> {
        self.visible_indices
            .iter()
            .position(|&idx| self.slice.items[idx].name.as_ref() == name)
    }

    fn selected_record(&self) -> Option<&ServiceRecord> {
        let sel = self.selected_name.as_deref()?;
        let row_ix = self.find_visible_index_by_name(sel)?;
        self.row_at(row_ix)
    }

    fn toolbar_state(&self) -> ServicesToolbarState {
        if !self.capabilities.contains(CapabilityFlags::SERVICES) {
            return ServicesToolbarState::default();
        }
        let Some(record) = self.selected_record() else {
            return ServicesToolbarState::default();
        };
        ServicesToolbarState {
            can_start: can_start_service(record.state),
            can_stop: can_stop_or_restart_service(record.state),
            can_restart: can_stop_or_restart_service(record.state),
        }
    }

    fn toggle_sort(&mut self, col_ix: usize) {
        let Some(&target) = ServicesSortColumn::ALL.get(col_ix) else {
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

impl TableDelegate for ServicesTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        ServicesSortColumn::ALL.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.visible_indices.len()
    }

    fn loading(&self, _cx: &App) -> bool {
        self.slice.revision == 0
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        let Some(&col) = ServicesSortColumn::ALL.get(col_ix) else {
            return Column::default();
        };
        let mut column = Column::new(col.slug(), col.label())
            .width(px(col.width()))
            .min_width(px(col.min_width()))
            .selectable(false)
            .movable(false);

        if col == ServicesSortColumn::Name {
            column = column.fixed_left();
        }
        if col.right_aligned() {
            column = column.text_right();
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
        let Some(&col) = ServicesSortColumn::ALL.get(col_ix) else {
            return Empty.into_any_element();
        };
        let is_active = col == self.sort_col;
        let fg = cx.theme().foreground;
        let muted_fg = cx.theme().muted_foreground;

        h_flex()
            .id(("services-th", col_ix))
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .when(col.right_aligned(), |d| d.justify_end())
            .text_xs()
            .text_color(if is_active { fg } else { muted_fg })
            .when(is_active, |d| d.font_semibold())
            .cursor_pointer()
            .on_click(cx.listener(move |table, _, _, cx| {
                table.delegate_mut().toggle_sort(col_ix);
                let sel = table
                    .delegate()
                    .selected_name
                    .as_deref()
                    .and_then(|name| table.delegate().find_visible_index_by_name(name));
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
        div().id(("services-row", row_ix)).text_xs()
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
        let Some(&col) = ServicesSortColumn::ALL.get(col_ix) else {
            return Empty.into_any_element();
        };

        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let primary = theme.primary;

        match col {
            ServicesSortColumn::Name => h_flex()
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
            ServicesSortColumn::Pid => {
                let text = record
                    .pid
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "—".to_owned());
                h_flex()
                    .size_full()
                    .items_center()
                    .justify_end()
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .text_color(muted_fg)
                    .child(text)
                    .into_any_element()
            }
            ServicesSortColumn::Description => h_flex()
                .size_full()
                .items_center()
                .overflow_hidden()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(muted_fg)
                        .child(service_description_text(record).to_owned()),
                )
                .into_any_element(),
            ServicesSortColumn::Status => {
                let color = if record.state == ServiceState::Running {
                    primary
                } else {
                    muted_fg
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
                            .child(self.locale.state_label(record.state)),
                    )
                    .into_any_element()
            }
            ServicesSortColumn::Group => {
                let text = record
                    .group
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
    _lease: ScopedMonitorLease,
    table: Entity<TableState<ServicesTableDelegate>>,
    table_pane: Entity<DataTablePane<ServicesTableDelegate>>,
    filter_pid: Option<u32>,
    toolbar_state: ServicesToolbarState,
    services_supported: bool,
    last_locale: LanguagePreference,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for ServicesView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.table.focus_handle(cx)
    }
}

impl ServicesView {
    /// Creates a new [`ServicesView`], immediately acquiring `MonitorDomain::Services`.
    pub(crate) fn new(
        store: Entity<UiStore>,
        filter_pid: Option<u32>,
        shell_handle: ShellHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let lease = store.read(cx).acquire_lease(MonitorDomain::Services);
        let (services_supported, last_locale) = {
            let s = store.read(cx);
            (
                s.snapshot()
                    .capabilities
                    .contains(CapabilityFlags::SERVICES),
                s.settings().language,
            )
        };

        let table = cx.new(|cx| {
            let delegate = ServicesTableDelegate::new(store.clone(), filter_pid, shell_handle, cx);
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
        let toolbar_state = table.read(cx).delegate().toolbar_state();

        let mut subscriptions = Vec::with_capacity(2);

        let table_for_store = table.clone();
        subscriptions.push(
            cx.subscribe(&store, move |this, store, event, cx| match event {
                UiStoreEvent::SnapshotUpdated(revs) => {
                    let current_rev = table_for_store.read(cx).delegate().last_rev;
                    if revs.services == current_rev {
                        return;
                    }
                    let (next_slice, next_caps) = {
                        let snap = store.read(cx).snapshot();
                        (Arc::clone(&snap.services), snap.capabilities)
                    };
                    let next_toolbar = table_for_store.update(cx, |table, cx| {
                        let d = table.delegate_mut();
                        d.slice = next_slice;
                        d.capabilities = next_caps;
                        d.last_rev = revs.services;
                        d.rebuild_rows();
                        Self::sync_table_selection(table, cx);
                        cx.notify();
                        table.delegate().toolbar_state()
                    });
                    let supported = next_caps.contains(CapabilityFlags::SERVICES);
                    if this.toolbar_state != next_toolbar || this.services_supported != supported {
                        this.toolbar_state = next_toolbar;
                        this.services_supported = supported;
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
                            table.delegate_mut().locale = ServicesLocaleCache::build();
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
                        let name = table
                            .delegate()
                            .row_at(*row_ix)
                            .map(|r| Arc::clone(&r.name));
                        table.delegate_mut().selected_name = name;
                        table.delegate().toolbar_state()
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
                        table.delegate_mut().selected_name = name.clone();
                        if name.is_some() && table.selected_row() != Some(*row_ix) {
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
                        table.delegate_mut().selected_name = None;
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
            filter_pid,
            toolbar_state,
            services_supported,
            last_locale,
            _subscriptions: subscriptions,
        }
    }

    fn sync_table_selection(
        table: &mut TableState<ServicesTableDelegate>,
        cx: &mut Context<TableState<ServicesTableDelegate>>,
    ) {
        let selected_idx = table
            .delegate()
            .selected_name
            .as_deref()
            .and_then(|name| table.delegate().find_visible_index_by_name(name));
        if let Some(idx) = selected_idx {
            if table.selected_row() != Some(idx) {
                table.set_selected_row(idx, cx);
            }
        } else if table.selected_row().is_some() {
            table.clear_selection(cx);
        }
    }

    /// Updates or clears the active host PID filter (e.g., from `NavigationIntent::JumpToServices`).
    pub(crate) fn set_filter_pid(&mut self, pid: Option<u32>, cx: &mut Context<Self>) {
        if self.filter_pid == pid {
            return;
        }
        self.filter_pid = pid;
        let next_toolbar = self.table.update(cx, |table, cx| {
            let d = table.delegate_mut();
            d.filter_pid = pid;
            d.rebuild_rows();
            Self::sync_table_selection(table, cx);
            cx.notify();
            table.delegate().toolbar_state()
        });
        self.toolbar_state = next_toolbar;
        cx.notify();
    }

    fn selected_service_name(&self, cx: &App) -> Option<String> {
        self.table
            .read(cx)
            .delegate()
            .selected_record()
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

    use gpui_kit::{TestAppContext, VisualTestContext};
    use loom_config::{AppSettings, ConfigStore};
    use loom_engine::EngineCoordinator;
    use loom_pal::{NativeBackend, PlatformBackend};
    use loom_types::{
        AppSnapshot, MonitorDomain, ProcessCategory, ProcessKey, ProcessRowKind,
        ProcessRowProjection, ProcessStatusFlags, ServiceRecord, ServiceState, SortDirection,
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
    fn services_filter_and_sort_cover_all_fields_and_pid_filter() {
        let items = vec![
            sample_service(
                "Spooler",
                "Print Spooler",
                Some("Loads files to memory for later printing"),
                Some(2040),
                ServiceState::Running,
                Some("SpoolerGroup"),
            ),
            sample_service(
                "Dhcp",
                "DHCP Client",
                None,
                Some(1200),
                ServiceState::Running,
                Some("LocalServiceNetworkRestricted"),
            ),
            sample_service(
                "wuauserv",
                "Windows Update",
                Some("Enables detection and installation of updates"),
                None,
                ServiceState::Stopped,
                Some("netsvcs"),
            ),
        ];

        // Sort by Name ascending (case-insensitive).
        let by_name = filter_and_sort_services(
            &items,
            None,
            "",
            ServicesSortColumn::Name,
            SortDirection::Ascending,
        );
        assert_eq!(by_name, vec![1, 0, 2]);

        // Sort by PID descending.
        let by_pid = filter_and_sort_services(
            &items,
            None,
            "",
            ServicesSortColumn::Pid,
            SortDirection::Descending,
        );
        assert_eq!(by_pid, vec![0, 1, 2]);

        // Filter by PID matches strictly.
        let pid_only = filter_and_sort_services(
            &items,
            Some(1200),
            "",
            ServicesSortColumn::Name,
            SortDirection::Ascending,
        );
        assert_eq!(pid_only, vec![1]);

        // Search across description, group, and PID.
        assert_eq!(
            filter_and_sort_services(
                &items,
                None,
                "printing",
                ServicesSortColumn::Name,
                SortDirection::Ascending,
            ),
            vec![0]
        );
        assert_eq!(
            filter_and_sort_services(
                &items,
                None,
                "netsvcs",
                ServicesSortColumn::Name,
                SortDirection::Ascending,
            ),
            vec![2]
        );
        assert_eq!(
            filter_and_sort_services(
                &items,
                None,
                "2040",
                ServicesSortColumn::Name,
                SortDirection::Ascending,
            ),
            vec![0]
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
            });

        assert_eq!(
            resolve_process_key_by_pid(&snapshot, 4321),
            Some(target_key)
        );
        assert_eq!(resolve_process_key_by_pid(&snapshot, 0), None);
        assert_eq!(resolve_process_key_by_pid(&snapshot, 9999), None);
    }

    #[gpui_kit::test]
    fn services_view_acquires_and_releases_monitor_lease_on_mount_and_drop(
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
