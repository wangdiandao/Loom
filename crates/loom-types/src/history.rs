//! Fixed-capacity, stack-allocated quantized history ring buffers.

/// A fixed-size `Copy` ring buffer storing quantized permille values (`0..=1000`).
///
/// Each entry represents a percentage with `0.1%` precision (`0` = `0.0%`,
/// `1000` = `100.0%`). Because the storage is an inline `[u16; N]` array,
/// cloning or snapshotting a `HistoryRing<60>` requires no heap allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryRing<const N: usize> {
    buf: [u16; N],
    head: usize,
    len: usize,
}

impl<const N: usize> Default for HistoryRing<N> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> HistoryRing<N> {
    /// Maximum representable quantized permille value (`100.0%`).
    pub const MAX_PERMILLE: u16 = 1000;

    /// Sentinel value marking a sampling discontinuity / sleep gap in the ring buffer.
    ///
    /// Distinct from all valid permille samples (`0..=1000`) while keeping the ring
    /// `Copy` with identical inline `[u16; N]` storage layout.
    pub const GAP_SENTINEL: u16 = u16::MAX;

    /// Creates an empty ring buffer.
    #[inline]
    pub const fn new() -> Self {
        Self {
            buf: [0; N],
            head: 0,
            len: 0,
        }
    }

    /// Returns the fixed capacity `N` of the ring buffer.
    #[inline]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Returns `true` if `raw` is the [`Self::GAP_SENTINEL`] discontinuity marker.
    #[inline]
    pub const fn is_gap(raw: u16) -> bool {
        raw == Self::GAP_SENTINEL
    }

    /// Pushes a quantized permille value (`0..=1000`), clamping values above `1000`.
    ///
    /// When the buffer is already full, the oldest entry is overwritten.
    #[inline]
    pub fn push(&mut self, value_permille: u16) {
        if N == 0 {
            return;
        }
        self.buf[self.head] = value_permille.min(Self::MAX_PERMILLE);
        self.head = (self.head + 1) % N;
        if self.len < N {
            self.len += 1;
        }
    }

    /// Pushes a [`Self::GAP_SENTINEL`] marker recording a sampling discontinuity
    /// (such as system suspend/resume or paused collection).
    #[inline]
    pub fn push_gap(&mut self) {
        if N == 0 {
            return;
        }
        self.buf[self.head] = Self::GAP_SENTINEL;
        self.head = (self.head + 1) % N;
        if self.len < N {
            self.len += 1;
        }
    }

    /// Returns the number of samples currently stored (`0..=N`).
    #[inline]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if no samples have been pushed.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Clears all stored samples without zeroing unused capacity unnecessarily.
    #[inline]
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// Returns the most recently pushed value, or `None` when empty.
    #[inline]
    pub fn latest(&self) -> Option<u16> {
        if self.len == 0 || N == 0 {
            return None;
        }
        let idx = if self.head == 0 { N - 1 } else { self.head - 1 };
        Some(self.buf[idx])
    }

    /// Returns an iterator yielding stored values from oldest to newest.
    #[inline]
    pub fn iter_ordered(&self) -> HistoryRingIter<'_, N> {
        let start = if self.len == N && N > 0 { self.head } else { 0 };
        HistoryRingIter {
            ring: self,
            start,
            offset: 0,
            remaining: self.len,
        }
    }

    /// Converts the stored samples (oldest to newest) into floating-point percentages (`0.0..=100.0`).
    ///
    /// Gap sentinels are mapped to `0.0` for backward compatibility with callers expecting
    /// dense numeric slices; use [`Self::to_vec_opt_f32`] to preserve gap discontinuities.
    pub fn to_vec_f32(&self) -> Vec<f32> {
        self.iter_ordered()
            .map(|v| {
                if Self::is_gap(v) {
                    0.0
                } else {
                    f32::from(v) / 10.0
                }
            })
            .collect()
    }

    /// Converts the stored samples (oldest to newest) into optional floating-point
    /// percentages (`Some(0.0..=100.0)`), mapping [`Self::GAP_SENTINEL`] entries to `None`.
    pub fn to_vec_opt_f32(&self) -> Vec<Option<f32>> {
        self.iter_ordered()
            .map(|v| (!Self::is_gap(v)).then(|| f32::from(v) / 10.0))
            .collect()
    }
}

/// Ordered iterator (oldest to newest) over a [`HistoryRing`].
#[derive(Debug, Clone)]
pub struct HistoryRingIter<'a, const N: usize> {
    ring: &'a HistoryRing<N>,
    start: usize,
    offset: usize,
    remaining: usize,
}

impl<const N: usize> Iterator for HistoryRingIter<'_, N> {
    type Item = u16;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 || N == 0 {
            return None;
        }
        let idx = (self.start + self.offset) % N;
        self.offset += 1;
        self.remaining -= 1;
        Some(self.ring.buf[idx])
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<const N: usize> ExactSizeIterator for HistoryRingIter<'_, N> {
    #[inline]
    fn len(&self) -> usize {
        self.remaining
    }
}

/// A fixed-size `Copy` ring buffer storing raw `u64` throughput values (e.g., bytes/sec).
///
/// Backed by an inline `[u64; N]` array so snapshotting 60-point disk or network
/// throughput histories requires no extra heap allocations per ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThroughputRing<const N: usize> {
    buf: [u64; N],
    head: usize,
    len: usize,
}

impl<const N: usize> Default for ThroughputRing<N> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> ThroughputRing<N> {
    /// Sentinel value marking a sampling discontinuity / sleep gap in the ring buffer.
    ///
    /// Preserves `Copy` semantics and exact `[u64; N]` inline layout without wrapping
    /// each element in `Option<u64>`.
    pub const GAP_SENTINEL: u64 = u64::MAX;

    /// Creates an empty throughput ring buffer.
    #[inline]
    pub const fn new() -> Self {
        Self {
            buf: [0; N],
            head: 0,
            len: 0,
        }
    }

    /// Returns the fixed capacity `N` of the ring buffer.
    #[inline]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Returns `true` if `raw` is the [`Self::GAP_SENTINEL`] discontinuity marker.
    #[inline]
    pub const fn is_gap(raw: u64) -> bool {
        raw == Self::GAP_SENTINEL
    }

    /// Pushes a `u64` throughput sample, overwriting the oldest entry when full.
    ///
    /// Values equal to [`Self::GAP_SENTINEL`] (`u64::MAX`) are clamped to `u64::MAX - 1`
    /// so real throughput samples never collide with the gap marker.
    #[inline]
    pub fn push(&mut self, value: u64) {
        if N == 0 {
            return;
        }
        self.buf[self.head] = value.min(Self::GAP_SENTINEL - 1);
        self.head = (self.head + 1) % N;
        if self.len < N {
            self.len += 1;
        }
    }

    /// Pushes a [`Self::GAP_SENTINEL`] marker recording a sampling discontinuity.
    #[inline]
    pub fn push_gap(&mut self) {
        if N == 0 {
            return;
        }
        self.buf[self.head] = Self::GAP_SENTINEL;
        self.head = (self.head + 1) % N;
        if self.len < N {
            self.len += 1;
        }
    }

    /// Returns the number of samples currently stored (`0..=N`).
    #[inline]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if no samples have been pushed.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Clears all stored samples.
    #[inline]
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// Returns the most recently pushed value, or `None` when empty.
    #[inline]
    pub fn latest(&self) -> Option<u64> {
        if self.len == 0 || N == 0 {
            return None;
        }
        let idx = if self.head == 0 { N - 1 } else { self.head - 1 };
        Some(self.buf[idx])
    }

    /// Returns an ordered iterator yielding stored values from oldest to newest.
    #[inline]
    pub fn iter(&self) -> ThroughputRingIter<'_, N> {
        let start = if self.len == N && N > 0 { self.head } else { 0 };
        ThroughputRingIter {
            ring: self,
            start,
            offset: 0,
            remaining: self.len,
        }
    }

    /// Collects stored samples (oldest to newest) into a `Vec<u64>`.
    pub fn to_vec(&self) -> Vec<u64> {
        self.iter().collect()
    }

    /// Collects stored samples (oldest to newest) into a `Vec<Option<u64>>`,
    /// mapping [`Self::GAP_SENTINEL`] entries to `None`.
    pub fn to_vec_opt(&self) -> Vec<Option<u64>> {
        self.iter()
            .map(|v| (!Self::is_gap(v)).then_some(v))
            .collect()
    }

    /// Returns the maximum non-gap value currently stored in the ring, or `0` when empty
    /// or containing only gap sentinels.
    #[inline]
    pub fn max_value(&self) -> u64 {
        self.iter().filter(|&v| !Self::is_gap(v)).max().unwrap_or(0)
    }
}

/// Ordered iterator (oldest to newest) over a [`ThroughputRing`].
#[derive(Debug, Clone)]
pub struct ThroughputRingIter<'a, const N: usize> {
    ring: &'a ThroughputRing<N>,
    start: usize,
    offset: usize,
    remaining: usize,
}

impl<const N: usize> Iterator for ThroughputRingIter<'_, N> {
    type Item = u64;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 || N == 0 {
            return None;
        }
        let idx = (self.start + self.offset) % N;
        self.offset += 1;
        self.remaining -= 1;
        Some(self.ring.buf[idx])
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<const N: usize> ExactSizeIterator for ThroughputRingIter<'_, N> {
    #[inline]
    fn len(&self) -> usize {
        self.remaining
    }
}
