//! Column metadata, selection anchoring, scroll coalescing, locale caching,
//! and viewport fingerprinting for the Users feature slice.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};
use std::ops::Range;
use std::time::{Duration, Instant};

use gpui_kit::SharedString;
use gpui_kit::component::table::ColumnSort;
use loom_types::{
    ProcessKey, ProcessStatusFlags, SortDirection, UserSessionState, UsersRowProjection,
    UsersSlice, UsersSortColumn,
};
use loom_ui_kit::{
    HeatmapLevel, quantize_bytes_display, quantize_net_bps_display, quantize_rate_bps_display,
};

use crate::i18n;

pub(super) const USERS_HEADER_HEIGHT: f32 = 32.0;
const SCROLL_COALESCE_QUIET_WINDOW: Duration = Duration::from_millis(150);
const SCROLL_COALESCE_MAX_DEFER: Duration = Duration::from_millis(500);
const DEFAULT_FINGERPRINT_WINDOW: usize = 64;

/// Sentinel creation timestamp used to cache top-level session aggregate metrics inside
/// [`loom_ui_kit::RowTextCache`] without colliding with any real OS [`ProcessKey`].
pub(super) const SESSION_CACHE_CREATE_TIME_SENTINEL: u64 = u64::MAX;

/// Fallback total physical memory (16 GiB) used only before the first performance sample arrives.
const FALLBACK_TOTAL_RAM_BYTES: u64 = 16 * 1024 * 1024 * 1024;

pub(super) const USERS_COLUMNS: [UsersSortColumn; 6] = [
    UsersSortColumn::User,
    UsersSortColumn::Status,
    UsersSortColumn::Cpu,
    UsersSortColumn::Memory,
    UsersSortColumn::Disk,
    UsersSortColumn::Network,
];

#[inline]
pub(super) const fn column_slug(col: UsersSortColumn) -> &'static str {
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
pub(super) const fn column_width(col: UsersSortColumn) -> f32 {
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
pub(super) const fn column_min_width(col: UsersSortColumn) -> f32 {
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
pub(super) const fn column_right_aligned(col: UsersSortColumn) -> bool {
    matches!(
        col,
        UsersSortColumn::Cpu
            | UsersSortColumn::Memory
            | UsersSortColumn::Disk
            | UsersSortColumn::Network
    )
}

#[inline]
pub(super) const fn column_is_heatmap(col: UsersSortColumn) -> bool {
    matches!(
        col,
        UsersSortColumn::Cpu
            | UsersSortColumn::Memory
            | UsersSortColumn::Disk
            | UsersSortColumn::Network
    )
}

#[inline]
pub(super) const fn column_default_direction(col: UsersSortColumn) -> SortDirection {
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
pub(super) const fn to_column_sort(dir: SortDirection) -> ColumnSort {
    match dir {
        SortDirection::Ascending => ColumnSort::Ascending,
        SortDirection::Descending => ColumnSort::Descending,
    }
}

#[inline]
pub(super) fn memory_heatmap_level(bytes: u64, total_ram_bytes: u64) -> HeatmapLevel {
    let denom = if total_ram_bytes > 0 {
        total_ram_bytes
    } else {
        FALLBACK_TOTAL_RAM_BYTES
    };
    let permille = ((u128::from(bytes) * 1_000) / u128::from(denom)).min(1_000) as u16;
    HeatmapLevel::from_permille(permille)
}

#[inline]
pub(super) fn is_critical_or_system_process(status: ProcessStatusFlags) -> bool {
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
pub(super) fn row_key(row: &UsersRowProjection) -> UsersRowKey {
    match row {
        UsersRowProjection::Session { session_id, .. } => UsersRowKey::Session(*session_id),
        UsersRowProjection::Process { process, .. } => UsersRowKey::Process(process.key),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CoalesceDecision {
    ApplyNow,
    Defer(Duration),
}

#[derive(Debug, Default)]
pub(super) struct ScrollCoalescer {
    deferred_since: Option<Instant>,
}

impl ScrollCoalescer {
    pub(super) fn decide(
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

/// Cached localized strings for [`super::table::UsersTableDelegate`].
pub(super) struct UsersLocaleCache {
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
    pub(super) running: SharedString,
    pub(super) suspended: SharedString,
    pub(super) loading: SharedString,
    pub(super) empty: SharedString,
}

impl UsersLocaleCache {
    pub(super) fn build() -> Self {
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
    pub(super) fn column_label(&self, col: UsersSortColumn) -> SharedString {
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
    pub(super) fn state_label(&self, state: UserSessionState) -> SharedString {
        match state {
            UserSessionState::Active => self.active.clone(),
            UserSessionState::Disconnected => self.disconnected.clone(),
            UserSessionState::Idle => self.idle.clone(),
            UserSessionState::Unknown => self.unknown.clone(),
        }
    }
}
