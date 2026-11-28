//! Scroll-burst snapshot coalescer and deterministic 64-bit FNV-1a viewport fingerprint hasher.

use std::time::{Duration, Instant};

/// Default quiet period after the latest scroll tick before deferred snapshots flush (`150 ms`).
pub(crate) const DEFAULT_SCROLL_QUIET_WINDOW: Duration = Duration::from_millis(150);

/// Default upper bound on snapshot deferral during continuous scrolling (`500 ms`).
pub(crate) const DEFAULT_MAX_DEFER_WINDOW: Duration = Duration::from_millis(500);

/// Pure, deterministic state machine that defers periodic live snapshot notifications
/// during active table scroll bursts so `gpui-fast`'s GPU scroll layers remain intact.
///
/// User-initiated updates (search filter, sort change, view mode change, expand/collapse,
/// or jump-to-row navigation) always bypass deferral immediately.
#[derive(Debug, Clone)]
pub(crate) struct ScrollBurstCoalescer {
    scroll_quiet_window: Duration,
    max_defer_window: Duration,
    last_scroll_at: Option<Instant>,
    first_deferred_at: Option<Instant>,
    has_pending: bool,
}

impl Default for ScrollBurstCoalescer {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl ScrollBurstCoalescer {
    /// Creates a [`ScrollBurstCoalescer`] with the default `150 ms` quiet window and
    /// `500 ms` maximum deferral cap.
    #[inline]
    pub(crate) const fn new() -> Self {
        Self::with_windows(DEFAULT_SCROLL_QUIET_WINDOW, DEFAULT_MAX_DEFER_WINDOW)
    }

    /// Creates a [`ScrollBurstCoalescer`] with custom quiet and maximum deferral windows.
    #[inline]
    pub(crate) const fn with_windows(
        scroll_quiet_window: Duration,
        max_defer_window: Duration,
    ) -> Self {
        Self {
            scroll_quiet_window,
            max_defer_window,
            last_scroll_at: None,
            first_deferred_at: None,
            has_pending: false,
        }
    }

    /// Records a scroll event at `now`.
    #[inline]
    pub(crate) fn on_scroll(&mut self, now: Instant) {
        self.last_scroll_at = Some(now);
    }

    /// Returns `true` if a live snapshot arriving at `now` should be deferred until scrolling
    /// settles or the maximum deferral window elapses.
    ///
    /// User-initiated updates (`is_user_initiated == true`) clear any pending deferred state
    /// and always return `false`.
    pub(crate) fn should_defer_snapshot(&mut self, now: Instant, is_user_initiated: bool) -> bool {
        if is_user_initiated {
            self.has_pending = false;
            self.first_deferred_at = None;
            return false;
        }

        if let Some(last_scroll) = self.last_scroll_at
            && now.saturating_duration_since(last_scroll) < self.scroll_quiet_window
        {
            if let Some(first_deferred) = self.first_deferred_at
                && now.saturating_duration_since(first_deferred) >= self.max_defer_window
            {
                self.has_pending = false;
                self.first_deferred_at = None;
                return false;
            }
            self.has_pending = true;
            self.first_deferred_at.get_or_insert(now);
            return true;
        }

        self.has_pending = false;
        self.first_deferred_at = None;
        false
    }

    /// Returns how long a flush timer should wait before applying the pending snapshot,
    /// bounded by both the scroll quiet window and the maximum deferral cap.
    ///
    /// Returns `None` if no snapshot is currently pending.
    pub(crate) fn remaining_quiet_delay(&self, now: Instant) -> Option<Duration> {
        if !self.has_pending {
            return None;
        }
        let quiet_rem = self
            .last_scroll_at
            .map(|last| {
                self.scroll_quiet_window
                    .saturating_sub(now.saturating_duration_since(last))
            })
            .unwrap_or(Duration::ZERO);
        let cap_rem = self
            .first_deferred_at
            .map(|first| {
                self.max_defer_window
                    .saturating_sub(now.saturating_duration_since(first))
            })
            .unwrap_or(quiet_rem);
        Some(quiet_rem.min(cap_rem))
    }

    /// Clears deferred state and returns whether a pending snapshot existed.
    #[inline]
    pub(crate) fn take_pending(&mut self) -> bool {
        let had = self.has_pending;
        self.has_pending = false;
        self.first_deferred_at = None;
        had
    }
}

/// Fast, deterministic 64-bit FNV-1a hasher for computing table viewport fingerprints
/// without `RandomState` initialization or heap allocations.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FnvHasher64(u64);

impl Default for FnvHasher64 {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl FnvHasher64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    /// Creates a new [`FnvHasher64`] initialized with the 64-bit FNV-1a offset basis.
    #[inline]
    pub(crate) const fn new() -> Self {
        Self(Self::OFFSET_BASIS)
    }

    /// Mixes a byte slice into the hash state.
    #[inline]
    pub(crate) fn write_bytes(&mut self, bytes: &[u8]) {
        let mut hash = self.0;
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(Self::PRIME);
        }
        self.0 = hash;
    }

    /// Mixes a single `u8` into the hash state.
    #[inline]
    pub(crate) fn write_u8(&mut self, v: u8) {
        self.0 ^= u64::from(v);
        self.0 = self.0.wrapping_mul(Self::PRIME);
    }

    /// Mixes a `bool` into the hash state.
    #[inline]
    pub(crate) fn write_bool(&mut self, v: bool) {
        self.write_u8(u8::from(v));
    }

    /// Mixes a `u16` into the hash state.
    #[inline]
    pub(crate) fn write_u16(&mut self, v: u16) {
        self.write_bytes(&v.to_le_bytes());
    }

    /// Mixes a `u32` into the hash state.
    #[inline]
    pub(crate) fn write_u32(&mut self, v: u32) {
        self.write_bytes(&v.to_le_bytes());
    }

    /// Mixes a `u64` into the hash state.
    #[inline]
    pub(crate) fn write_u64(&mut self, v: u64) {
        self.write_bytes(&v.to_le_bytes());
    }

    /// Mixes a `usize` into the hash state.
    #[inline]
    pub(crate) fn write_usize(&mut self, v: usize) {
        self.write_u64(v as u64);
    }

    /// Mixes a length-delimited string slice into the hash state.
    #[inline]
    pub(crate) fn write_str(&mut self, s: &str) {
        self.write_usize(s.len());
        self.write_bytes(s.as_bytes());
    }

    /// Mixes a bounded head+tail sample of a potentially multi-kilobyte string (e.g. command line)
    /// into the hash state in `O(1)` time.
    #[inline]
    pub(crate) fn write_preview_str(&mut self, s: &str) {
        let bytes = s.as_bytes();
        self.write_usize(bytes.len());
        if bytes.len() <= 320 {
            self.write_bytes(bytes);
        } else {
            self.write_bytes(&bytes[..256]);
            self.write_bytes(&bytes[bytes.len() - 64..]);
        }
    }

    /// Returns the final 64-bit fingerprint.
    #[inline]
    pub(crate) const fn finish(self) -> u64 {
        self.0
    }
}
