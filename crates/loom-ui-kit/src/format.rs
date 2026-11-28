//! Formatting helpers and semantic intensity buckets for resource metrics.

use std::sync::{Arc, OnceLock};

use gpui_kit::SharedString;
use loom_types::ResourceDisplayMode;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;
const TIB: u64 = 1024 * GIB;

/// Maximum `u32` value covered by the zero-allocation static integer table.
pub const SMALL_U32_TABLE_MAX: u32 = 1024;

/// Maximum child count covered by the zero-allocation static `"(N)"` table.
pub const CHILD_COUNT_TABLE_MAX: u32 = 256;

/// Maximum quantized tenths value covered by the lazily-chunked static table
/// (`10_299` -> `"1029.9"`, covering every `KB/MB/GB/TB` and `Kbps/Mbps/Gbps`
/// single-decimal tier below `1030.0`).
pub const TENTHS_TABLE_MAX: u64 = 10_299;

/// Number of entries per lazy tenths chunk (`10.0` per chunk).
const TENTHS_CHUNK_LEN: usize = 100;
/// Total lazy tenths chunks covering `0..=10_299`.
const TENTHS_CHUNK_COUNT: usize = (TENTHS_TABLE_MAX as usize / TENTHS_CHUNK_LEN) + 1;

/// Maximum magnitude covered by the lazily-chunked signed integer delta table (`±1024`).
const SIGNED_U32_TABLE_MAX: u64 = 1024;
const SIGNED_U32_CHUNK_LEN: usize = 64;
const SIGNED_U32_CHUNK_COUNT: usize = (SIGNED_U32_TABLE_MAX as usize / SIGNED_U32_CHUNK_LEN) + 1;

/// Maximum magnitude covered by the lazily-chunked signed tenths delta table (`±127.9`).
const SIGNED_TENTHS_TABLE_MAX: u64 = 1279;
const SIGNED_TENTHS_CHUNK_LEN: usize = 64;
const SIGNED_TENTHS_CHUNK_COUNT: usize =
    (SIGNED_TENTHS_TABLE_MAX as usize / SIGNED_TENTHS_CHUNK_LEN) + 1;

/// Default single-line character cap applied to long table cell preview strings
/// (such as command lines or executable paths) before passing them to text shaping.
pub const DEFAULT_CELL_PREVIEW_CHARS: usize = 256;

/// Pre-formatted permille strings covering `"0.0%" ..= "100.0%"`.
static PERMILLE_TABLE: OnceLock<[SharedString; 1001]> = OnceLock::new();

/// Pre-formatted small unsigned integer strings covering `"0" ..= "1024"`.
static SMALL_U32_TABLE: OnceLock<[SharedString; SMALL_U32_TABLE_MAX as usize + 1]> =
    OnceLock::new();

/// Pre-formatted child-count badge strings covering `"(0)" ..= "(256)"`.
static CHILD_COUNT_TABLE: OnceLock<[SharedString; 257]> = OnceLock::new();

/// Lazily initialized 100-entry chunks of `"X.Y"` strings covering `"0.0" ..= "1029.9"`.
static TENTHS_CHUNKS: [OnceLock<[SharedString; TENTHS_CHUNK_LEN]>; TENTHS_CHUNK_COUNT] =
    [const { OnceLock::new() }; TENTHS_CHUNK_COUNT];

/// Lazily initialized signed integer chunks (`+0..=+1024` and `-0..=-1024`).
static SIGNED_POS_U32_CHUNKS: [OnceLock<[SharedString; SIGNED_U32_CHUNK_LEN]>;
    SIGNED_U32_CHUNK_COUNT] = [const { OnceLock::new() }; SIGNED_U32_CHUNK_COUNT];
static SIGNED_NEG_U32_CHUNKS: [OnceLock<[SharedString; SIGNED_U32_CHUNK_LEN]>;
    SIGNED_U32_CHUNK_COUNT] = [const { OnceLock::new() }; SIGNED_U32_CHUNK_COUNT];

/// Lazily initialized signed tenths chunks (`+0.0..=+127.9` and `-0.0..=-127.9`).
static SIGNED_POS_TENTHS_CHUNKS: [OnceLock<[SharedString; SIGNED_TENTHS_CHUNK_LEN]>;
    SIGNED_TENTHS_CHUNK_COUNT] = [const { OnceLock::new() }; SIGNED_TENTHS_CHUNK_COUNT];
static SIGNED_NEG_TENTHS_CHUNKS: [OnceLock<[SharedString; SIGNED_TENTHS_CHUNK_LEN]>;
    SIGNED_TENTHS_CHUNK_COUNT] = [const { OnceLock::new() }; SIGNED_TENTHS_CHUNK_COUNT];

#[inline]
fn permille_table() -> &'static [SharedString; 1001] {
    PERMILLE_TABLE.get_or_init(|| {
        std::array::from_fn(|p| match p {
            0 => SharedString::new_static("0.0%"),
            1_000 => SharedString::new_static("100.0%"),
            _ => SharedString::from(format!("{}.{}%", p / 10, p % 10)),
        })
    })
}

#[inline]
fn small_u32_table() -> &'static [SharedString; SMALL_U32_TABLE_MAX as usize + 1] {
    SMALL_U32_TABLE.get_or_init(|| {
        std::array::from_fn(|n| match n {
            0 => SharedString::new_static("0"),
            _ => SharedString::from(n.to_string()),
        })
    })
}

#[inline]
fn child_count_table() -> &'static [SharedString; 257] {
    CHILD_COUNT_TABLE.get_or_init(|| std::array::from_fn(|n| SharedString::from(format!("({n})"))))
}

/// Exact integer division rounded to nearest integer with ties-to-even.
#[inline]
const fn div_round_ties_even_u128(num: u128, den: u128) -> u64 {
    let q = num / den;
    let r = num % den;
    let twice_r = r * 2;
    let rounded = if twice_r > den || (twice_r == den && (q & 1) == 1) {
        q + 1
    } else {
        q
    };
    rounded as u64
}

/// Zero-allocation fast-path tenths formatter returning a [`SharedString`]
/// strictly using ASCII digits and `'.'` so `gpui-fast::NumberShaping` always hits.
///
/// Values in `0..=10_299` (`"0.0" ..= "1029.9"`) clone from lazily-initialized
/// 100-entry static chunks with zero steady-state heap allocation; larger values
/// fall back to dynamic formatting.
#[inline]
pub fn format_tenths_shared(tenths: u64) -> SharedString {
    if tenths <= TENTHS_TABLE_MAX {
        let idx = tenths as usize;
        let chunk_ix = idx / TENTHS_CHUNK_LEN;
        let offset = idx % TENTHS_CHUNK_LEN;
        let chunk = TENTHS_CHUNKS[chunk_ix].get_or_init(|| {
            let base = chunk_ix * TENTHS_CHUNK_LEN;
            std::array::from_fn(|i| {
                let v = base + i;
                SharedString::from(format!("{}.{}", v / 10, v % 10))
            })
        });
        chunk[offset].clone()
    } else {
        SharedString::from(format!("{}.{}", tenths / 10, tenths % 10))
    }
}

#[inline]
fn format_signed_u32_shared(positive: bool, mag: u64) -> SharedString {
    if mag <= SIGNED_U32_TABLE_MAX {
        let idx = mag as usize;
        let chunk_ix = idx / SIGNED_U32_CHUNK_LEN;
        let offset = idx % SIGNED_U32_CHUNK_LEN;
        let sign = if positive { '+' } else { '-' };
        let chunks = if positive {
            &SIGNED_POS_U32_CHUNKS
        } else {
            &SIGNED_NEG_U32_CHUNKS
        };
        let chunk = chunks[chunk_ix].get_or_init(|| {
            let base = chunk_ix * SIGNED_U32_CHUNK_LEN;
            std::array::from_fn(|i| SharedString::from(format!("{sign}{}", base + i)))
        });
        chunk[offset].clone()
    } else {
        let sign = if positive { '+' } else { '-' };
        SharedString::from(format!("{sign}{mag}"))
    }
}

#[inline]
fn format_signed_tenths_shared(positive: bool, tenths: u64) -> SharedString {
    if tenths <= SIGNED_TENTHS_TABLE_MAX {
        let idx = tenths as usize;
        let chunk_ix = idx / SIGNED_TENTHS_CHUNK_LEN;
        let offset = idx % SIGNED_TENTHS_CHUNK_LEN;
        let sign = if positive { '+' } else { '-' };
        let chunks = if positive {
            &SIGNED_POS_TENTHS_CHUNKS
        } else {
            &SIGNED_NEG_TENTHS_CHUNKS
        };
        let chunk = chunks[chunk_ix].get_or_init(|| {
            let base = chunk_ix * SIGNED_TENTHS_CHUNK_LEN;
            std::array::from_fn(|i| {
                let v = base + i;
                SharedString::from(format!("{sign}{}.{}", v / 10, v % 10))
            })
        });
        chunk[offset].clone()
    } else {
        let sign = if positive { '+' } else { '-' };
        SharedString::from(format!("{sign}{}.{}", tenths / 10, tenths % 10))
    }
}

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

/// Formats a signed byte delta using binary (`1024`-based) units (`"0 B"`,
/// `"+512 B"`, `"-4 KB"`, `"+1.5 MB"`, `"-2.0 GB"`, etc.).
pub fn format_signed_bytes(delta_bytes: i64) -> String {
    if delta_bytes == 0 {
        return String::from("0 B");
    }
    let sign = if delta_bytes > 0 { '+' } else { '-' };
    format!("{sign}{}", format_bytes(delta_bytes.unsigned_abs()))
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
/// Uses a static lookup table covering `"0.0%" ..= "100.0%"` so values in
/// `0..=1000` perform zero heap allocations; values above `1000` fall back to
/// dynamic formatting.
#[inline]
pub fn format_permille_shared(permille: u16) -> SharedString {
    if permille <= 1_000 {
        permille_table()[usize::from(permille)].clone()
    } else {
        format_permille(permille).into()
    }
}

/// Zero-allocation fast-path unsigned integer formatter returning a [`SharedString`].
///
/// Values in `0..=512` clone from a pre-initialized lookup table; larger values
/// format dynamically.
#[inline]
pub fn format_u32_shared(val: u32) -> SharedString {
    if val <= SMALL_U32_TABLE_MAX {
        small_u32_table()[val as usize].clone()
    } else {
        val.to_string().into()
    }
}

/// Zero-allocation fast-path process group child-count formatter returning `"(N)"`.
///
/// Values in `0..=256` clone from a pre-initialized lookup table; larger counts
/// format dynamically.
#[inline]
pub fn format_child_count_shared(count: u32) -> SharedString {
    if count <= CHILD_COUNT_TABLE_MAX {
        child_count_table()[count as usize].clone()
    } else {
        format!("({count})").into()
    }
}

#[inline]
const fn quantize_bytes_parts(bytes: u64) -> (u8, u64) {
    if bytes == 0 {
        (0, 0)
    } else if bytes < KIB {
        (1, bytes)
    } else if bytes < MIB {
        (2, bytes / KIB)
    } else if bytes < GIB {
        (
            3,
            div_round_ties_even_u128((bytes as u128) * 10, MIB as u128),
        )
    } else if bytes < TIB {
        (
            4,
            div_round_ties_even_u128((bytes as u128) * 10, GIB as u128),
        )
    } else {
        (
            5,
            div_round_ties_even_u128((bytes as u128) * 10, TIB as u128),
        )
    }
}

#[inline]
const fn quantize_rate_bps_parts(bytes_per_sec: u64) -> (u8, u64) {
    if bytes_per_sec == 0 {
        (0, 0)
    } else if bytes_per_sec < KIB {
        (1, bytes_per_sec)
    } else if bytes_per_sec < MIB {
        (
            2,
            div_round_ties_even_u128((bytes_per_sec as u128) * 10, KIB as u128),
        )
    } else if bytes_per_sec < GIB {
        (
            3,
            div_round_ties_even_u128((bytes_per_sec as u128) * 10, MIB as u128),
        )
    } else {
        (
            4,
            div_round_ties_even_u128((bytes_per_sec as u128) * 10, GIB as u128),
        )
    }
}

#[inline]
const fn quantize_net_bps_parts(bytes_per_sec: u64) -> (u8, u64) {
    if bytes_per_sec == 0 {
        return (0, 0);
    }
    let bits_per_sec = (bytes_per_sec as u128) * 8;
    if bits_per_sec < 1_000 {
        (1, bits_per_sec as u64)
    } else if bits_per_sec < 100_000 {
        (2, div_round_ties_even_u128(bits_per_sec, 100))
    } else if bits_per_sec < 1_000_000_000 {
        (3, div_round_ties_even_u128(bits_per_sec, 100_000))
    } else {
        (4, div_round_ties_even_u128(bits_per_sec, 100_000_000))
    }
}

/// Encodes the display unit tier and rounded numeric value for [`format_bytes_metric`].
///
/// Two byte counts produce the same key if and only if they render identically
/// via [`format_bytes_metric`].
#[inline]
pub const fn quantize_bytes_display(bytes: u64) -> u64 {
    let (tier, val) = quantize_bytes_parts(bytes);
    ((tier as u64) << 60) | val
}

/// Encodes the sign, display unit tier, and rounded magnitude for
/// [`format_signed_bytes_metric`].
///
/// Two signed byte deltas produce the same key if and only if they render
/// identically via [`format_signed_bytes_metric`].
#[inline]
pub const fn quantize_signed_bytes_display(delta_bytes: i64) -> u64 {
    if delta_bytes == 0 {
        return 0;
    }
    let sign_tag: u64 = if delta_bytes > 0 { 1 } else { 2 };
    let (tier, val) = quantize_bytes_parts(delta_bytes.unsigned_abs());
    (sign_tag << 62) | ((tier as u64) << 58) | val
}

/// Encodes the display unit tier and rounded numeric value for [`format_rate_bytes_per_sec_metric`].
///
/// Two byte rates produce the same key if and only if they render identically
/// via [`format_rate_bytes_per_sec_metric`].
#[inline]
pub const fn quantize_rate_bps_display(bytes_per_sec: u64) -> u64 {
    let (tier, val) = quantize_rate_bps_parts(bytes_per_sec);
    ((tier as u64) << 60) | val
}

/// Encodes the display unit tier and rounded numeric value for [`format_network_rate_mbps_metric`].
///
/// Two network byte rates produce the same key if and only if they render
/// identically via [`format_network_rate_mbps_metric`].
#[inline]
pub const fn quantize_net_bps_display(bytes_per_sec: u64) -> u64 {
    let (tier, val) = quantize_net_bps_parts(bytes_per_sec);
    ((tier as u64) << 60) | val
}

/// Quantizes cumulative CPU time in 100-nanosecond ticks down to whole display seconds.
#[inline]
pub const fn quantize_cpu_time_secs(ticks_100ns: u64) -> u64 {
    ticks_100ns / TICKS_PER_SECOND
}

/// Formats a permille value (`0..=1000`) into a [`MetricDisplay`] whose `value`
/// hits `gpui-fast::NumberShaping` directly (with an empty unit suffix).
#[inline]
pub fn format_permille_metric(permille: u16) -> MetricDisplay {
    MetricDisplay::new(format_permille_shared(permille), "")
}

/// Formats a byte count into a split [`MetricDisplay`] (`value` strictly numeric,
/// `unit` in `{" B", " KB", " MB", " GB", " TB"}`).
pub fn format_bytes_metric(bytes: u64) -> MetricDisplay {
    let (tier, val) = quantize_bytes_parts(bytes);
    match tier {
        0 => MetricDisplay::from_static("0", " B"),
        1 => MetricDisplay::new(format_u32_shared(val as u32), " B"),
        2 => MetricDisplay::new(format_u32_shared(val as u32), " KB"),
        3 => MetricDisplay::new(format_tenths_shared(val), " MB"),
        4 => MetricDisplay::new(format_tenths_shared(val), " GB"),
        _ => MetricDisplay::new(format_tenths_shared(val), " TB"),
    }
}

/// Formats a signed byte delta into a split [`MetricDisplay`] (`value` strictly
/// numeric with an explicit `'+'` / `'-'` sign for non-zero deltas, `unit` in
/// `{" B", " KB", " MB", " GB", " TB"}`).
pub fn format_signed_bytes_metric(delta_bytes: i64) -> MetricDisplay {
    if delta_bytes == 0 {
        return MetricDisplay::from_static("0", " B");
    }
    let positive = delta_bytes > 0;
    let (tier, val) = quantize_bytes_parts(delta_bytes.unsigned_abs());
    match tier {
        0 => MetricDisplay::from_static("0", " B"),
        1 => MetricDisplay::new(format_signed_u32_shared(positive, val), " B"),
        2 => MetricDisplay::new(format_signed_u32_shared(positive, val), " KB"),
        3 => MetricDisplay::new(format_signed_tenths_shared(positive, val), " MB"),
        4 => MetricDisplay::new(format_signed_tenths_shared(positive, val), " GB"),
        _ => MetricDisplay::new(format_signed_tenths_shared(positive, val), " TB"),
    }
}

/// Formats a storage/transfer throughput in bytes per second into a split
/// [`MetricDisplay`] (`value` strictly numeric, `unit` in `{" B/s", " KB/s", " MB/s", " GB/s"}`).
pub fn format_rate_bytes_per_sec_metric(bytes_per_sec: u64) -> MetricDisplay {
    let (tier, val) = quantize_rate_bps_parts(bytes_per_sec);
    match tier {
        0 => MetricDisplay::from_static("0", " MB/s"),
        1 => MetricDisplay::new(format_u32_shared(val as u32), " B/s"),
        2 => MetricDisplay::new(format_tenths_shared(val), " KB/s"),
        3 => MetricDisplay::new(format_tenths_shared(val), " MB/s"),
        _ => MetricDisplay::new(format_tenths_shared(val), " GB/s"),
    }
}

/// Formats a network throughput (in bytes per second) into a split
/// [`MetricDisplay`] (`value` strictly numeric, `unit` in `{" bps", " Kbps", " Mbps", " Gbps"}`).
pub fn format_network_rate_mbps_metric(bytes_per_sec: u64) -> MetricDisplay {
    let (tier, val) = quantize_net_bps_parts(bytes_per_sec);
    match tier {
        0 => MetricDisplay::from_static("0", " Mbps"),
        1 => MetricDisplay::new(format_u32_shared(val as u32), " bps"),
        2 => MetricDisplay::new(format_tenths_shared(val), " Kbps"),
        3 => MetricDisplay::new(format_tenths_shared(val), " Mbps"),
        _ => MetricDisplay::new(format_tenths_shared(val), " Gbps"),
    }
}

/// Formats a memory table cell as a [`MetricDisplay`] according to [`ResourceDisplayMode`].
#[inline]
pub fn format_memory_cell_metric(
    bytes: u64,
    permille: u16,
    mode: ResourceDisplayMode,
) -> MetricDisplay {
    match mode {
        ResourceDisplayMode::Percent => format_permille_metric(permille),
        ResourceDisplayMode::Values => format_bytes_metric(bytes),
    }
}

/// Formats a disk throughput table cell as a [`MetricDisplay`] according to [`ResourceDisplayMode`].
#[inline]
pub fn format_disk_cell_metric(
    bytes_per_sec: u64,
    permille: u16,
    mode: ResourceDisplayMode,
) -> MetricDisplay {
    match mode {
        ResourceDisplayMode::Percent => format_permille_metric(permille),
        ResourceDisplayMode::Values => format_rate_bytes_per_sec_metric(bytes_per_sec),
    }
}

/// Formats a network throughput table cell as a [`MetricDisplay`] according to [`ResourceDisplayMode`].
#[inline]
pub fn format_network_cell_metric(
    bytes_per_sec: u64,
    permille: u16,
    mode: ResourceDisplayMode,
) -> MetricDisplay {
    match mode {
        ResourceDisplayMode::Percent => format_permille_metric(permille),
        ResourceDisplayMode::Values => format_network_rate_mbps_metric(bytes_per_sec),
    }
}

/// Produces a UTF-8 safe single-line preview capped at `max_chars` Unicode
/// scalar values, replacing any control characters (including `\r` and `\n`)
/// with spaces and appending `'…'` if truncated.
///
/// Returns `None` when `s` already has `<= max_chars` characters and contains
/// no control characters, allowing callers to reuse the original string without
/// allocating.
pub fn cap_single_line_preview(s: &str, max_chars: usize) -> Option<String> {
    let mut has_control = false;
    let mut truncated = false;
    let mut prefix_end = s.len();

    for (count, (byte_idx, ch)) in s.char_indices().enumerate() {
        if count == max_chars {
            truncated = true;
            prefix_end = byte_idx;
            break;
        }
        if ch.is_control() {
            has_control = true;
        }
    }

    if !truncated && !has_control {
        return None;
    }

    let prefix = &s[..prefix_end];
    let extra = if truncated { '…'.len_utf8() } else { 0 };
    let mut out = String::with_capacity(prefix.len() + extra);
    if has_control {
        for ch in prefix.chars() {
            if ch.is_control() {
                out.push(' ');
            } else {
                out.push(ch);
            }
        }
    } else {
        out.push_str(prefix);
    }
    if truncated {
        out.push('…');
    }
    Some(out)
}

/// Returns a single-line, length-capped [`SharedString`] preview of `arc`.
///
/// When `arc` fits within `max_chars` and contains no control characters,
/// returns `SharedString::from(Arc::clone(arc))` with zero heap allocation.
#[inline]
pub fn display_preview_shared(arc: &Arc<str>, max_chars: usize) -> SharedString {
    match cap_single_line_preview(arc, max_chars) {
        Some(capped) => SharedString::from(capped),
        None => SharedString::from(Arc::clone(arc)),
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

/// Formats an optional power draw in milliwatts as watts with one decimal place
/// (e.g., `42_500` -> `"42.5 W"`), or [`UNAVAILABLE_LABEL`] (`"—"`) when sleeping
/// or unavailable.
#[inline]
pub fn format_opt_power_mw(val: Option<u32>, sleeping: bool) -> SharedString {
    if sleeping {
        return SharedString::new_static(UNAVAILABLE_LABEL);
    }
    match val {
        Some(mw) if mw > 0 => {
            let tenths_w = (u64::from(mw) + 50) / 100;
            SharedString::from(format!("{}.{} W", tenths_w / 10, tenths_w % 10))
        }
        _ => SharedString::new_static(UNAVAILABLE_LABEL),
    }
}

/// Formats an optional fan speed percentage (`0..=100` -> `"45%"`), or
/// [`UNAVAILABLE_LABEL`] (`"—"`) when sleeping or unavailable.
#[inline]
pub fn format_opt_fan_percent(val: Option<u8>, sleeping: bool) -> SharedString {
    if sleeping {
        return SharedString::new_static(UNAVAILABLE_LABEL);
    }
    match val {
        Some(pct) if pct <= 100 => SharedString::from(format!("{pct}%")),
        _ => SharedString::new_static(UNAVAILABLE_LABEL),
    }
}

/// Formats an optional clock frequency in MHz (e.g., `2130` -> `"2130 MHz"`),
/// or [`UNAVAILABLE_LABEL`] (`"—"`) when sleeping or unavailable.
#[inline]
pub fn format_opt_clock_mhz(val: Option<u32>, sleeping: bool) -> SharedString {
    if sleeping {
        return SharedString::new_static(UNAVAILABLE_LABEL);
    }
    match val {
        Some(mhz) if mhz > 0 => SharedString::from(format!("{mhz} MHz")),
        _ => SharedString::new_static(UNAVAILABLE_LABEL),
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
