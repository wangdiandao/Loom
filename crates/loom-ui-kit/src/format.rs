//! Formatting helpers and semantic intensity buckets for resource metrics.

use loom_types::ResourceDisplayMode;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;
const TIB: u64 = 1024 * GIB;

/// 100 KB/s threshold for [`HeatmapLevel::Low`] throughput cells.
const RATE_LOW_BPS: u64 = 100 * KIB;
/// 2 MB/s threshold for [`HeatmapLevel::Medium`] throughput cells.
const RATE_MEDIUM_BPS: u64 = 2 * MIB;
/// 20 MB/s threshold for [`HeatmapLevel::High`] throughput cells.
const RATE_HIGH_BPS: u64 = 20 * MIB;
/// 100 MB/s threshold for [`HeatmapLevel::Critical`] throughput cells.
const RATE_CRITICAL_BPS: u64 = 100 * MIB;

/// 100-nanosecond ticks per second (`10_000_000`).
const TICKS_PER_SECOND: u64 = 10_000_000;

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

/// Formats a permille value (`0..=1000`) as a percentage string with one decimal
/// place (e.g., `0` -> `"0.0%"`, `125` -> `"12.5%"`, `1000` -> `"100.0%"`).
#[inline]
pub fn format_permille(permille: u16) -> String {
    format!("{}.{}%", permille / 10, permille % 10)
}

/// Formats a byte count using binary (`1024`-based) units (`B`, `KB`, `MB`,
/// `GB`, `TB`), showing one decimal place for `MB` and above.
pub fn format_bytes(bytes: u64) -> String {
    if bytes < KIB {
        format!("{bytes} B")
    } else if bytes < MIB {
        format!("{} KB", bytes / KIB)
    } else if bytes < GIB {
        format!("{:.1} MB", bytes as f64 / MIB as f64)
    } else if bytes < TIB {
        format!("{:.1} GB", bytes as f64 / GIB as f64)
    } else {
        format!("{:.1} TB", bytes as f64 / TIB as f64)
    }
}

/// Formats a storage/transfer throughput in bytes per second (e.g., `"0 MB/s"`,
/// `"512.0 KB/s"`, `"1.4 MB/s"`, `"2.1 GB/s"`).
pub fn format_rate_bytes_per_sec(bytes_per_sec: u64) -> String {
    if bytes_per_sec == 0 {
        return String::from("0 MB/s");
    }
    if bytes_per_sec < KIB {
        format!("{bytes_per_sec} B/s")
    } else if bytes_per_sec < MIB {
        format!("{:.1} KB/s", bytes_per_sec as f64 / KIB as f64)
    } else if bytes_per_sec < GIB {
        format!("{:.1} MB/s", bytes_per_sec as f64 / MIB as f64)
    } else {
        format!("{:.1} GB/s", bytes_per_sec as f64 / GIB as f64)
    }
}

/// Formats a network throughput given in bytes per second as decimal bits per
/// second (`"0 Mbps"`, `"12.4 Mbps"`, `"1.2 Gbps"`, etc.).
pub fn format_network_rate_mbps(bytes_per_sec: u64) -> String {
    if bytes_per_sec == 0 {
        return String::from("0 Mbps");
    }
    let bits_per_sec = bytes_per_sec as f64 * 8.0;
    if bits_per_sec < 1_000.0 {
        format!("{bits_per_sec:.0} bps")
    } else if bits_per_sec < 100_000.0 {
        format!("{:.1} Kbps", bits_per_sec / 1_000.0)
    } else if bits_per_sec < 1_000_000_000.0 {
        format!("{:.1} Mbps", bits_per_sec / 1_000_000.0)
    } else {
        format!("{:.1} Gbps", bits_per_sec / 1_000_000_000.0)
    }
}

/// Formats a memory table cell according to the active [`ResourceDisplayMode`].
#[inline]
pub fn format_memory_cell(bytes: u64, permille: u16, mode: ResourceDisplayMode) -> String {
    match mode {
        ResourceDisplayMode::Percent => format_permille(permille),
        ResourceDisplayMode::Values => format_bytes(bytes),
    }
}

/// Formats a disk throughput table cell according to the active [`ResourceDisplayMode`].
#[inline]
pub fn format_disk_cell(bytes_per_sec: u64, permille: u16, mode: ResourceDisplayMode) -> String {
    match mode {
        ResourceDisplayMode::Percent => format_permille(permille),
        ResourceDisplayMode::Values => format_rate_bytes_per_sec(bytes_per_sec),
    }
}

/// Formats a network throughput table cell according to the active [`ResourceDisplayMode`].
#[inline]
pub fn format_network_cell(bytes_per_sec: u64, permille: u16, mode: ResourceDisplayMode) -> String {
    match mode {
        ResourceDisplayMode::Percent => format_permille(permille),
        ResourceDisplayMode::Values => format_network_rate_mbps(bytes_per_sec),
    }
}

/// Formats a system uptime duration in seconds as `"D:HH:MM:SS"`.
pub fn format_uptime(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    let rem_secs = secs % 60;
    format!("{days}:{hours:02}:{mins:02}:{rem_secs:02}")
}

/// Formats cumulative CPU time in 100-nanosecond ticks as `"HH:MM:SS"`.
pub fn format_cpu_time(ticks_100ns: u64) -> String {
    let total_secs = ticks_100ns / TICKS_PER_SECOND;
    let hours = total_secs / 3_600;
    let mins = (total_secs % 3_600) / 60;
    let secs = total_secs % 60;
    format!("{hours:02}:{mins:02}:{secs:02}")
}
