//! Column metadata, BIOS time formatting, scroll coalescing, locale caching,
//! and viewport fingerprinting for the Startup Apps feature slice.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};
use std::ops::Range;
use std::time::{Duration, Instant};

use gpui_kit::SharedString;
use gpui_kit::component::table::ColumnSort;
use loom_types::{SortDirection, StartupImpact, StartupSlice, StartupSortColumn};

use crate::i18n;

pub(super) const STARTUP_HEADER_HEIGHT: f32 = 32.0;
const SCROLL_COALESCE_QUIET_WINDOW: Duration = Duration::from_millis(150);
const SCROLL_COALESCE_MAX_DEFER: Duration = Duration::from_millis(500);
const DEFAULT_FINGERPRINT_WINDOW: usize = 64;

pub(super) const STARTUP_COLUMNS: [StartupSortColumn; 4] = [
    StartupSortColumn::Name,
    StartupSortColumn::Publisher,
    StartupSortColumn::Status,
    StartupSortColumn::Impact,
];

#[inline]
pub(super) const fn column_slug(col: StartupSortColumn) -> &'static str {
    match col {
        StartupSortColumn::Name => "name",
        StartupSortColumn::Publisher => "publisher",
        StartupSortColumn::Status => "status",
        StartupSortColumn::Impact => "impact",
    }
}

#[inline]
pub(super) const fn column_width(col: StartupSortColumn) -> f32 {
    match col {
        StartupSortColumn::Name => 280.0,
        StartupSortColumn::Publisher => 220.0,
        StartupSortColumn::Status => 120.0,
        StartupSortColumn::Impact => 150.0,
    }
}

#[inline]
pub(super) const fn column_min_width(col: StartupSortColumn) -> f32 {
    match col {
        StartupSortColumn::Name => 140.0,
        StartupSortColumn::Publisher => 110.0,
        StartupSortColumn::Status => 84.0,
        StartupSortColumn::Impact => 96.0,
    }
}

#[inline]
pub(super) const fn column_default_direction(col: StartupSortColumn) -> SortDirection {
    match col {
        StartupSortColumn::Status | StartupSortColumn::Impact => SortDirection::Descending,
        StartupSortColumn::Name | StartupSortColumn::Publisher => SortDirection::Ascending,
    }
}

#[inline]
pub(super) const fn to_column_sort(dir: SortDirection) -> ColumnSort {
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
pub(super) struct StartupToolbarState {
    pub(super) can_enable: bool,
    pub(super) can_disable: bool,
}

/// Cached localized strings for [`super::table::StartupTableDelegate`].
pub(super) struct StartupLocaleCache {
    col_name: SharedString,
    col_publisher: SharedString,
    col_status: SharedString,
    col_impact: SharedString,
    pub(super) enabled: SharedString,
    pub(super) disabled: SharedString,
    impact_high: SharedString,
    impact_medium: SharedString,
    impact_low: SharedString,
    impact_not_measured: SharedString,
    impact_none: SharedString,
    pub(super) loading: SharedString,
    pub(super) empty: SharedString,
}

impl StartupLocaleCache {
    pub(super) fn build() -> Self {
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
    pub(super) fn column_label(&self, col: StartupSortColumn) -> SharedString {
        match col {
            StartupSortColumn::Name => self.col_name.clone(),
            StartupSortColumn::Publisher => self.col_publisher.clone(),
            StartupSortColumn::Status => self.col_status.clone(),
            StartupSortColumn::Impact => self.col_impact.clone(),
        }
    }

    #[inline]
    pub(super) fn impact_label(&self, impact: StartupImpact) -> SharedString {
        match impact {
            StartupImpact::High => self.impact_high.clone(),
            StartupImpact::Medium => self.impact_medium.clone(),
            StartupImpact::Low => self.impact_low.clone(),
            StartupImpact::NotMeasured => self.impact_not_measured.clone(),
            StartupImpact::None => self.impact_none.clone(),
        }
    }
}
