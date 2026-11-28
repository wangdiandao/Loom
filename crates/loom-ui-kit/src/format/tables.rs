//! Zero-allocation static and lazily-chunked lookup tables for hot-path metric strings.

use std::sync::OnceLock;

use gpui_kit::SharedString;

use super::metrics::format_permille;

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
pub(super) const fn div_round_ties_even_u128(num: u128, den: u128) -> u64 {
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
/// Values in `0..=1024` ([`SMALL_U32_TABLE_MAX`]) clone from a pre-initialized
/// lookup table; larger values format dynamically.
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
/// Values in `0..=256` ([`CHILD_COUNT_TABLE_MAX`]) clone from a pre-initialized
/// lookup table; larger counts format dynamically.
#[inline]
pub fn format_child_count_shared(count: u32) -> SharedString {
    if count <= CHILD_COUNT_TABLE_MAX {
        child_count_table()[count as usize].clone()
    } else {
        format!("({count})").into()
    }
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
pub(super) fn format_signed_u32_shared(positive: bool, mag: u64) -> SharedString {
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
pub(super) fn format_signed_tenths_shared(positive: bool, tenths: u64) -> SharedString {
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
