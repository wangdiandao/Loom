//! On-demand Users vertical feature slice gated by an RAII [`ScopedMonitorLease`].
//!
//! Splits the static toolbar ([`UsersView`]) from the expandable session-tree
//! 2D-virtualized `gpui-kit` `DataTable` (`Entity<TableState<UsersTableDelegate>>`)
//! so hidden routes incur zero WTS session polling or per-session aggregation overhead.
//! Sorting, filtering, and session-tree flattening are offloaded to `loom-engine`.

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
    IntoElement, MouseButton, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
    prelude::FluentBuilder as _, px,
};
use loom_types::{
    CapabilityFlags, LanguagePreference, MonitorDomain, ProcessKey, ProcessStatusFlags,
    ResourceDisplayMode, RunTaskRequest, SortDirection, UserSessionProcess, UserSessionState,
    UsersRowProjection, UsersSlice, UsersSortColumn,
};
use loom_ui_kit::{
    CachedSemanticColors, HeatmapLevel, RowTextCache, format_child_count_shared,
    format_permille_metric, heatmap_cell_metric, quantize_bytes_display, quantize_net_bps_display,
    quantize_rate_bps_display,
};

use crate::bridge::{ScopedMonitorLease, UiCommands, UiStore, UiStoreEvent};
use crate::features::table_columns::DataTablePane;
use crate::i18n;
use crate::shell::ShellHandle;

const USERS_HEADER_HEIGHT: f32 = 32.0;
const SCROLL_COALESCE_QUIET_WINDOW: Duration = Duration::from_millis(150);
const SCROLL_COALESCE_MAX_DEFER: Duration = Duration::from_millis(500);
const DEFAULT_FINGERPRINT_WINDOW: usize = 64;

/// Sentinel creation timestamp used to cache top-level session aggregate metrics inside
/// [`RowTextCache`] without colliding with any real OS [`ProcessKey`].
const SESSION_CACHE_CREATE_TIME_SENTINEL: u64 = u64::MAX;

/// Fallback total physical memory (16 GiB) used only before the first performance sample arrives.
const FALLBACK_TOTAL_RAM_BYTES: u64 = 16 * 1024 * 1024 * 1024;

const USERS_COLUMNS: [UsersSortColumn; 6] = [
    UsersSortColumn::User,
    UsersSortColumn::Status,
    UsersSortColumn::Cpu,
    UsersSortColumn::Memory,
    UsersSortColumn::Disk,
    UsersSortColumn::Network,
];

#[inline]
const fn column_slug(col: UsersSortColumn) -> &'static str {
    match col {
        UsersSortColumn::User => "user",
        UsersSortColumn::Status => "status",
        UsersSortColumn::Cpu => "cpu",
        UsersSortColumn::Memory => "memory",
        UsersSortColumn::Disk => "disk",
        UsersSortColumn::Network => "network",
    }
}

#[inline]
const fn column_width(col: UsersSortColumn) -> f32 {
    match col {
        UsersSortColumn::User => 280.0,
        UsersSortColumn::Status => 120.0,
        UsersSortColumn::Cpu => 92.0,
        UsersSortColumn::Memory => 128.0,
        UsersSortColumn::Disk => 116.0,
        UsersSortColumn::Network => 116.0,
    }
}

#[inline]
const fn column_min_width(col: UsersSortColumn) -> f32 {
    match col {
        UsersSortColumn::User => 140.0,
        UsersSortColumn::Status => 84.0,
        UsersSortColumn::Cpu => 68.0,
        UsersSortColumn::Memory => 88.0,
        UsersSortColumn::Disk => 88.0,
        UsersSortColumn::Network => 88.0,
    }
}

#[inline]
const fn column_right_aligned(col: UsersSortColumn) -> bool {
    matches!(
        col,
        UsersSortColumn::Cpu
            | UsersSortColumn::Memory
            | UsersSortColumn::Disk
            | UsersSortColumn::Network
    )
}

#[inline]
const fn column_is_heatmap(col: UsersSortColumn) -> bool {
    matches!(
        col,
        UsersSortColumn::Cpu
            | UsersSortColumn::Memory
            | UsersSortColumn::Disk
            | UsersSortColumn::Network
    )
}

#[inline]
const fn column_default_direction(col: UsersSortColumn) -> SortDirection {
    match col {
        UsersSortColumn::Cpu
        | UsersSortColumn::Memory
        | UsersSortColumn::Disk
        | UsersSortColumn::Network
        | UsersSortColumn::Status => SortDirection::Descending,
        UsersSortColumn::User => SortDirection::Ascending,
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
fn memory_heatmap_level(bytes: u64, total_ram_bytes: u64) -> HeatmapLevel {
    let denom = if total_ram_bytes > 0 {
        total_ram_bytes
    } else {
        FALLBACK_TOTAL_RAM_BYTES
    };
    let permille = ((u128::from(bytes) * 1_000) / u128::from(denom)).min(1_000) as u16;
    HeatmapLevel::from_permille(permille)
}

#[inline]
fn is_critical_or_system_process(status: ProcessStatusFlags) -> bool {
    status.intersects(
        ProcessStatusFlags::CRITICAL
            | ProcessStatusFlags::PROTECTED
            | ProcessStatusFlags::KERNEL
            | ProcessStatusFlags::SYSTEM_IDLE,
    )
}

/// Stable selection key anchoring rows across live refreshes and tree expansion toggles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum UsersRowKey {
    Session(u32),
    Process(ProcessKey),
}

#[inline]
fn row_key(row: &UsersRowProjection) -> UsersRowKey {
    match row {
        UsersRowProjection::Session { session_id, .. } => UsersRowKey::Session(*session_id),
        UsersRowProjection::Process { process, .. } => UsersRowKey::Process(process.key),
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
/// display-quantized metric keys so sub-tenth metric jitter does not force repaints.
pub(super) fn compute_users_viewport_fingerprint(
    slice: &UsersSlice,
    total_ram_bytes: u64,
    visible_rows: &Range<usize>,
    selected_row: Option<usize>,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    let total = slice.rows.len();
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

    for row in &slice.rows[range] {
        match row {
            UsersRowProjection::Session {
                session_id,
                user_name,
                state,
                process_count,
                cpu_permille,
                memory_bytes,
                disk_bytes_per_sec,
                net_bytes_per_sec,
                has_children,
                is_expanded,
            } => {
                0u8.hash(&mut hasher);
                session_id.hash(&mut hasher);
                user_name.hash(&mut hasher);
                state.hash(&mut hasher);
                process_count.hash(&mut hasher);
                cpu_permille.hash(&mut hasher);
                quantize_bytes_display(*memory_bytes).hash(&mut hasher);
                memory_heatmap_level(*memory_bytes, total_ram_bytes).hash(&mut hasher);
                quantize_rate_bps_display(*disk_bytes_per_sec).hash(&mut hasher);
                HeatmapLevel::from_bytes_per_sec(*disk_bytes_per_sec).hash(&mut hasher);
                quantize_net_bps_display(*net_bytes_per_sec).hash(&mut hasher);
                HeatmapLevel::from_bytes_per_sec(*net_bytes_per_sec).hash(&mut hasher);
                has_children.hash(&mut hasher);
                is_expanded.hash(&mut hasher);
            }
            UsersRowProjection::Process {
                session_id,
                process,
            } => {
                1u8.hash(&mut hasher);
                session_id.hash(&mut hasher);
                process.key.hash(&mut hasher);
                process.status.bits().hash(&mut hasher);
                process.cpu_permille.hash(&mut hasher);
                quantize_bytes_display(process.memory_bytes).hash(&mut hasher);
                memory_heatmap_level(process.memory_bytes, total_ram_bytes).hash(&mut hasher);
                quantize_rate_bps_display(process.disk_bytes_per_sec).hash(&mut hasher);
                HeatmapLevel::from_bytes_per_sec(process.disk_bytes_per_sec).hash(&mut hasher);
                quantize_net_bps_display(process.net_bytes_per_sec).hash(&mut hasher);
                HeatmapLevel::from_bytes_per_sec(process.net_bytes_per_sec).hash(&mut hasher);
            }
        }
    }

    hasher.finish()
}

/// Opens a confirmation dialog before disconnecting a user session.
fn open_disconnect_session_dialog(
    store: &Entity<UiStore>,
    session_id: u32,
    user_name: String,
    window: &mut Window,
    cx: &mut App,
) {
    let store = store.clone();
    window.open_dialog(cx, move |dialog, _window, cx| {
        let store_confirm = store.clone();
        let muted_fg = cx.theme().muted_foreground;
        let warning = cx.theme().warning;
        let danger = cx.theme().danger;
        let radius = cx.theme().radius;
        let message = rust_i18n::t!(
            "users.confirm_disconnect_message",
            user = user_name.as_str(),
            session_id = session_id
        )
        .into_owned();

        dialog
            .w(px(440.))
            .title(i18n::tr("users.confirm_disconnect_title"))
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
                            .child(message),
                    ),
            )
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("users-confirm-disconnect-cancel")
                            .outline()
                            .label(i18n::tr("common.cancel"))
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("users-confirm-disconnect-ok")
                            .danger()
                            .label(i18n::tr("users.confirm_disconnect_action"))
                            .on_click(move |_, window, cx| {
                                UiCommands::disconnect_session(&store_confirm, session_id, cx);
                                window.close_dialog(cx);
                            }),
                    ),
            )
    });
}

/// Terminates a child process directly, or opens a confirmation dialog for critical processes.
fn request_end_user_process(
    store: &Entity<UiStore>,
    process: &UserSessionProcess,
    window: &mut Window,
    cx: &mut App,
) {
    if !is_critical_or_system_process(process.status) {
        UiCommands::terminate_process(store, process.key, cx);
        return;
    }

    let store = store.clone();
    let key = process.key;
    let name = process.name.to_string();
    window.open_dialog(cx, move |dialog, _window, cx| {
        let store_confirm = store.clone();
        let warning = cx.theme().warning;
        let danger = cx.theme().danger;
        let muted_fg = cx.theme().muted_foreground;
        let radius = cx.theme().radius;
        let message = rust_i18n::t!(
            "processes.confirm_end_message",
            name = name.as_str(),
            pid = key.pid
        )
        .into_owned();

        dialog
            .w(px(440.))
            .title(i18n::tr("processes.confirm_end_title"))
            .child(
                v_flex()
                    .w_full()
                    .gap_3()
                    .child(div().text_sm().text_color(muted_fg).child(message))
                    .child(
                        h_flex()
                            .w_full()
                            .items_start()
                            .gap_2()
                            .p_2p5()
                            .rounded(radius)
                            .bg(warning.opacity(0.14))
                            .border_1()
                            .border_color(warning.opacity(0.4))
                            .child(
                                Icon::new(IconName::TriangleAlert)
                                    .small()
                                    .text_color(danger),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_xs()
                                    .text_color(danger)
                                    .child(i18n::tr("processes.confirm_end_critical")),
                            ),
                    ),
            )
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("users-confirm-end-cancel")
                            .outline()
                            .label(i18n::tr("common.cancel"))
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("users-confirm-end-ok")
                            .danger()
                            .label(i18n::tr("processes.confirm_end_action"))
                            .on_click(move |_, window, cx| {
                                UiCommands::terminate_process(&store_confirm, key, cx);
                                window.close_dialog(cx);
                            }),
                    ),
            )
    });
}

fn launch_manage_user_accounts(store: &Entity<UiStore>, cx: &mut App) {
    UiCommands::run_new_task(
        store,
        RunTaskRequest {
            command: "netplwiz".to_owned(),
            run_as_admin: false,
        },
        cx,
    );
}

/// Cached localized strings for [`UsersTableDelegate`].
struct UsersLocaleCache {
    col_user: SharedString,
    col_status: SharedString,
    col_cpu: SharedString,
    col_memory: SharedString,
    col_disk: SharedString,
    col_network: SharedString,
    active: SharedString,
    disconnected: SharedString,
    idle: SharedString,
    unknown: SharedString,
    running: SharedString,
    suspended: SharedString,
    loading: SharedString,
    empty: SharedString,
}

impl UsersLocaleCache {
    fn build() -> Self {
        Self {
            col_user: i18n::tr("users.col_user").into(),
            col_status: i18n::tr("users.col_status").into(),
            col_cpu: i18n::tr("users.col_cpu").into(),
            col_memory: i18n::tr("users.col_memory").into(),
            col_disk: i18n::tr("users.col_disk").into(),
            col_network: i18n::tr("users.col_network").into(),
            active: i18n::user_session_state_label(UserSessionState::Active).into(),
            disconnected: i18n::user_session_state_label(UserSessionState::Disconnected).into(),
            idle: i18n::user_session_state_label(UserSessionState::Idle).into(),
            unknown: i18n::user_session_state_label(UserSessionState::Unknown).into(),
            running: i18n::tr("details.status_running").into(),
            suspended: i18n::tr("details.status_suspended").into(),
            loading: i18n::tr("users.loading").into(),
            empty: i18n::tr("users.empty").into(),
        }
    }

    #[inline]
    fn column_label(&self, col: UsersSortColumn) -> SharedString {
        match col {
            UsersSortColumn::User => self.col_user.clone(),
            UsersSortColumn::Status => self.col_status.clone(),
            UsersSortColumn::Cpu => self.col_cpu.clone(),
            UsersSortColumn::Memory => self.col_memory.clone(),
            UsersSortColumn::Disk => self.col_disk.clone(),
            UsersSortColumn::Network => self.col_network.clone(),
        }
    }

    #[inline]
    fn state_label(&self, state: UserSessionState) -> SharedString {
        match state {
            UserSessionState::Active => self.active.clone(),
            UserSessionState::Disconnected => self.disconnected.clone(),
            UserSessionState::Idle => self.idle.clone(),
            UserSessionState::Unknown => self.unknown.clone(),
        }
    }
}

/// 2D-virtualized `gpui-kit` [`TableDelegate`] backing [`UsersView`].
struct UsersTableDelegate {
    store: Entity<UiStore>,
    shell_handle: ShellHandle,
    slice: Arc<UsersSlice>,
    capabilities: CapabilityFlags,
    total_ram_bytes: u64,
    last_rev: u64,
    selected_key: Option<UsersRowKey>,
    visible_rows: Range<usize>,
    last_scroll_at: Option<Instant>,
    last_viewport_fp: Option<u64>,
    user_action_pending: bool,
    semantic_colors: CachedSemanticColors,
    row_text_cache: RowTextCache,
    locale: UsersLocaleCache,
}

impl UsersTableDelegate {
    fn new(store: Entity<UiStore>, shell_handle: ShellHandle, cx: &App) -> Self {
        let store_ref = store.read(cx);
        let snap = store_ref.snapshot();
        let slice = Arc::clone(&snap.users);
        let capabilities = snap.capabilities;
        let total_ram_bytes = snap.performance.latest.mem_total_bytes;

        Self {
            store: store.clone(),
            shell_handle,
            last_rev: snap.revisions.users,
            slice,
            capabilities,
            total_ram_bytes,
            selected_key: None,
            visible_rows: 0..0,
            last_scroll_at: None,
            last_viewport_fp: None,
            user_action_pending: false,
            semantic_colors: CachedSemanticColors::new(),
            row_text_cache: RowTextCache::new(),
            locale: UsersLocaleCache::build(),
        }
    }

    /// Locates `key` in `slice.rows`, checking `hint_row` in O(1) before scanning.
    #[inline]
    fn find_row_index(&self, key: UsersRowKey, hint_row: Option<usize>) -> Option<usize> {
        if let Some(hint) = hint_row
            && self
                .slice
                .rows
                .get(hint)
                .is_some_and(|row| row_key(row) == key)
        {
            return Some(hint);
        }
        self.slice.rows.iter().position(|row| row_key(row) == key)
    }

    fn selected_session(&self, hint_row: Option<usize>) -> Option<(u32, String)> {
        let sel = self.selected_key?;
        let row_ix = self.find_row_index(sel, hint_row)?;
        match self.slice.rows.get(row_ix)? {
            UsersRowProjection::Session {
                session_id,
                user_name,
                ..
            } => Some((*session_id, user_name.to_string())),
            UsersRowProjection::Process { .. } => None,
        }
    }

    fn can_disconnect_selected(&self, hint_row: Option<usize>) -> bool {
        self.capabilities.contains(CapabilityFlags::USER_SESSIONS)
            && self.selected_session(hint_row).is_some()
    }

    fn trigger_sort(&mut self, col_ix: usize, cx: &mut Context<TableState<Self>>) {
        let Some(&target) = USERS_COLUMNS.get(col_ix) else {
            return;
        };
        let next_dir = if self.slice.sort_column == target {
            self.slice.sort_direction.toggle()
        } else {
            column_default_direction(target)
        };
        self.user_action_pending = true;
        let store = self.store.clone();
        UiCommands::set_users_sort(&store, target, next_dir, cx);
    }

    fn trigger_toggle_session(&mut self, session_id: u32, cx: &mut Context<TableState<Self>>) {
        self.user_action_pending = true;
        let store = self.store.clone();
        UiCommands::toggle_user_session_expanded(&store, session_id, cx);
    }
}

impl TableDelegate for UsersTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        USERS_COLUMNS.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.slice.rows.len()
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
        let Some(&col) = USERS_COLUMNS.get(col_ix) else {
            return Column::default();
        };
        let mut column = Column::new(column_slug(col), self.locale.column_label(col))
            .width(px(column_width(col)))
            .min_width(px(column_min_width(col)))
            .selectable(false)
            .movable(false);

        if col == UsersSortColumn::User {
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
            .id("users-column-header")
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
        let Some(&col) = USERS_COLUMNS.get(col_ix) else {
            return Empty.into_any_element();
        };
        let is_active = col == self.slice.sort_column;
        let fg = cx.theme().foreground;
        let muted_fg = cx.theme().muted_foreground;
        let label = self.locale.column_label(col);

        h_flex()
            .id(("users-th", col_ix))
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
        div().id(("users-row", row_ix)).text_xs()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(&col) = USERS_COLUMNS.get(col_ix) else {
            return Empty.into_any_element();
        };
        if row_ix >= self.slice.rows.len() {
            return Empty.into_any_element();
        }

        let theme = cx.theme();
        let colors = self.semantic_colors.get_copy(theme);
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let primary = theme.primary;
        let list_hover = theme.list_hover;
        let mono_font = theme.mono_font_family.clone();
        let total_ram_bytes = self.total_ram_bytes;

        match &self.slice.rows[row_ix] {
            UsersRowProjection::Session {
                session_id,
                user_name,
                state,
                process_count,
                cpu_permille,
                memory_bytes,
                disk_bytes_per_sec,
                net_bytes_per_sec,
                has_children,
                is_expanded,
            } => {
                let sid = *session_id;
                let state = *state;
                let process_count = *process_count;
                let cpu_permille = *cpu_permille;
                let memory_bytes = *memory_bytes;
                let disk_bytes_per_sec = *disk_bytes_per_sec;
                let net_bytes_per_sec = *net_bytes_per_sec;
                let has_children = *has_children;
                let is_expanded = *is_expanded;
                let cache_key = ProcessKey::new(sid, SESSION_CACHE_CREATE_TIME_SENTINEL);

                match col {
                    UsersSortColumn::User => {
                        let user_label = SharedString::from(Arc::clone(user_name));
                        let count_label = format_child_count_shared(process_count);
                        h_flex()
                            .size_full()
                            .items_center()
                            .gap_1p5()
                            .overflow_hidden()
                            .child(if has_children {
                                div()
                                    .id(("users-expand", sid as usize))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .size(px(18.))
                                    .flex_shrink_0()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .hover(move |s| s.bg(list_hover))
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation();
                                    })
                                    .on_click(cx.listener(move |table, _, _, cx| {
                                        table.delegate_mut().trigger_toggle_session(sid, cx);
                                    }))
                                    .child(
                                        Icon::new(if is_expanded {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronRight
                                        })
                                        .xsmall()
                                        .text_color(muted_fg),
                                    )
                                    .into_any_element()
                            } else {
                                div().w(px(18.)).flex_shrink_0().into_any_element()
                            })
                            .child(
                                Icon::new(IconName::User)
                                    .xsmall()
                                    .text_color(primary)
                                    .flex_shrink_0(),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .font_medium()
                                    .text_color(fg)
                                    .child(user_label),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_xs()
                                    .text_color(muted_fg)
                                    .child(count_label),
                            )
                            .into_any_element()
                    }
                    UsersSortColumn::Status => {
                        let color = if state == UserSessionState::Active {
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
                    UsersSortColumn::Cpu => heatmap_cell_metric(
                        format_permille_metric(cpu_permille),
                        HeatmapLevel::from_permille(cpu_permille),
                        &colors,
                        mono_font,
                    )
                    .into_any_element(),
                    UsersSortColumn::Memory => {
                        let metric = self.row_text_cache.memory_metric(
                            cache_key,
                            memory_bytes,
                            0,
                            ResourceDisplayMode::Values,
                        );
                        heatmap_cell_metric(
                            metric,
                            memory_heatmap_level(memory_bytes, total_ram_bytes),
                            &colors,
                            mono_font,
                        )
                        .into_any_element()
                    }
                    UsersSortColumn::Disk => {
                        let metric = self.row_text_cache.disk_metric(
                            cache_key,
                            disk_bytes_per_sec,
                            0,
                            ResourceDisplayMode::Values,
                        );
                        heatmap_cell_metric(
                            metric,
                            HeatmapLevel::from_bytes_per_sec(disk_bytes_per_sec),
                            &colors,
                            mono_font,
                        )
                        .into_any_element()
                    }
                    UsersSortColumn::Network => {
                        let metric = self.row_text_cache.network_metric(
                            cache_key,
                            net_bytes_per_sec,
                            0,
                            ResourceDisplayMode::Values,
                        );
                        heatmap_cell_metric(
                            metric,
                            HeatmapLevel::from_bytes_per_sec(net_bytes_per_sec),
                            &colors,
                            mono_font,
                        )
                        .into_any_element()
                    }
                }
            }
            UsersRowProjection::Process { process, .. } => {
                let proc_key = process.key;
                let status = process.status;
                let cpu_permille = process.cpu_permille;
                let memory_bytes = process.memory_bytes;
                let disk_bytes_per_sec = process.disk_bytes_per_sec;
                let net_bytes_per_sec = process.net_bytes_per_sec;

                match col {
                    UsersSortColumn::User => {
                        let display = if process.display_name.trim().is_empty() {
                            SharedString::from(Arc::clone(&process.name))
                        } else {
                            SharedString::from(Arc::clone(&process.display_name))
                        };
                        h_flex()
                            .size_full()
                            .items_center()
                            .gap_1p5()
                            .pl(px(28.))
                            .overflow_hidden()
                            .child(
                                Icon::new(IconName::File)
                                    .xsmall()
                                    .text_color(muted_fg)
                                    .flex_shrink_0(),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(fg)
                                    .child(display),
                            )
                            .into_any_element()
                    }
                    UsersSortColumn::Status => {
                        let text = if status.contains(ProcessStatusFlags::SUSPENDED) {
                            self.locale.suspended.clone()
                        } else {
                            self.locale.running.clone()
                        };
                        h_flex()
                            .size_full()
                            .items_center()
                            .truncate()
                            .text_xs()
                            .text_color(muted_fg)
                            .child(text)
                            .into_any_element()
                    }
                    UsersSortColumn::Cpu => heatmap_cell_metric(
                        format_permille_metric(cpu_permille),
                        HeatmapLevel::from_permille(cpu_permille),
                        &colors,
                        mono_font,
                    )
                    .into_any_element(),
                    UsersSortColumn::Memory => {
                        let metric = self.row_text_cache.memory_metric(
                            proc_key,
                            memory_bytes,
                            0,
                            ResourceDisplayMode::Values,
                        );
                        heatmap_cell_metric(
                            metric,
                            memory_heatmap_level(memory_bytes, total_ram_bytes),
                            &colors,
                            mono_font,
                        )
                        .into_any_element()
                    }
                    UsersSortColumn::Disk => {
                        let metric = self.row_text_cache.disk_metric(
                            proc_key,
                            disk_bytes_per_sec,
                            0,
                            ResourceDisplayMode::Values,
                        );
                        heatmap_cell_metric(
                            metric,
                            HeatmapLevel::from_bytes_per_sec(disk_bytes_per_sec),
                            &colors,
                            mono_font,
                        )
                        .into_any_element()
                    }
                    UsersSortColumn::Network => {
                        let metric = self.row_text_cache.network_metric(
                            proc_key,
                            net_bytes_per_sec,
                            0,
                            ResourceDisplayMode::Values,
                        );
                        heatmap_cell_metric(
                            metric,
                            HeatmapLevel::from_bytes_per_sec(net_bytes_per_sec),
                            &colors,
                            mono_font,
                        )
                        .into_any_element()
                    }
                }
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
        let Some(row) = self.slice.rows.get(row_ix).cloned() else {
            return menu;
        };

        match row {
            UsersRowProjection::Session {
                session_id,
                user_name,
                has_children,
                is_expanded,
                ..
            } => {
                let supported = self.capabilities.contains(CapabilityFlags::USER_SESSIONS);
                let store_disc = self.store.clone();
                let user_disc = user_name.to_string();
                let store_manage = self.store.clone();
                let table_weak = cx.entity().downgrade();

                let mut menu = menu;
                if has_children {
                    let toggle_label = if is_expanded {
                        i18n::tr("users.menu_collapse")
                    } else {
                        i18n::tr("users.menu_expand")
                    };
                    let toggle_icon = if is_expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    };
                    menu = menu
                        .item(PopupMenuItem::new(toggle_label).icon(toggle_icon).on_click(
                            move |_, _, cx| {
                                let _ = table_weak.update(cx, |table, cx| {
                                    table.delegate_mut().trigger_toggle_session(session_id, cx);
                                });
                            },
                        ))
                        .separator();
                }

                menu.item(
                    PopupMenuItem::new(i18n::tr("users.menu_disconnect"))
                        .icon(IconName::CircleX)
                        .disabled(!supported)
                        .on_click(move |_, window, cx| {
                            open_disconnect_session_dialog(
                                &store_disc,
                                session_id,
                                user_disc.clone(),
                                window,
                                cx,
                            );
                        }),
                )
                .item(
                    PopupMenuItem::new(i18n::tr("users.menu_manage_accounts"))
                        .icon(IconName::User)
                        .on_click(move |_, _, cx| {
                            launch_manage_user_accounts(&store_manage, cx);
                        }),
                )
            }
            UsersRowProjection::Process { process, .. } => {
                let store_end = self.store.clone();
                let proc_end = process.clone();
                let shell_details = self.shell_handle.clone();
                let key = process.key;
                let store_search = self.store.clone();
                let query = process.name.to_string();

                menu.item(
                    PopupMenuItem::new(i18n::tr("users.menu_end_task"))
                        .icon(IconName::CircleX)
                        .on_click(move |_, window, cx| {
                            request_end_user_process(&store_end, &proc_end, window, cx);
                        }),
                )
                .item(
                    PopupMenuItem::new(i18n::tr("users.menu_go_to_details"))
                        .icon(IconName::ArrowRight)
                        .on_click(move |_, window, cx| {
                            shell_details.jump_to_details(key, window, cx);
                        }),
                )
                .separator()
                .item(
                    PopupMenuItem::new(i18n::tr("users.menu_search_online"))
                        .icon(IconName::Globe)
                        .on_click(move |_, _, cx| {
                            UiCommands::search_online(&store_search, query.clone(), cx);
                        }),
                )
            }
        }
    }

    fn row_label(&self, row_ix: usize, _cx: &App) -> Option<SharedString> {
        match self.slice.rows.get(row_ix)? {
            UsersRowProjection::Session {
                user_name,
                process_count,
                state,
                ..
            } => Some(
                format!(
                    "{} ({}) - {}",
                    user_name,
                    process_count,
                    self.locale.state_label(*state)
                )
                .into(),
            ),
            UsersRowProjection::Process { process, .. } => {
                Some(format!("{} (PID {})", process.display_name, process.key.pid).into())
            }
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

/// On-demand logged-in user sessions view holding an RAII `MonitorDomain::Users` lease.
pub(crate) struct UsersView {
    store: Entity<UiStore>,
    _lease: ScopedMonitorLease,
    table: Entity<TableState<UsersTableDelegate>>,
    table_pane: Entity<DataTablePane<UsersTableDelegate>>,
    can_disconnect: bool,
    last_locale: LanguagePreference,
    scroll_coalescer: ScrollCoalescer,
    coalesce_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for UsersView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.table.focus_handle(cx)
    }
}

impl UsersView {
    /// Creates a new [`UsersView`], immediately acquiring `MonitorDomain::Users`.
    pub(crate) fn new(
        store: Entity<UiStore>,
        shell_handle: ShellHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let lease = store.read(cx).acquire_lease(MonitorDomain::Users);
        let last_locale = store.read(cx).settings().language;

        let table = cx.new(|cx| {
            let delegate = UsersTableDelegate::new(store.clone(), shell_handle, cx);
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
                px(USERS_HEADER_HEIGHT),
                false,
                i18n::tr("users.title"),
            )
        });
        let can_disconnect = {
            let t = table.read(cx);
            t.delegate().can_disconnect_selected(t.selected_row())
        };

        let mut subscriptions = Vec::with_capacity(2);

        let table_for_store = table.clone();
        subscriptions.push(
            cx.subscribe(&store, move |this, store, event, cx| match event {
                UiStoreEvent::SnapshotUpdated(revs) => {
                    let current_rev = table_for_store.read(cx).delegate().last_rev;
                    if revs.users == current_rev {
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
                            d.locale = UsersLocaleCache::build();
                            d.last_viewport_fp = None;
                            table.refresh(cx);
                            cx.notify();
                        });
                        this.table_pane.update(cx, |pane, cx| {
                            pane.set_accessibility_label(i18n::tr("users.title"), cx);
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
                    let next_can_disconnect = table.update(cx, |table, _| {
                        let key = table.delegate().slice.rows.get(*row_ix).map(row_key);
                        let d = table.delegate_mut();
                        d.selected_key = key;
                        d.last_viewport_fp = None;
                        d.can_disconnect_selected(Some(*row_ix))
                    });
                    if this.can_disconnect != next_can_disconnect {
                        this.can_disconnect = next_can_disconnect;
                        cx.notify();
                    }
                }
                TableEvent::RightClickedRow(Some(row_ix)) => {
                    let next_can_disconnect = table.update(cx, |table, cx| {
                        let key = table.delegate().slice.rows.get(*row_ix).map(row_key);
                        {
                            let d = table.delegate_mut();
                            d.selected_key = key;
                            d.last_viewport_fp = None;
                        }
                        if key.is_some() && table.selected_row() != Some(*row_ix) {
                            table.set_selected_row(*row_ix, cx);
                        }
                        table
                            .delegate()
                            .can_disconnect_selected(table.selected_row())
                    });
                    if this.can_disconnect != next_can_disconnect {
                        this.can_disconnect = next_can_disconnect;
                        cx.notify();
                    }
                }
                TableEvent::ClearSelection => {
                    let next_can_disconnect = table.update(cx, |table, _| {
                        let d = table.delegate_mut();
                        d.selected_key = None;
                        d.last_viewport_fp = None;
                        d.can_disconnect_selected(None)
                    });
                    if this.can_disconnect != next_can_disconnect {
                        this.can_disconnect = next_can_disconnect;
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
            can_disconnect,
            last_locale,
            scroll_coalescer: ScrollCoalescer::default(),
            coalesce_task: None,
            _subscriptions: subscriptions,
        }
    }

    fn handle_snapshot_updated(&mut self, cx: &mut Context<Self>) {
        let (next_rev, next_slice, next_caps, next_ram) = {
            let snap = self.store.read(cx).snapshot();
            (
                snap.revisions.users,
                Arc::clone(&snap.users),
                snap.capabilities,
                snap.performance.latest.mem_total_bytes,
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
                let next_can_disconnect = self.table.update(cx, |table, cx| {
                    let prev_fp = table.delegate().last_viewport_fp;
                    let sort_changed = {
                        let d = table.delegate_mut();
                        let changed = d.slice.sort_column != next_slice.sort_column
                            || d.slice.sort_direction != next_slice.sort_direction;
                        d.slice = next_slice;
                        d.capabilities = next_caps;
                        d.total_ram_bytes = next_ram;
                        d.last_rev = next_rev;
                        d.user_action_pending = false;
                        d.row_text_cache.begin_epoch();
                        d.row_text_cache.prune_if_needed();
                        changed
                    };

                    Self::sync_table_selection(table, cx);

                    let selected_row = table.selected_row();
                    let next_fp = {
                        let d = table.delegate();
                        compute_users_viewport_fingerprint(
                            &d.slice,
                            d.total_ram_bytes,
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
                    table.delegate().can_disconnect_selected(selected_row)
                });
                if self.can_disconnect != next_can_disconnect {
                    self.can_disconnect = next_can_disconnect;
                    cx.notify();
                }
            }
        }
    }

    fn sync_table_selection(
        table: &mut TableState<UsersTableDelegate>,
        cx: &mut Context<TableState<UsersTableDelegate>>,
    ) {
        let hint_row = table.selected_row();
        let selected_idx = table
            .delegate()
            .selected_key
            .and_then(|key| table.delegate().find_row_index(key, hint_row));
        if let Some(idx) = selected_idx {
            if hint_row != Some(idx) {
                table.set_selected_row(idx, cx);
            }
        } else if hint_row.is_some() {
            table.clear_selection(cx);
        }
    }
}

impl Render for UsersView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        debug_assert_eq!(self._lease.domain(), MonitorDomain::Users);

        let theme = cx.theme();
        let border = theme.border;
        let fg = theme.foreground;
        let can_disconnect = self.can_disconnect;
        let store_manage = self.store.clone();

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
                    .child(i18n::tr("users.title")),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        Button::new("users-disconnect")
                            .small()
                            .danger()
                            .outline()
                            .icon(IconName::CircleX)
                            .label(i18n::tr("users.disconnect"))
                            .disabled(!can_disconnect)
                            .on_click(cx.listener(|this, _, window, cx| {
                                let t = this.table.read(cx);
                                if let Some((session_id, user_name)) =
                                    t.delegate().selected_session(t.selected_row())
                                {
                                    open_disconnect_session_dialog(
                                        &this.store,
                                        session_id,
                                        user_name,
                                        window,
                                        cx,
                                    );
                                }
                            })),
                    )
                    .child(
                        Button::new("users-manage-accounts")
                            .small()
                            .outline()
                            .icon(IconName::User)
                            .label(i18n::tr("users.manage_accounts"))
                            .on_click(move |_, _, cx| {
                                launch_manage_user_accounts(&store_manage, cx);
                            }),
                    ),
            );

        v_flex()
            .id("users-view-root")
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
        MonitorDomain, ProcessKey, ProcessStatusFlags, SortDirection, UserSessionProcess,
        UserSessionState, UsersRowProjection, UsersSlice, UsersSortColumn,
    };

    use super::*;

    fn sample_proc(pid: u32, name: &str, cpu_permille: u16, mem: u64) -> UserSessionProcess {
        UserSessionProcess {
            key: ProcessKey::new(pid, u64::from(pid) * 100),
            name: Arc::from(name),
            display_name: Arc::from(name),
            status: ProcessStatusFlags::empty(),
            cpu_permille,
            memory_bytes: mem,
            disk_bytes_per_sec: 0,
            net_bytes_per_sec: 0,
        }
    }

    #[test]
    fn users_viewport_fingerprint_tracks_visible_quantized_metrics_and_ignores_offscreen() {
        let mut slice = UsersSlice {
            revision: 1,
            sort_column: UsersSortColumn::Cpu,
            sort_direction: SortDirection::Descending,
            rows: vec![
                UsersRowProjection::Session {
                    session_id: 1,
                    user_name: Arc::from("Alice"),
                    state: UserSessionState::Active,
                    process_count: 2,
                    cpu_permille: 120,
                    memory_bytes: 128 * 1024 * 1024,
                    disk_bytes_per_sec: 0,
                    net_bytes_per_sec: 0,
                    has_children: true,
                    is_expanded: true,
                },
                UsersRowProjection::Process {
                    session_id: 1,
                    process: sample_proc(101, "code.exe", 100, 64 * 1024 * 1024),
                },
                UsersRowProjection::Process {
                    session_id: 1,
                    process: sample_proc(102, "notepad.exe", 20, 32 * 1024 * 1024),
                },
            ],
            ..UsersSlice::default()
        };

        let ram = 16 * 1024 * 1024 * 1024;
        let fp_initial = compute_users_viewport_fingerprint(&slice, ram, &(0..2), Some(0));

        // Sub-tenth memory jitter (4 KB on 128.0 MB) stays in the same quantized display bucket.
        if let UsersRowProjection::Session { memory_bytes, .. } = &mut slice.rows[0] {
            *memory_bytes += 4_096;
        }
        assert_eq!(
            fp_initial,
            compute_users_viewport_fingerprint(&slice, ram, &(0..2), Some(0))
        );

        // Off-screen row change (index 2) leaves visible fingerprint unchanged.
        if let UsersRowProjection::Process { process, .. } = &mut slice.rows[2] {
            process.cpu_permille = 450;
        }
        assert_eq!(
            fp_initial,
            compute_users_viewport_fingerprint(&slice, ram, &(0..2), Some(0))
        );

        // Visible CPU change (index 1) invalidates the fingerprint.
        if let UsersRowProjection::Process { process, .. } = &mut slice.rows[1] {
            process.cpu_permille = 250;
        }
        assert_ne!(
            fp_initial,
            compute_users_viewport_fingerprint(&slice, ram, &(0..2), Some(0))
        );
    }

    #[test]
    fn users_scroll_coalescer_enforces_quiet_window_cap_and_bypass() {
        let mut coalescer = ScrollCoalescer::default();
        let t0 = Instant::now();
        assert_eq!(
            coalescer.decide(t0 + Duration::from_millis(20), Some(t0), false),
            CoalesceDecision::Defer(Duration::from_millis(130))
        );
        assert_eq!(
            coalescer.decide(t0 + Duration::from_millis(150), Some(t0), false),
            CoalesceDecision::ApplyNow
        );
        // Bypass immediately flushes.
        assert_eq!(
            coalescer.decide(t0 + Duration::from_millis(10), Some(t0), true),
            CoalesceDecision::ApplyNow
        );
    }

    #[gpui_kit::test]
    fn users_view_acquires_lease_dispatches_sort_and_expand_and_releases_on_drop(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().expect("tempdir");
        let config_store = ConfigStore::new(dir.path().join("settings.toml"));
        let settings = AppSettings::default();
        let backend = NativeBackend::initialize().expect("initialize native backend");
        let coordinator =
            Arc::new(EngineCoordinator::new(backend, settings.clone()).expect("start coordinator"));

        assert!(!coordinator.leases().is_active(MonitorDomain::Users));

        let store = cx.new(|cx| UiStore::new(coordinator.clone(), config_store, settings, cx));
        let shell_handle = ShellHandle::new(|_, _, _| {});

        let mut view_handle = None;
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                let view =
                    cx.new(|cx| UsersView::new(store.clone(), shell_handle.clone(), window, cx));
                view_handle = Some(view.clone());
                cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
            })
            .expect("open users test window")
        });
        let view = view_handle.expect("UsersView constructed");

        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.update(|window, cx| window.draw(cx).clear(cx));

        assert!(
            coordinator.leases().is_active(MonitorDomain::Users),
            "mounting UsersView must acquire MonitorDomain::Users lease"
        );

        view.update(&mut vcx, |view, cx| {
            view.table.update(cx, |table, cx| {
                table.delegate_mut().trigger_sort(3, cx);
                table.delegate_mut().trigger_toggle_session(1, cx);
            });
        });
        vcx.run_until_parked();

        drop(view);
        vcx.update(|window, _| window.remove_window());
        vcx.run_until_parked();

        assert!(
            !coordinator.leases().is_active(MonitorDomain::Users),
            "dropping UsersView must release MonitorDomain::Users lease"
        );

        coordinator.shutdown();
    }
}
