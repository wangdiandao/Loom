//! Monotonic and kernel timestamp primitives in 100-nanosecond ticks.

use serde::{Deserialize, Serialize};

/// A timestamp or duration measured in 100-nanosecond (`100 ns`) ticks.
///
/// This matches Windows kernel `FILETIME` / `LARGE_INTEGER` tick granularity
/// while remaining OS-independent and compact (`8` bytes, `Copy`).
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Timestamp100Ns(pub u64);

impl Timestamp100Ns {
    /// Number of 100-nanosecond ticks in one millisecond.
    pub const TICKS_PER_MILLI: u64 = 10_000;

    /// Number of 100-nanosecond ticks in one second.
    pub const TICKS_PER_SEC: u64 = 10_000_000;

    /// Creates a new timestamp from raw 100-nanosecond ticks.
    #[inline]
    pub const fn new(ticks: u64) -> Self {
        Self(ticks)
    }

    /// Returns the underlying raw 100-nanosecond tick count.
    #[inline]
    pub const fn as_raw(self) -> u64 {
        self.0
    }

    /// Computes `self - earlier`, saturating at `0` if `earlier > self`.
    #[inline]
    pub const fn saturating_sub(self, earlier: Self) -> Self {
        Self(self.0.saturating_sub(earlier.0))
    }

    /// Converts this value to whole milliseconds (truncating fractional milliseconds).
    #[inline]
    pub const fn as_millis(self) -> u64 {
        self.0 / Self::TICKS_PER_MILLI
    }

    /// Converts this value to seconds as a floating-point number.
    #[inline]
    pub fn as_secs_f64(self) -> f64 {
        self.0 as f64 / Self::TICKS_PER_SEC as f64
    }
}
