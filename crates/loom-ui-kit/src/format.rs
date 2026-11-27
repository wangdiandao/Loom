//! Formatting helpers and semantic intensity buckets for resource metrics.

use gpui_kit::SharedString;
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

/// Honest placeholder string rendered when a sensor, counter, or sleeping
/// device cannot report a valid measurement (`"—"` instead of a fabricated
/// `0%` or `0°C`).
pub const UNAVAILABLE_LABEL: &str = "—";

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

/// Zero-allocation fast-path permille formatter returning a [`SharedString`].
///
/// Avoids heap allocations for the common `0.0%` and `100.0%` values in hot table cells.
#[inline]
pub fn format_permille_shared(permille: u16) -> SharedString {
    match permille {
        0 => SharedString::new_static("0.0%"),
        1_000 => SharedString::new_static("100.0%"),
        _ => format_permille(permille).into(),
    }
}

/// Formats an optional permille utilization value (`0..=1000`) as a percentage
/// [`SharedString`], or [`UNAVAILABLE_LABEL`] (`"—"`) when the metric is
/// unavailable rather than fabricating `"0.0%"`.
#[inline]
pub fn format_opt_percent_permille(val: Option<u16>) -> SharedString {
    match val {
        Some(permille) => format_permille_shared(permille),
        None => SharedString::new_static(UNAVAILABLE_LABEL),
    }
}

/// Formats an optional temperature in whole degrees Celsius, returning
/// [`UNAVAILABLE_LABEL`] (`"—"`) when the device is sleeping (e.g. D3cold
/// discrete GPU) or the sensor reading is unavailable rather than fabricating
/// `"0°C"`.
#[inline]
pub fn format_opt_temp_celsius(val: Option<i16>, sleeping: bool) -> SharedString {
    if sleeping {
        return SharedString::new_static(UNAVAILABLE_LABEL);
    }
    match val {
        Some(celsius) => SharedString::from(format!("{celsius}°C")),
        None => SharedString::new_static(UNAVAILABLE_LABEL),
    }
}

/// Zero-allocation fast-path byte formatter returning a [`SharedString`].
#[inline]
pub fn format_bytes_shared(bytes: u64) -> SharedString {
    if bytes == 0 {
        SharedString::new_static("0 B")
    } else {
        format_bytes(bytes).into()
    }
}

/// Zero-allocation fast-path throughput formatter returning a [`SharedString`].
#[inline]
pub fn format_rate_bytes_per_sec_shared(bytes_per_sec: u64) -> SharedString {
    if bytes_per_sec == 0 {
        SharedString::new_static("0 MB/s")
    } else {
        format_rate_bytes_per_sec(bytes_per_sec).into()
    }
}

/// Zero-allocation fast-path network rate formatter returning a [`SharedString`].
#[inline]
pub fn format_network_rate_mbps_shared(bytes_per_sec: u64) -> SharedString {
    if bytes_per_sec == 0 {
        SharedString::new_static("0 Mbps")
    } else {
        format_network_rate_mbps(bytes_per_sec).into()
    }
}

/// Formats a memory table cell as a [`SharedString`] with zero-value static fast paths.
#[inline]
pub fn format_memory_cell_shared(
    bytes: u64,
    permille: u16,
    mode: ResourceDisplayMode,
) -> SharedString {
    match mode {
        ResourceDisplayMode::Percent => format_permille_shared(permille),
        ResourceDisplayMode::Values => format_bytes_shared(bytes),
    }
}

/// Formats a disk throughput table cell as a [`SharedString`] with zero-value static fast paths.
#[inline]
pub fn format_disk_cell_shared(
    bytes_per_sec: u64,
    permille: u16,
    mode: ResourceDisplayMode,
) -> SharedString {
    match mode {
        ResourceDisplayMode::Percent => format_permille_shared(permille),
        ResourceDisplayMode::Values => format_rate_bytes_per_sec_shared(bytes_per_sec),
    }
}

/// Formats a network throughput table cell as a [`SharedString`] with zero-value static fast paths.
#[inline]
pub fn format_network_cell_shared(
    bytes_per_sec: u64,
    permille: u16,
    mode: ResourceDisplayMode,
) -> SharedString {
    match mode {
        ResourceDisplayMode::Percent => format_permille_shared(permille),
        ResourceDisplayMode::Values => format_network_rate_mbps_shared(bytes_per_sec),
    }
}

/// Zero-allocation fast-path cumulative CPU time formatter returning a [`SharedString`].
#[inline]
pub fn format_cpu_time_shared(ticks_100ns: u64) -> SharedString {
    if ticks_100ns < TICKS_PER_SECOND {
        SharedString::new_static("00:00:00")
    } else {
        format_cpu_time(ticks_100ns).into()
    }
}

/// Formats a processor frequency in MHz as GHz with two decimal places
/// (e.g., `3820` -> `"3.82 GHz"`, `800` -> `"0.80 GHz"`).
#[inline]
pub fn format_freq_ghz(mhz: u32) -> String {
    format!("{:.2} GHz", f64::from(mhz) / 1000.0)
}

/// Formats a bit rate in bits per second using decimal SI prefixes
/// (`bps`, `Kbps`, `Mbps`, `Gbps`).
pub fn format_bits_per_sec(bps: u64) -> String {
    if bps == 0 {
        return String::from("0 bps");
    }
    if bps < 1_000 {
        format!("{bps} bps")
    } else if bps < 1_000_000 {
        format!("{:.1} Kbps", bps as f64 / 1_000.0)
    } else if bps < 1_000_000_000 {
        format!("{:.1} Mbps", bps as f64 / 1_000_000.0)
    } else {
        format!("{:.1} Gbps", bps as f64 / 1_000_000_000.0)
    }
}

/// Formats an I/O response latency in microseconds (`"0.0 ms"`, `"250 µs"`,
/// `"12.4 ms"`, `"1.50 s"`).
pub fn format_latency_us(us: u64) -> String {
    if us == 0 {
        String::from("0.0 ms")
    } else if us < 1_000 {
        format!("{us} µs")
    } else if us < 1_000_000 {
        format!("{:.1} ms", us as f64 / 1_000.0)
    } else {
        format!("{:.2} s", us as f64 / 1_000_000.0)
    }
}

/// Computes a discrete, jitter-resistant Y-axis ceiling (in bytes per second) and
/// formatted top-right scale label (`KB/s`, `MB/s`, `GB/s`) for disk transfer charts.
///
/// The returned `f32` ceiling uses a minimum floor of `100 KB/s` (`102_400.0`) so
/// idle disks render against a stable baseline.
pub fn adaptive_throughput_scale(max_bytes_per_sec: u64) -> (f32, SharedString) {
    const STEPS: &[(u64, &str)] = &[
        (100 * KIB, "100 KB/s"),
        (500 * KIB, "500 KB/s"),
        (MIB, "1 MB/s"),
        (10 * MIB, "10 MB/s"),
        (50 * MIB, "50 MB/s"),
        (100 * MIB, "100 MB/s"),
        (500 * MIB, "500 MB/s"),
        (GIB, "1 GB/s"),
        (2 * GIB, "2 GB/s"),
        (5 * GIB, "5 GB/s"),
        (10 * GIB, "10 GB/s"),
    ];

    for &(limit, label) in STEPS {
        if max_bytes_per_sec <= limit {
            return (limit as f32, SharedString::new_static(label));
        }
    }

    let gbps = max_bytes_per_sec.div_ceil(GIB);
    let ceiling = gbps.saturating_mul(GIB);
    (ceiling as f32, SharedString::from(format!("{gbps} GB/s")))
}

/// Computes a discrete, jitter-resistant Y-axis ceiling (in **bits per second**)
/// and formatted top-right scale label (`Kbps`, `Mbps`, `Gbps`) for network charts.
///
/// Accepts peak throughput in `bytes_per_sec` (as stored in `ThroughputRing`) and
/// converts to decimal bits per second (`bytes * 8`) with a minimum floor of
/// `100 Kbps` (`100_000.0`).
pub fn adaptive_network_scale(max_bytes_per_sec: u64) -> (f32, SharedString) {
    const KBPS: u64 = 1_000;
    const MBPS: u64 = 1_000_000;
    const GBPS: u64 = 1_000_000_000;
    const STEPS: &[(u64, &str)] = &[
        (100 * KBPS, "100 Kbps"),
        (500 * KBPS, "500 Kbps"),
        (MBPS, "1 Mbps"),
        (10 * MBPS, "10 Mbps"),
        (50 * MBPS, "50 Mbps"),
        (100 * MBPS, "100 Mbps"),
        (500 * MBPS, "500 Mbps"),
        (GBPS, "1 Gbps"),
        (2 * GBPS, "2 Gbps"),
        (5 * GBPS, "5 Gbps"),
        (10 * GBPS, "10 Gbps"),
    ];

    let max_bps = max_bytes_per_sec.saturating_mul(8);
    for &(limit, label) in STEPS {
        if max_bps <= limit {
            return (limit as f32, SharedString::new_static(label));
        }
    }

    let gbps = max_bps.div_ceil(GBPS);
    let ceiling = gbps.saturating_mul(GBPS);
    (ceiling as f32, SharedString::from(format!("{gbps} Gbps")))
}
