//! Column metadata, service action guards, scroll coalescing, locale caching,
//! and viewport fingerprinting for the Services feature slice.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::SharedString;
use gpui_kit::component::table::ColumnSort;
use loom_types::{
    AppSnapshot, ProcessKey, ProcessRowKind, ServiceRecord, ServiceState, ServicesSlice,
    ServicesSortColumn, SortDirection,
};
use loom_ui_kit::UNAVAILABLE_LABEL;

use crate::i18n;

pub(super) const SERVICES_HEADER_HEIGHT: f32 = 32.0;
const SCROLL_COALESCE_QUIET_WINDOW: Duration = Duration::from_millis(150);
const SCROLL_COALESCE_MAX_DEFER: Duration = Duration::from_millis(500);
pub(super) const PID_TEXT_CACHE_LIMIT: usize = 1024;
const DEFAULT_FINGERPRINT_WINDOW: usize = 64;

pub(super) const SERVICES_COLUMNS: [ServicesSortColumn; 5] = [
    ServicesSortColumn::Name,
    ServicesSortColumn::Pid,
    ServicesSortColumn::Description,
    ServicesSortColumn::Status,
    ServicesSortColumn::Group,
];

#[inline]
pub(super) const fn column_slug(col: ServicesSortColumn) -> &'static str {
    match col {
        ServicesSortColumn::Name => "name",
        ServicesSortColumn::Pid => "pid",
        ServicesSortColumn::Description => "description",
        ServicesSortColumn::Status => "status",
        ServicesSortColumn::Group => "group",
    }
}

#[inline]
pub(super) const fn column_width(col: ServicesSortColumn) -> f32 {
    match col {
        ServicesSortColumn::Name => 200.0,
        ServicesSortColumn::Pid => 84.0,
        ServicesSortColumn::Description => 340.0,
        ServicesSortColumn::Status => 120.0,
        ServicesSortColumn::Group => 180.0,
    }
}

#[inline]
pub(super) const fn column_min_width(col: ServicesSortColumn) -> f32 {
    match col {
        ServicesSortColumn::Name => 120.0,
        ServicesSortColumn::Pid => 64.0,
        ServicesSortColumn::Description => 140.0,
        ServicesSortColumn::Status => 84.0,
        ServicesSortColumn::Group => 96.0,
    }
}

#[inline]
pub(super) const fn column_right_aligned(col: ServicesSortColumn) -> bool {
    matches!(col, ServicesSortColumn::Pid)
}

#[inline]
pub(super) const fn column_default_direction(col: ServicesSortColumn) -> SortDirection {
    match col {
        ServicesSortColumn::Status => SortDirection::Descending,
        ServicesSortColumn::Name
        | ServicesSortColumn::Pid
        | ServicesSortColumn::Description
        | ServicesSortColumn::Group => SortDirection::Ascending,
    }
}

#[inline]
pub(super) const fn to_column_sort(dir: SortDirection) -> ColumnSort {
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
pub(super) enum CoalesceDecision {
    ApplyNow,
    Defer(Duration),
}

/// Coalesces periodic snapshot ticks during active scrolling to keep GPU row layers
/// on their fast composite path, while enforcing a bounded maximum defer cap.
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

    pub(super) fn reset(&mut self) {
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
pub(super) struct ServicesToolbarState {
    pub(super) can_start: bool,
    pub(super) can_stop: bool,
    pub(super) can_restart: bool,
}

/// Cached localized strings for [`super::table::ServicesTableDelegate`].
pub(super) struct ServicesLocaleCache {
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
    pub(super) loading: SharedString,
    pub(super) empty: SharedString,
}

impl ServicesLocaleCache {
    pub(super) fn build() -> Self {
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
    pub(super) fn column_label(&self, col: ServicesSortColumn) -> SharedString {
        match col {
            ServicesSortColumn::Name => self.col_name.clone(),
            ServicesSortColumn::Pid => self.col_pid.clone(),
            ServicesSortColumn::Description => self.col_description.clone(),
            ServicesSortColumn::Status => self.col_status.clone(),
            ServicesSortColumn::Group => self.col_group.clone(),
        }
    }

    #[inline]
    pub(super) fn state_label(&self, state: ServiceState) -> SharedString {
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
