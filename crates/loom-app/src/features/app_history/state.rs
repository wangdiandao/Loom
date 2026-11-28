//! Column metadata, UTC datetime formatting, heatmap buckets, scroll coalescing,
//! locale caching, and viewport fingerprinting for the App History feature slice.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};
use std::ops::Range;
use std::time::{Duration, Instant};

use gpui_kit::SharedString;
use gpui_kit::component::table::ColumnSort;
use loom_types::{AppHistorySlice, AppHistorySortColumn, SortDirection};
use loom_ui_kit::{HeatmapLevel, quantize_bytes_display, quantize_cpu_time_secs};

use crate::i18n;

pub(super) const APP_HISTORY_HEADER_HEIGHT: f32 = 32.0;
const SCROLL_COALESCE_QUIET_WINDOW: Duration = Duration::from_millis(150);
const SCROLL_COALESCE_MAX_DEFER: Duration = Duration::from_millis(500);
pub(super) const CPU_TIME_TEXT_CACHE_LIMIT: usize = 512;
const DEFAULT_FINGERPRINT_WINDOW: usize = 64;

pub(super) const APP_HISTORY_COLUMNS: [AppHistorySortColumn; 4] = [
    AppHistorySortColumn::Name,
    AppHistorySortColumn::CpuTime,
    AppHistorySortColumn::Network,
    AppHistorySortColumn::MeteredNetwork,
];

#[inline]
pub(super) const fn column_slug(col: AppHistorySortColumn) -> &'static str {
    match col {
        AppHistorySortColumn::Name => "name",
        AppHistorySortColumn::CpuTime => "cpu_time",
        AppHistorySortColumn::Network => "network",
        AppHistorySortColumn::MeteredNetwork => "metered_network",
    }
}

#[inline]
pub(super) const fn column_width(col: AppHistorySortColumn) -> f32 {
    match col {
        AppHistorySortColumn::Name => 320.0,
        AppHistorySortColumn::CpuTime => 150.0,
        AppHistorySortColumn::Network => 150.0,
        AppHistorySortColumn::MeteredNetwork => 170.0,
    }
}

#[inline]
pub(super) const fn column_min_width(col: AppHistorySortColumn) -> f32 {
    match col {
        AppHistorySortColumn::Name => 150.0,
        AppHistorySortColumn::CpuTime => 100.0,
        AppHistorySortColumn::Network => 100.0,
        AppHistorySortColumn::MeteredNetwork => 116.0,
    }
}

#[inline]
pub(super) const fn column_right_aligned(col: AppHistorySortColumn) -> bool {
    matches!(
        col,
        AppHistorySortColumn::CpuTime
            | AppHistorySortColumn::Network
            | AppHistorySortColumn::MeteredNetwork
    )
}

#[inline]
pub(super) const fn column_is_heatmap(col: AppHistorySortColumn) -> bool {
    matches!(
        col,
        AppHistorySortColumn::CpuTime
            | AppHistorySortColumn::Network
            | AppHistorySortColumn::MeteredNetwork
    )
}

#[inline]
pub(super) const fn column_default_direction(col: AppHistorySortColumn) -> SortDirection {
    match col {
        AppHistorySortColumn::CpuTime
        | AppHistorySortColumn::Network
        | AppHistorySortColumn::MeteredNetwork => SortDirection::Descending,
        AppHistorySortColumn::Name => SortDirection::Ascending,
    }
}

#[inline]
pub(super) const fn to_column_sort(dir: SortDirection) -> ColumnSort {
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
/// display-quantized CPU seconds and byte buckets.
pub(super) fn compute_app_history_viewport_fingerprint(
    slice: &AppHistorySlice,
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
        if let Some(record) = slice.items.get(item_idx) {
            record.app_id.hash(&mut hasher);
            record.name.hash(&mut hasher);
            quantize_cpu_time_secs(record.cpu_time_100ns).hash(&mut hasher);
            quantize_bytes_display(record.network_bytes).hash(&mut hasher);
            cumulative_bytes_heatmap_level(record.network_bytes).hash(&mut hasher);
            quantize_bytes_display(record.metered_network_bytes).hash(&mut hasher);
            cumulative_bytes_heatmap_level(record.metered_network_bytes).hash(&mut hasher);
        }
    }

    hasher.finish()
}

/// Cached localized strings for [`super::table::AppHistoryTableDelegate`].
pub(super) struct AppHistoryLocaleCache {
    col_name: SharedString,
    col_cpu_time: SharedString,
    col_network: SharedString,
    col_metered_network: SharedString,
    pub(super) loading: SharedString,
    pub(super) empty: SharedString,
}

impl AppHistoryLocaleCache {
    pub(super) fn build() -> Self {
        Self {
            col_name: i18n::tr("app_history.col_name").into(),
            col_cpu_time: i18n::tr("app_history.col_cpu_time").into(),
            col_network: i18n::tr("app_history.col_network").into(),
            col_metered_network: i18n::tr("app_history.col_metered_network").into(),
            loading: i18n::tr("app_history.loading").into(),
            empty: i18n::tr("app_history.empty").into(),
        }
    }

    #[inline]
    pub(super) fn column_label(&self, col: AppHistorySortColumn) -> SharedString {
        match col {
            AppHistorySortColumn::Name => self.col_name.clone(),
            AppHistorySortColumn::CpuTime => self.col_cpu_time.clone(),
            AppHistorySortColumn::Network => self.col_network.clone(),
            AppHistorySortColumn::MeteredNetwork => self.col_metered_network.clone(),
        }
    }
}
