//! On-demand Users vertical feature slice gated by an RAII [`ScopedMonitorLease`].
//!
//! Splits the static toolbar ([`UsersView`]) from the expandable session-tree
//! 2D-virtualized `gpui-kit` `DataTable` (`Entity<TableState<UsersTableDelegate>>`)
//! so hidden routes incur zero WTS session polling or per-session aggregation overhead.

use std::cmp::Ordering;
use std::collections::HashSet;
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
    IntoElement, MouseButton, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};
use loom_types::{
    CapabilityFlags, LanguagePreference, MonitorDomain, ProcessKey, ProcessStatusFlags,
    RunTaskRequest, SortDirection, UserSessionProcess, UserSessionRecord, UserSessionState,
    UsersSlice,
};
use loom_ui_kit::{
    HeatmapLevel, LoomSemanticColors, format_bytes_shared, format_network_rate_mbps_shared,
    format_permille_shared, format_rate_bytes_per_sec_shared, heatmap_cell,
};

use crate::bridge::{ScopedMonitorLease, UiCommands, UiStore, UiStoreEvent};
use crate::features::table_columns::DataTablePane;
use crate::i18n;
use crate::shell::ShellHandle;

const USERS_HEADER_HEIGHT: f32 = 32.0;

/// Fallback total physical memory (16 GiB) used only before the first performance sample arrives.
const FALLBACK_TOTAL_RAM_BYTES: u64 = 16 * 1024 * 1024 * 1024;

/// Sortable columns displayed by [`UsersView`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UsersSortColumn {
    User,
    Status,
    Cpu,
    Memory,
    Disk,
    Network,
}

impl UsersSortColumn {
    const ALL: [Self; 6] = [
        Self::User,
        Self::Status,
        Self::Cpu,
        Self::Memory,
        Self::Disk,
        Self::Network,
    ];

    #[inline]
    const fn slug(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Status => "status",
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::Disk => "disk",
            Self::Network => "network",
        }
    }

    #[inline]
    const fn width(self) -> f32 {
        match self {
            Self::User => 280.0,
            Self::Status => 120.0,
            Self::Cpu => 92.0,
            Self::Memory => 128.0,
            Self::Disk => 116.0,
            Self::Network => 116.0,
        }
    }

    #[inline]
    const fn min_width(self) -> f32 {
        match self {
            Self::User => 140.0,
            Self::Status => 84.0,
            Self::Cpu => 68.0,
            Self::Memory => 88.0,
            Self::Disk => 88.0,
            Self::Network => 88.0,
        }
    }

    #[inline]
    const fn right_aligned(self) -> bool {
        matches!(self, Self::Cpu | Self::Memory | Self::Disk | Self::Network)
    }

    #[inline]
    const fn is_heatmap(self) -> bool {
        matches!(self, Self::Cpu | Self::Memory | Self::Disk | Self::Network)
    }

    #[inline]
    const fn default_direction(self) -> SortDirection {
        match self {
            Self::Cpu | Self::Memory | Self::Disk | Self::Network | Self::Status => {
                SortDirection::Descending
            }
            Self::User => SortDirection::Ascending,
        }
    }

    fn label(self) -> String {
        match self {
            Self::User => i18n::tr("users.col_user"),
            Self::Status => i18n::tr("users.col_status"),
            Self::Cpu => i18n::tr("users.col_cpu"),
            Self::Memory => i18n::tr("users.col_memory"),
            Self::Disk => i18n::tr("users.col_disk"),
            Self::Network => i18n::tr("users.col_network"),
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
const fn session_state_rank(state: UserSessionState) -> u8 {
    match state {
        UserSessionState::Active => 3,
        UserSessionState::Idle => 2,
        UserSessionState::Disconnected => 1,
        UserSessionState::Unknown => 0,
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

/// Stable selection key anchoring rows across live refreshes and tree expansion toggles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum UsersRowKey {
    Session(u32),
    Process(ProcessKey),
}

/// Flattened row projection rendered by [`UsersTableDelegate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum UsersRow {
    Session {
        session_id: u32,
        user_name: Arc<str>,
        state: UserSessionState,
        process_count: u32,
        cpu_permille: u16,
        memory_bytes: u64,
        disk_bytes_per_sec: u64,
        net_bytes_per_sec: u64,
        has_children: bool,
        is_expanded: bool,
    },
    Process {
        session_id: u32,
        process: UserSessionProcess,
    },
}

impl UsersRow {
    #[inline]
    pub(super) fn key(&self) -> UsersRowKey {
        match self {
            Self::Session { session_id, .. } => UsersRowKey::Session(*session_id),
            Self::Process { process, .. } => UsersRowKey::Process(process.key),
        }
    }
}

fn matches_session_header(session: &UserSessionRecord, query_lower: &str) -> bool {
    contains_icase(&session.user_name, query_lower)
        || session.session_id.to_string().contains(query_lower)
}

fn matches_session_process(proc_item: &UserSessionProcess, query_lower: &str) -> bool {
    contains_icase(&proc_item.display_name, query_lower)
        || contains_icase(&proc_item.name, query_lower)
        || proc_item.key.pid.to_string().contains(query_lower)
}

fn compare_sessions(
    a: &UserSessionRecord,
    b: &UserSessionRecord,
    sort_col: UsersSortColumn,
    sort_dir: SortDirection,
) -> Ordering {
    let ord = match sort_col {
        UsersSortColumn::User => cmp_icase(&a.user_name, &b.user_name),
        UsersSortColumn::Status => session_state_rank(a.state).cmp(&session_state_rank(b.state)),
        UsersSortColumn::Cpu => a.cpu_permille.cmp(&b.cpu_permille),
        UsersSortColumn::Memory => a.memory_bytes.cmp(&b.memory_bytes),
        UsersSortColumn::Disk => a.disk_bytes_per_sec.cmp(&b.disk_bytes_per_sec),
        UsersSortColumn::Network => a.net_bytes_per_sec.cmp(&b.net_bytes_per_sec),
    };
    let ordered = match sort_dir {
        SortDirection::Ascending => ord,
        SortDirection::Descending => ord.reverse(),
    };
    ordered.then_with(|| a.session_id.cmp(&b.session_id))
}

fn compare_child_processes(
    a: &UserSessionProcess,
    b: &UserSessionProcess,
    sort_col: UsersSortColumn,
    sort_dir: SortDirection,
) -> Ordering {
    let ord = match sort_col {
        UsersSortColumn::User => cmp_icase(&a.display_name, &b.display_name),
        UsersSortColumn::Status => {
            let a_running = !a.status.contains(ProcessStatusFlags::SUSPENDED);
            let b_running = !b.status.contains(ProcessStatusFlags::SUSPENDED);
            a_running.cmp(&b_running)
        }
        UsersSortColumn::Cpu => a.cpu_permille.cmp(&b.cpu_permille),
        UsersSortColumn::Memory => a.memory_bytes.cmp(&b.memory_bytes),
        UsersSortColumn::Disk => a.disk_bytes_per_sec.cmp(&b.disk_bytes_per_sec),
        UsersSortColumn::Network => a.net_bytes_per_sec.cmp(&b.net_bytes_per_sec),
    };
    let ordered = match sort_dir {
        SortDirection::Ascending => ord,
        SortDirection::Descending => ord.reverse(),
    };
    ordered
        .then_with(|| cmp_icase(&a.display_name, &b.display_name))
        .then_with(|| a.key.cmp(&b.key))
}

/// Flattens `sessions` into expandable session headers and child process rows.
///
/// When `search_query` is non-empty, sessions with matching child processes are
/// automatically expanded to reveal those matching children.
pub(super) fn flatten_users_rows(
    sessions: &[UserSessionRecord],
    expanded_sessions: &HashSet<u32>,
    search_query: &str,
    sort_col: UsersSortColumn,
    sort_dir: SortDirection,
) -> Vec<UsersRow> {
    let query_lower = search_query.trim().to_lowercase();
    let mut session_refs: Vec<&UserSessionRecord> = sessions.iter().collect();
    session_refs.sort_by(|a, b| compare_sessions(a, b, sort_col, sort_dir));

    let mut rows = Vec::with_capacity(sessions.len() * 2);

    for session in session_refs {
        if query_lower.is_empty() {
            let has_children = !session.processes.is_empty();
            let is_expanded = has_children && expanded_sessions.contains(&session.session_id);
            rows.push(UsersRow::Session {
                session_id: session.session_id,
                user_name: Arc::clone(&session.user_name),
                state: session.state,
                process_count: session.process_count,
                cpu_permille: session.cpu_permille,
                memory_bytes: session.memory_bytes,
                disk_bytes_per_sec: session.disk_bytes_per_sec,
                net_bytes_per_sec: session.net_bytes_per_sec,
                has_children,
                is_expanded,
            });
            if is_expanded {
                let mut children: Vec<&UserSessionProcess> = session.processes.iter().collect();
                children.sort_by(|a, b| compare_child_processes(a, b, sort_col, sort_dir));
                for proc_item in children {
                    rows.push(UsersRow::Process {
                        session_id: session.session_id,
                        process: proc_item.clone(),
                    });
                }
            }
        } else {
            let header_matches = matches_session_header(session, &query_lower);
            let mut matching_children: Vec<&UserSessionProcess> = session
                .processes
                .iter()
                .filter(|p| matches_session_process(p, &query_lower))
                .collect();

            if !header_matches && matching_children.is_empty() {
                continue;
            }

            let has_children = !session.processes.is_empty();
            let auto_expand = !matching_children.is_empty();
            let is_expanded =
                auto_expand || (has_children && expanded_sessions.contains(&session.session_id));

            rows.push(UsersRow::Session {
                session_id: session.session_id,
                user_name: Arc::clone(&session.user_name),
                state: session.state,
                process_count: session.process_count,
                cpu_permille: session.cpu_permille,
                memory_bytes: session.memory_bytes,
                disk_bytes_per_sec: session.disk_bytes_per_sec,
                net_bytes_per_sec: session.net_bytes_per_sec,
                has_children,
                is_expanded,
            });

            if auto_expand {
                matching_children.sort_by(|a, b| compare_child_processes(a, b, sort_col, sort_dir));
                for proc_item in matching_children {
                    rows.push(UsersRow::Process {
                        session_id: session.session_id,
                        process: proc_item.clone(),
                    });
                }
            } else if is_expanded {
                let mut all_children: Vec<&UserSessionProcess> = session.processes.iter().collect();
                all_children.sort_by(|a, b| compare_child_processes(a, b, sort_col, sort_dir));
                for proc_item in all_children {
                    rows.push(UsersRow::Process {
                        session_id: session.session_id,
                        process: proc_item.clone(),
                    });
                }
            }
        }
    }

    rows
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
    search_query: String,
    sort_col: UsersSortColumn,
    sort_dir: SortDirection,
    expanded_sessions: HashSet<u32>,
    rows: Vec<UsersRow>,
    selected_key: Option<UsersRowKey>,
    locale: UsersLocaleCache,
}

impl UsersTableDelegate {
    fn new(store: Entity<UiStore>, shell_handle: ShellHandle, cx: &App) -> Self {
        let store_ref = store.read(cx);
        let snap = store_ref.snapshot();
        let slice = Arc::clone(&snap.users);
        let capabilities = snap.capabilities;
        let total_ram_bytes = snap.performance.latest.mem_total_bytes;
        let search_query = store_ref.search_query().to_owned();
        let sort_col = UsersSortColumn::User;
        let sort_dir = SortDirection::Ascending;
        let expanded_sessions = HashSet::new();
        let rows = flatten_users_rows(
            &slice.sessions,
            &expanded_sessions,
            &search_query,
            sort_col,
            sort_dir,
        );

        Self {
            store: store.clone(),
            shell_handle,
            last_rev: snap.revisions.users,
            slice,
            capabilities,
            total_ram_bytes,
            search_query,
            sort_col,
            sort_dir,
            expanded_sessions,
            rows,
            selected_key: None,
            locale: UsersLocaleCache::build(),
        }
    }

    fn rebuild_rows(&mut self) {
        self.rows = flatten_users_rows(
            &self.slice.sessions,
            &self.expanded_sessions,
            &self.search_query,
            self.sort_col,
            self.sort_dir,
        );
    }

    fn toggle_session_expanded(&mut self, session_id: u32) {
        if !self.expanded_sessions.remove(&session_id) {
            self.expanded_sessions.insert(session_id);
        }
        self.rebuild_rows();
    }

    fn find_row_index(&self, key: UsersRowKey) -> Option<usize> {
        self.rows.iter().position(|row| row.key() == key)
    }

    fn selected_session(&self) -> Option<(u32, String)> {
        let sel = self.selected_key?;
        let row_ix = self.find_row_index(sel)?;
        match self.rows.get(row_ix)? {
            UsersRow::Session {
                session_id,
                user_name,
                ..
            } => Some((*session_id, user_name.to_string())),
            UsersRow::Process { .. } => None,
        }
    }

    fn can_disconnect_selected(&self) -> bool {
        self.capabilities.contains(CapabilityFlags::USER_SESSIONS)
            && self.selected_session().is_some()
    }

    fn toggle_sort(&mut self, col_ix: usize) {
        let Some(&target) = UsersSortColumn::ALL.get(col_ix) else {
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

impl TableDelegate for UsersTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        UsersSortColumn::ALL.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.rows.len()
    }

    fn loading(&self, _cx: &App) -> bool {
        self.slice.revision == 0
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        let Some(&col) = UsersSortColumn::ALL.get(col_ix) else {
            return Column::default();
        };
        let mut column = Column::new(col.slug(), col.label())
            .width(px(col.width()))
            .min_width(px(col.min_width()))
            .selectable(false)
            .movable(false);

        if col == UsersSortColumn::User {
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
        let Some(&col) = UsersSortColumn::ALL.get(col_ix) else {
            return Empty.into_any_element();
        };
        let is_active = col == self.sort_col;
        let fg = cx.theme().foreground;
        let muted_fg = cx.theme().muted_foreground;

        h_flex()
            .id(("users-th", col_ix))
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
                    .selected_key
                    .and_then(|key| table.delegate().find_row_index(key));
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
        div().id(("users-row", row_ix)).text_xs()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(row) = self.rows.get(row_ix) else {
            return Empty.into_any_element();
        };
        let Some(&col) = UsersSortColumn::ALL.get(col_ix) else {
            return Empty.into_any_element();
        };

        let theme = cx.theme();
        let colors = LoomSemanticColors::from_theme(theme);
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let primary = theme.primary;

        match row {
            UsersRow::Session {
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
                match col {
                    UsersSortColumn::User => {
                        let label = format!("{} ({})", user_name, process_count);
                        h_flex()
                            .size_full()
                            .items_center()
                            .gap_1p5()
                            .overflow_hidden()
                            .child(if *has_children {
                                div()
                                    .flex_shrink_0()
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation();
                                    })
                                    .child(
                                        Button::new(("users-expand", sid as usize))
                                            .xsmall()
                                            .ghost()
                                            .icon(if *is_expanded {
                                                IconName::ChevronDown
                                            } else {
                                                IconName::ChevronRight
                                            })
                                            .on_click(cx.listener(move |table, _, _, cx| {
                                                table.delegate_mut().toggle_session_expanded(sid);
                                                let sel =
                                                    table.delegate().selected_key.and_then(|key| {
                                                        table.delegate().find_row_index(key)
                                                    });
                                                if let Some(idx) = sel {
                                                    table.set_selected_row(idx, cx);
                                                } else {
                                                    table.clear_selection(cx);
                                                }
                                                cx.notify();
                                            })),
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
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .font_medium()
                                    .text_color(fg)
                                    .child(label),
                            )
                            .into_any_element()
                    }
                    UsersSortColumn::Status => {
                        let color = if *state == UserSessionState::Active {
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
                                    .child(self.locale.state_label(*state)),
                            )
                            .into_any_element()
                    }
                    UsersSortColumn::Cpu => heatmap_cell(
                        format_permille_shared(*cpu_permille),
                        HeatmapLevel::from_permille(*cpu_permille),
                        &colors,
                    )
                    .into_any_element(),
                    UsersSortColumn::Memory => heatmap_cell(
                        format_bytes_shared(*memory_bytes),
                        memory_heatmap_level(*memory_bytes, self.total_ram_bytes),
                        &colors,
                    )
                    .into_any_element(),
                    UsersSortColumn::Disk => heatmap_cell(
                        format_rate_bytes_per_sec_shared(*disk_bytes_per_sec),
                        HeatmapLevel::from_bytes_per_sec(*disk_bytes_per_sec),
                        &colors,
                    )
                    .into_any_element(),
                    UsersSortColumn::Network => heatmap_cell(
                        format_network_rate_mbps_shared(*net_bytes_per_sec),
                        HeatmapLevel::from_bytes_per_sec(*net_bytes_per_sec),
                        &colors,
                    )
                    .into_any_element(),
                }
            }
            UsersRow::Process { process, .. } => match col {
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
                    let text = if process.status.contains(ProcessStatusFlags::SUSPENDED) {
                        self.locale.suspended.clone()
                    } else {
                        self.locale.running.clone()
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
                                .text_color(muted_fg)
                                .child(text),
                        )
                        .into_any_element()
                }
                UsersSortColumn::Cpu => heatmap_cell(
                    format_permille_shared(process.cpu_permille),
                    HeatmapLevel::from_permille(process.cpu_permille),
                    &colors,
                )
                .into_any_element(),
                UsersSortColumn::Memory => heatmap_cell(
                    format_bytes_shared(process.memory_bytes),
                    memory_heatmap_level(process.memory_bytes, self.total_ram_bytes),
                    &colors,
                )
                .into_any_element(),
                UsersSortColumn::Disk => heatmap_cell(
                    format_rate_bytes_per_sec_shared(process.disk_bytes_per_sec),
                    HeatmapLevel::from_bytes_per_sec(process.disk_bytes_per_sec),
                    &colors,
                )
                .into_any_element(),
                UsersSortColumn::Network => heatmap_cell(
                    format_network_rate_mbps_shared(process.net_bytes_per_sec),
                    HeatmapLevel::from_bytes_per_sec(process.net_bytes_per_sec),
                    &colors,
                )
                .into_any_element(),
            },
        }
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(row) = self.rows.get(row_ix).cloned() else {
            return menu;
        };

        match row {
            UsersRow::Session {
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
                                    table.delegate_mut().toggle_session_expanded(session_id);
                                    cx.notify();
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
            UsersRow::Process { process, .. } => {
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
        match self.rows.get(row_ix)? {
            UsersRow::Session {
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
            UsersRow::Process { process, .. } => {
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
        let can_disconnect = table.read(cx).delegate().can_disconnect_selected();

        let mut subscriptions = Vec::with_capacity(2);

        let table_for_store = table.clone();
        subscriptions.push(
            cx.subscribe(&store, move |this, store, event, cx| match event {
                UiStoreEvent::SnapshotUpdated(revs) => {
                    let current_rev = table_for_store.read(cx).delegate().last_rev;
                    if revs.users == current_rev {
                        return;
                    }
                    let (next_slice, next_caps, next_ram) = {
                        let snap = store.read(cx).snapshot();
                        (
                            Arc::clone(&snap.users),
                            snap.capabilities,
                            snap.performance.latest.mem_total_bytes,
                        )
                    };
                    let next_can_disconnect = table_for_store.update(cx, |table, cx| {
                        let d = table.delegate_mut();
                        d.slice = next_slice;
                        d.capabilities = next_caps;
                        d.total_ram_bytes = next_ram;
                        d.last_rev = revs.users;
                        d.rebuild_rows();
                        Self::sync_table_selection(table, cx);
                        cx.notify();
                        table.delegate().can_disconnect_selected()
                    });
                    if this.can_disconnect != next_can_disconnect {
                        this.can_disconnect = next_can_disconnect;
                        cx.notify();
                    }
                }
                UiStoreEvent::SearchQueryChanged => {
                    let next_query = store.read(cx).search_query().to_owned();
                    let next_can_disconnect = table_for_store.update(cx, |table, cx| {
                        let d = table.delegate_mut();
                        d.search_query = next_query;
                        d.rebuild_rows();
                        Self::sync_table_selection(table, cx);
                        cx.notify();
                        table.delegate().can_disconnect_selected()
                    });
                    if this.can_disconnect != next_can_disconnect {
                        this.can_disconnect = next_can_disconnect;
                        cx.notify();
                    }
                }
                UiStoreEvent::SettingsChanged => {
                    let next_locale = store.read(cx).settings().language;
                    if this.last_locale != next_locale {
                        this.last_locale = next_locale;
                        table_for_store.update(cx, |table, cx| {
                            table.delegate_mut().locale = UsersLocaleCache::build();
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
                        let key = table.delegate().rows.get(*row_ix).map(UsersRow::key);
                        table.delegate_mut().selected_key = key;
                        table.delegate().can_disconnect_selected()
                    });
                    if this.can_disconnect != next_can_disconnect {
                        this.can_disconnect = next_can_disconnect;
                        cx.notify();
                    }
                }
                TableEvent::RightClickedRow(Some(row_ix)) => {
                    let next_can_disconnect = table.update(cx, |table, cx| {
                        let key = table.delegate().rows.get(*row_ix).map(UsersRow::key);
                        table.delegate_mut().selected_key = key;
                        if key.is_some() && table.selected_row() != Some(*row_ix) {
                            table.set_selected_row(*row_ix, cx);
                        }
                        table.delegate().can_disconnect_selected()
                    });
                    if this.can_disconnect != next_can_disconnect {
                        this.can_disconnect = next_can_disconnect;
                        cx.notify();
                    }
                }
                TableEvent::ClearSelection => {
                    let next_can_disconnect = table.update(cx, |table, _| {
                        table.delegate_mut().selected_key = None;
                        table.delegate().can_disconnect_selected()
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
            _subscriptions: subscriptions,
        }
    }

    fn sync_table_selection(
        table: &mut TableState<UsersTableDelegate>,
        cx: &mut Context<TableState<UsersTableDelegate>>,
    ) {
        let selected_idx = table
            .delegate()
            .selected_key
            .and_then(|key| table.delegate().find_row_index(key));
        if let Some(idx) = selected_idx {
            if table.selected_row() != Some(idx) {
                table.set_selected_row(idx, cx);
            }
        } else if table.selected_row().is_some() {
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
                                if let Some((session_id, user_name)) =
                                    this.table.read(cx).delegate().selected_session()
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
    use std::collections::HashSet;
    use std::sync::Arc;

    use gpui_kit::{TestAppContext, VisualTestContext};
    use loom_config::{AppSettings, ConfigStore};
    use loom_engine::EngineCoordinator;
    use loom_pal::{NativeBackend, PlatformBackend};
    use loom_types::{
        MonitorDomain, ProcessKey, ProcessStatusFlags, SortDirection, UserSessionProcess,
        UserSessionRecord, UserSessionState,
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
    fn flatten_users_rows_expands_sessions_and_auto_expands_on_child_search() {
        let sessions = vec![
            UserSessionRecord {
                session_id: 1,
                user_name: Arc::from("Alice"),
                state: UserSessionState::Active,
                process_count: 2,
                cpu_permille: 120,
                memory_bytes: 512 * 1024 * 1024,
                disk_bytes_per_sec: 1024,
                net_bytes_per_sec: 2048,
                processes: vec![
                    sample_proc(101, "code.exe", 100, 400 * 1024 * 1024),
                    sample_proc(102, "notepad.exe", 20, 112 * 1024 * 1024),
                ],
            },
            UserSessionRecord {
                session_id: 2,
                user_name: Arc::from("Bob"),
                state: UserSessionState::Disconnected,
                process_count: 1,
                cpu_permille: 10,
                memory_bytes: 64 * 1024 * 1024,
                disk_bytes_per_sec: 0,
                net_bytes_per_sec: 0,
                processes: vec![sample_proc(201, "cmd.exe", 10, 64 * 1024 * 1024)],
            },
        ];

        // Collapsed by default: only 2 session headers.
        let collapsed = flatten_users_rows(
            &sessions,
            &HashSet::new(),
            "",
            UsersSortColumn::User,
            SortDirection::Ascending,
        );
        assert_eq!(collapsed.len(), 2);
        assert!(matches!(
            collapsed[0],
            UsersRow::Session {
                session_id: 1,
                is_expanded: false,
                ..
            }
        ));

        // Explicitly expanding session 1 reveals both children sorted by CPU desc.
        let mut expanded = HashSet::new();
        expanded.insert(1);
        let with_children = flatten_users_rows(
            &sessions,
            &expanded,
            "",
            UsersSortColumn::Cpu,
            SortDirection::Descending,
        );
        assert_eq!(with_children.len(), 4);
        assert!(matches!(
            with_children[0],
            UsersRow::Session {
                session_id: 1,
                is_expanded: true,
                ..
            }
        ));
        assert!(matches!(
            &with_children[1],
            UsersRow::Process { process, .. } if process.name.as_ref() == "code.exe"
        ));
        assert!(matches!(
            &with_children[2],
            UsersRow::Process { process, .. } if process.name.as_ref() == "notepad.exe"
        ));
        assert!(matches!(
            with_children[3],
            UsersRow::Session { session_id: 2, .. }
        ));

        // Searching for a child process auto-expands its owning session even if collapsed.
        let search_child = flatten_users_rows(
            &sessions,
            &HashSet::new(),
            "notepad",
            UsersSortColumn::User,
            SortDirection::Ascending,
        );
        assert_eq!(search_child.len(), 2);
        assert!(matches!(
            search_child[0],
            UsersRow::Session {
                session_id: 1,
                is_expanded: true,
                ..
            }
        ));
        assert!(matches!(
            &search_child[1],
            UsersRow::Process { process, .. } if process.key.pid == 102
        ));
    }

    #[gpui_kit::test]
    fn users_view_acquires_and_releases_monitor_lease_on_mount_and_drop(cx: &mut TestAppContext) {
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
