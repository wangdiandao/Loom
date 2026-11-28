//! Formatting helpers and semantic intensity buckets for resource metrics.

mod metrics;
mod preview;
mod tables;
#[cfg(test)]
mod tests;

use gpui_kit::SharedString;

pub use metrics::{
    UNAVAILABLE_LABEL, adaptive_network_scale, adaptive_throughput_scale, format_bits_per_sec,
    format_bytes, format_bytes_metric, format_bytes_shared, format_cpu_time,
    format_cpu_time_shared, format_disk_cell, format_disk_cell_metric, format_disk_cell_shared,
    format_freq_ghz, format_latency_us, format_memory_cell, format_memory_cell_metric,
    format_memory_cell_shared, format_network_cell, format_network_cell_metric,
    format_network_cell_shared, format_network_rate_mbps, format_network_rate_mbps_metric,
    format_network_rate_mbps_shared, format_opt_clock_mhz, format_opt_fan_percent,
    format_opt_percent_permille, format_opt_power_mw, format_opt_temp_celsius, format_permille,
    format_permille_metric, format_rate_bytes_per_sec, format_rate_bytes_per_sec_metric,
    format_rate_bytes_per_sec_shared, format_signed_bytes, format_signed_bytes_metric,
    format_uptime, quantize_bytes_display, quantize_cpu_time_secs, quantize_net_bps_display,
    quantize_rate_bps_display, quantize_signed_bytes_display,
};
pub use preview::{DEFAULT_CELL_PREVIEW_CHARS, cap_single_line_preview, display_preview_shared};
pub use tables::{
    CHILD_COUNT_TABLE_MAX, SMALL_U32_TABLE_MAX, TENTHS_TABLE_MAX, format_child_count_shared,
    format_permille_shared, format_tenths_shared, format_u32_shared,
};

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;

/// 100 KB/s threshold for [`HeatmapLevel::Low`] throughput cells.
const RATE_LOW_BPS: u64 = 100 * KIB;
/// 2 MB/s threshold for [`HeatmapLevel::Medium`] throughput cells.
const RATE_MEDIUM_BPS: u64 = 2 * MIB;
/// 20 MB/s threshold for [`HeatmapLevel::High`] throughput cells.
const RATE_HIGH_BPS: u64 = 20 * MIB;
/// 100 MB/s threshold for [`HeatmapLevel::Critical`] throughput cells.
const RATE_CRITICAL_BPS: u64 = 100 * MIB;

/// Split numeric value and static unit suffix for high-frequency table cells.
///
/// `gpui-fast`'s `NumberShaping` fast path only matches strings whose bytes
/// belong to `b"0123456789.,+-%$KMBT"` (excluding spaces, `'G'`, `'/'`, `'s'`,
/// `'p'`, etc.). Keeping the numeric portion in [`Self::value`] and the unit
/// suffix in [`Self::unit`] lets the value hit `NumberShaping` on every frame
/// while the static unit hits `LineLayoutCache` with zero re-shaping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricDisplay {
    /// Pure numeric text (bytes strictly in `b"0123456789.,+-%"`).
    pub value: SharedString,
    /// Static trailing unit string (e.g. `" MB"`, `" MB/s"`, `" Mbps"`, or `""`).
    pub unit: &'static str,
}

impl MetricDisplay {
    /// Creates a new [`MetricDisplay`] from a numeric string and static unit suffix.
    #[inline]
    pub fn new(value: impl Into<SharedString>, unit: &'static str) -> Self {
        Self {
            value: value.into(),
            unit,
        }
    }

    /// Creates a [`MetricDisplay`] backed entirely by static string slices.
    #[inline]
    pub const fn from_static(value: &'static str, unit: &'static str) -> Self {
        Self {
            value: SharedString::new_static(value),
            unit,
        }
    }
}

/// Semantic intensity bucket for process resource heatmap cells.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HeatmapLevel {
    /// Negligible utilization (`< 5%` or `< 100 KB/s`).
    #[default]
    None,
    /// Light utilization (`5%..25%` or `100 KB/s..2 MB/s`).
    Low,
    /// Moderate utilization (`25%..60%` or `2 MB/s..20 MB/s`).
    Medium,
    /// High utilization (`60%..85%` or `20 MB/s..100 MB/s`).
    High,
    /// Critical utilization (`>= 85%` or `>= 100 MB/s`).
    Critical,
}

impl HeatmapLevel {
    /// Maps a quantized permille value (`0..=1000`) to a [`HeatmapLevel`].
    #[inline]
    pub const fn from_permille(permille: u16) -> Self {
        match permille {
            0..=49 => Self::None,
            50..=249 => Self::Low,
            250..=599 => Self::Medium,
            600..=849 => Self::High,
            _ => Self::Critical,
        }
    }

    /// Maps an I/O throughput in bytes per second to a [`HeatmapLevel`].
    ///
    /// Thresholds:
    /// - `< 100 KB/s` -> [`HeatmapLevel::None`]
    /// - `100 KB/s..2 MB/s` -> [`HeatmapLevel::Low`]
    /// - `2 MB/s..20 MB/s` -> [`HeatmapLevel::Medium`]
    /// - `20 MB/s..100 MB/s` -> [`HeatmapLevel::High`]
    /// - `>= 100 MB/s` -> [`HeatmapLevel::Critical`]
    #[inline]
    pub const fn from_bytes_per_sec(bytes_per_sec: u64) -> Self {
        if bytes_per_sec < RATE_LOW_BPS {
            Self::None
        } else if bytes_per_sec < RATE_MEDIUM_BPS {
            Self::Low
        } else if bytes_per_sec < RATE_HIGH_BPS {
            Self::Medium
        } else if bytes_per_sec < RATE_CRITICAL_BPS {
            Self::High
        } else {
            Self::Critical
        }
    }
}
