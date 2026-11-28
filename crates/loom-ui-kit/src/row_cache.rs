//! Bounded per-process formatted text cache inspired by System Informer's `TextCache`.
//!
//! High-frequency tables (Processes, Details, Users) re-render visible rows on
//! every snapshot tick and scroll event, while most individual cells retain the
//! same formatted text across frames (e.g., PID never changes, CPU time changes
//! once per second, and memory/I/O metrics stay inside the same display tenth).
//! [`RowTextCache`] keys cached [`SharedString`] and [`MetricDisplay`] values by
//! [`ProcessKey`] and quantized display keys so unchanged cells reuse existing
//! reference-counted strings with zero heap allocation or string scanning.

use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::SharedString;
use loom_types::{ProcessKey, ResourceDisplayMode};

use crate::format::{
    MetricDisplay, SMALL_U32_TABLE_MAX, display_preview_shared, format_bytes_metric,
    format_cpu_time_shared, format_disk_cell_metric, format_memory_cell_metric,
    format_network_cell_metric, format_signed_bytes_metric, format_u32_shared,
    quantize_bytes_display, quantize_cpu_time_secs, quantize_net_bps_display,
    quantize_rate_bps_display, quantize_signed_bytes_display,
};

/// Default maximum number of [`ProcessKey`] entries retained in [`RowTextCache`].
pub const DEFAULT_ROW_TEXT_CACHE_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MetricCacheKey {
    mode: ResourceDisplayMode,
    quantized: u64,
}

#[derive(Debug, Clone)]
struct PreviewSlotEntry {
    slot: u8,
    source: Arc<str>,
    max_chars: usize,
    preview: SharedString,
}

#[derive(Debug, Default, Clone)]
struct RowEntry {
    last_epoch: u64,
    pid: Option<SharedString>,
    memory: Option<(MetricCacheKey, MetricDisplay)>,
    disk: Option<(MetricCacheKey, MetricDisplay)>,
    network: Option<(MetricCacheKey, MetricDisplay)>,
    cpu_time: Option<(u64, SharedString)>,
    bytes_slots: Vec<(u8, u64, MetricDisplay)>,
    signed_bytes_slots: Vec<(u8, u64, MetricDisplay)>,
    u32_slots: Vec<(u8, u32, SharedString)>,
    preview_slots: Vec<PreviewSlotEntry>,
}

/// Bounded per-process text and [`MetricDisplay`] cache with epoch-based eviction.
///
/// Typical frame lifecycle inside a GPUI table delegate:
/// 1. Call [`Self::begin_epoch`] when starting a new snapshot / render cycle.
/// 2. Query cached cell strings (`pid_text`, `memory_metric`, `disk_metric`,
///    `network_metric`, `bytes_metric_slot`, `cpu_time_text`, `u32_slot`,
///    `preview_slot`) for visible rows.
/// 3. Call [`Self::prune_if_needed`] to evict the oldest untouched entries when
///    the cache grows past its configured capacity.
#[derive(Debug, Clone)]
pub struct RowTextCache {
    entries: HashMap<ProcessKey, RowEntry>,
    max_entries: usize,
    epoch: u64,
}

impl Default for RowTextCache {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl RowTextCache {
    /// Creates a new [`RowTextCache`] with [`DEFAULT_ROW_TEXT_CACHE_CAPACITY`] (`1024`) entries.
    #[inline]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_ROW_TEXT_CACHE_CAPACITY)
    }

    /// Creates a new [`RowTextCache`] bounded to `max_entries` process entries.
    #[inline]
    pub fn with_capacity(max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_entries,
            epoch: 0,
        }
    }

    /// Advances the monotonic cache epoch used to track recently accessed rows.
    #[inline]
    pub fn begin_epoch(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// Returns the current monotonic epoch counter.
    #[inline]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Returns the number of currently cached [`ProcessKey`] entries.
    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if no entries are currently cached.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the configured maximum number of retained entries.
    #[inline]
    pub const fn capacity(&self) -> usize {
        self.max_entries
    }

    /// Clears all cached entries without resetting the monotonic epoch.
    #[inline]
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Evicts the least-recently-used entries (by `last_epoch`) whenever the
    /// number of cached rows exceeds [`Self::capacity`].
    pub fn prune_if_needed(&mut self) {
        if self.entries.len() <= self.max_entries {
            return;
        }
        if self.max_entries == 0 {
            self.entries.clear();
            return;
        }

        let excess = self.entries.len() - self.max_entries;
        let mut ages: Vec<(u64, ProcessKey)> = self
            .entries
            .iter()
            .map(|(&key, entry)| (entry.last_epoch, key))
            .collect();
        ages.select_nth_unstable_by_key(excess - 1, |&(last_epoch, _)| last_epoch);
        for &(_, key) in &ages[..excess] {
            self.entries.remove(&key);
        }
    }

    #[inline]
    fn entry_mut(&mut self, key: ProcessKey) -> &mut RowEntry {
        // Guard against unbounded memory growth if a caller forgets to invoke
        // `prune_if_needed` over a long session.
        let hard_limit = self.max_entries.saturating_mul(2).max(16);
        if self.entries.len() >= hard_limit && !self.entries.contains_key(&key) {
            self.prune_if_needed();
        }
        let epoch = self.epoch;
        let entry = self.entries.entry(key).or_default();
        entry.last_epoch = epoch;
        entry
    }

    /// Returns the cached PID text for `key`, formatting at most once per process lifetime.
    pub fn pid_text(&mut self, key: ProcessKey) -> SharedString {
        let entry = self.entry_mut(key);
        entry
            .pid
            .get_or_insert_with(|| format_u32_shared(key.pid))
            .clone()
    }

    /// Returns the cached memory [`MetricDisplay`] for `key`, reformatting only
    /// when `mode` or the display-quantized value changes.
    pub fn memory_metric(
        &mut self,
        key: ProcessKey,
        bytes: u64,
        permille: u16,
        mode: ResourceDisplayMode,
    ) -> MetricDisplay {
        let cache_key = MetricCacheKey {
            mode,
            quantized: match mode {
                ResourceDisplayMode::Percent => u64::from(permille),
                ResourceDisplayMode::Values => quantize_bytes_display(bytes),
            },
        };
        let entry = self.entry_mut(key);
        if let Some((prev_key, cached)) = &entry.memory
            && *prev_key == cache_key
        {
            return cached.clone();
        }
        let formatted = format_memory_cell_metric(bytes, permille, mode);
        entry.memory = Some((cache_key, formatted.clone()));
        formatted
    }

    /// Returns the cached disk throughput [`MetricDisplay`] for `key`,
    /// reformatting only when `mode` or the display-quantized value changes.
    pub fn disk_metric(
        &mut self,
        key: ProcessKey,
        bps: u64,
        permille: u16,
        mode: ResourceDisplayMode,
    ) -> MetricDisplay {
        let cache_key = MetricCacheKey {
            mode,
            quantized: match mode {
                ResourceDisplayMode::Percent => u64::from(permille),
                ResourceDisplayMode::Values => quantize_rate_bps_display(bps),
            },
        };
        let entry = self.entry_mut(key);
        if let Some((prev_key, cached)) = &entry.disk
            && *prev_key == cache_key
        {
            return cached.clone();
        }
        let formatted = format_disk_cell_metric(bps, permille, mode);
        entry.disk = Some((cache_key, formatted.clone()));
        formatted
    }

    /// Returns the cached network throughput [`MetricDisplay`] for `key`,
    /// reformatting only when `mode` or the display-quantized value changes.
    pub fn network_metric(
        &mut self,
        key: ProcessKey,
        bps: u64,
        permille: u16,
        mode: ResourceDisplayMode,
    ) -> MetricDisplay {
        let cache_key = MetricCacheKey {
            mode,
            quantized: match mode {
                ResourceDisplayMode::Percent => u64::from(permille),
                ResourceDisplayMode::Values => quantize_net_bps_display(bps),
            },
        };
        let entry = self.entry_mut(key);
        if let Some((prev_key, cached)) = &entry.network
            && *prev_key == cache_key
        {
            return cached.clone();
        }
        let formatted = format_network_cell_metric(bps, permille, mode);
        entry.network = Some((cache_key, formatted.clone()));
        formatted
    }

    /// Returns the cached byte-size [`MetricDisplay`] for a numbered column
    /// `slot` (e.g., Private Working Set, Working Set, Commit, GPU Dedicated),
    /// reformatting only when the display-quantized byte bucket changes.
    pub fn bytes_metric_slot(&mut self, key: ProcessKey, slot: u8, bytes: u64) -> MetricDisplay {
        let quantized = quantize_bytes_display(bytes);
        let entry = self.entry_mut(key);
        if let Some((_, prev_q, cached)) = entry
            .bytes_slots
            .iter_mut()
            .find(|(existing_slot, _, _)| *existing_slot == slot)
        {
            if *prev_q == quantized {
                return cached.clone();
            }
            let formatted = format_bytes_metric(bytes);
            *prev_q = quantized;
            *cached = formatted.clone();
            return formatted;
        }
        let formatted = format_bytes_metric(bytes);
        entry.bytes_slots.push((slot, quantized, formatted.clone()));
        formatted
    }

    /// Returns the cached signed byte-delta [`MetricDisplay`] for a numbered
    /// column `slot` (e.g., Working Set Delta), returning a zero-allocation
    /// static `"0 B"` on zero deltas and reformatting non-zero deltas only when
    /// the display-quantized sign/bucket changes.
    pub fn signed_bytes_metric_slot(
        &mut self,
        key: ProcessKey,
        slot: u8,
        delta_bytes: i64,
    ) -> MetricDisplay {
        if delta_bytes == 0 {
            if let Some(entry) = self.entries.get_mut(&key) {
                entry.last_epoch = self.epoch;
            }
            return MetricDisplay::from_static("0", " B");
        }

        let quantized = quantize_signed_bytes_display(delta_bytes);
        let entry = self.entry_mut(key);
        if let Some((_, prev_q, cached)) = entry
            .signed_bytes_slots
            .iter_mut()
            .find(|(existing_slot, _, _)| *existing_slot == slot)
        {
            if *prev_q == quantized {
                return cached.clone();
            }
            let formatted = format_signed_bytes_metric(delta_bytes);
            *prev_q = quantized;
            *cached = formatted.clone();
            return formatted;
        }
        let formatted = format_signed_bytes_metric(delta_bytes);
        entry
            .signed_bytes_slots
            .push((slot, quantized, formatted.clone()));
        formatted
    }

    /// Returns the cached `"HH:MM:SS"` cumulative CPU time string for `key`,
    /// reformatting only when whole seconds (`ticks_100ns / 10_000_000`) change.
    pub fn cpu_time_text(&mut self, key: ProcessKey, ticks_100ns: u64) -> SharedString {
        let secs = quantize_cpu_time_secs(ticks_100ns);
        let entry = self.entry_mut(key);
        if let Some((prev_secs, cached)) = &entry.cpu_time
            && *prev_secs == secs
        {
            return cached.clone();
        }
        let formatted = format_cpu_time_shared(ticks_100ns);
        entry.cpu_time = Some((secs, formatted.clone()));
        formatted
    }

    /// Returns a formatted `u32` cell string for a numbered column `slot`
    /// (e.g., Handles, Threads, Session ID).
    ///
    /// Values `<= 1024` ([`SMALL_U32_TABLE_MAX`]) return directly from the static
    /// zero-allocation lookup table; larger values are cached per `(key, slot)`.
    pub fn u32_slot(&mut self, key: ProcessKey, slot: u8, val: u32) -> SharedString {
        if val <= SMALL_U32_TABLE_MAX {
            if let Some(entry) = self.entries.get_mut(&key) {
                entry.last_epoch = self.epoch;
            }
            return format_u32_shared(val);
        }

        let entry = self.entry_mut(key);
        if let Some((_, prev_val, cached)) = entry
            .u32_slots
            .iter_mut()
            .find(|(existing_slot, _, _)| *existing_slot == slot)
        {
            if *prev_val == val {
                return cached.clone();
            }
            let formatted = format_u32_shared(val);
            *prev_val = val;
            *cached = formatted.clone();
            return formatted;
        }
        let formatted = format_u32_shared(val);
        entry.u32_slots.push((slot, val, formatted.clone()));
        formatted
    }

    /// Returns a single-line, length-capped preview [`SharedString`] for a
    /// numbered text `slot` (e.g., command line, executable path, description).
    ///
    /// Uses pointer + length identity (`Arc::ptr_eq`) together with `max_chars`
    /// so repeated ticks with the same `Arc<str>` reuse the cached preview with
    /// zero UTF-8 re-scanning or allocation.
    pub fn preview_slot(
        &mut self,
        key: ProcessKey,
        slot: u8,
        source: &Arc<str>,
        max_chars: usize,
    ) -> SharedString {
        let entry = self.entry_mut(key);
        if let Some(existing) = entry.preview_slots.iter_mut().find(|s| s.slot == slot) {
            if Arc::ptr_eq(&existing.source, source) && existing.max_chars == max_chars {
                return existing.preview.clone();
            }
            let preview = display_preview_shared(source, max_chars);
            existing.source = Arc::clone(source);
            existing.max_chars = max_chars;
            existing.preview = preview.clone();
            return preview;
        }

        let preview = display_preview_shared(source, max_chars);
        entry.preview_slots.push(PreviewSlotEntry {
            slot,
            source: Arc::clone(source),
            max_chars,
            preview: preview.clone(),
        });
        preview
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use loom_types::{ProcessKey, ResourceDisplayMode};

    use crate::format::MetricDisplay;

    use super::RowTextCache;

    #[test]
    fn row_text_cache_hits_reuses_invalidates_and_evicts_oldest_epochs() {
        const MIB: u64 = 1024 * 1024;
        let p1 = ProcessKey::new(4096, 100);
        let p1_recycled = ProcessKey::new(4096, 200);
        let p2 = ProcessKey::new(8192, 150);
        let p3 = ProcessKey::new(9999, 250);

        let mut cache = RowTextCache::with_capacity(2);
        assert!(cache.is_empty());
        assert_eq!(cache.capacity(), 2);

        cache.begin_epoch();
        assert_eq!(cache.epoch(), 1);

        let pid_a = cache.pid_text(p1);
        let pid_b = cache.pid_text(p1);
        assert_eq!(pid_a.as_ref(), "4096");
        assert_eq!(pid_a, pid_b);

        let mem1 = cache.memory_metric(p1, 128 * MIB, 42, ResourceDisplayMode::Values);
        let mem2 = cache.memory_metric(p1, 128 * MIB + 64, 42, ResourceDisplayMode::Values);
        assert_eq!(mem1, MetricDisplay::new("128.0", " MB"));
        assert_eq!(mem1, mem2);

        let mem_pct = cache.memory_metric(p1, 128 * MIB + 64, 42, ResourceDisplayMode::Percent);
        assert_eq!(mem_pct, MetricDisplay::new("4.2%", ""));

        let d1 = cache.disk_metric(p1, 2 * MIB, 10, ResourceDisplayMode::Values);
        let d2 = cache.disk_metric(p1, 2 * MIB + 16, 10, ResourceDisplayMode::Values);
        assert_eq!(d1, MetricDisplay::new("2.0", " MB/s"));
        assert_eq!(d1, d2);

        let n1 = cache.network_metric(p1, 1_550_000, 124, ResourceDisplayMode::Values);
        let n2 = cache.network_metric(p1, 1_550_050, 124, ResourceDisplayMode::Values);
        assert_eq!(n1, MetricDisplay::new("12.4", " Mbps"));
        assert_eq!(n1, n2);

        let ws1 = cache.bytes_metric_slot(p1, 0, 64 * MIB);
        let commit1 = cache.bytes_metric_slot(p1, 1, 256 * MIB);
        let ws2 = cache.bytes_metric_slot(p1, 0, 64 * MIB + 10);
        assert_eq!(ws1, MetricDisplay::new("64.0", " MB"));
        assert_eq!(commit1, MetricDisplay::new("256.0", " MB"));
        assert_eq!(ws1, ws2);

        let d0 = cache.signed_bytes_metric_slot(p1, 0, 0);
        let d_pos1 = cache.signed_bytes_metric_slot(p1, 0, (4 * MIB) as i64);
        let d_pos2 = cache.signed_bytes_metric_slot(p1, 0, (4 * MIB) as i64 + 16);
        let d_neg = cache.signed_bytes_metric_slot(p1, 0, -((4 * MIB) as i64));
        assert_eq!(d0, MetricDisplay::from_static("0", " B"));
        assert_eq!(d_pos1, MetricDisplay::new("+4.0", " MB"));
        assert_eq!(d_pos1, d_pos2);
        assert_eq!(d_neg, MetricDisplay::new("-4.0", " MB"));

        let t1 = cache.cpu_time_text(p1, 10_000_000);
        let t2 = cache.cpu_time_text(p1, 19_999_999);
        let t3 = cache.cpu_time_text(p1, 20_000_000);
        assert_eq!(t1.as_ref(), "00:00:01");
        assert_eq!(t1, t2);
        assert_eq!(t3.as_ref(), "00:00:02");

        let small = cache.u32_slot(p1, 0, 42);
        assert_eq!(small.as_ref(), "42");
        let big1 = cache.u32_slot(p1, 0, 12_345);
        let big2 = cache.u32_slot(p1, 0, 12_345);
        let big3 = cache.u32_slot(p1, 0, 12_346);
        assert_eq!(big1.as_ref(), "12345");
        assert_eq!(big1, big2);
        assert_eq!(big3.as_ref(), "12346");

        let long_cmd: Arc<str> = Arc::from(format!("cmd.exe --arg={}", "a".repeat(300)));
        let prev1 = cache.preview_slot(p1, 0, &long_cmd, 48);
        let prev2 = cache.preview_slot(p1, 0, &long_cmd, 48);
        assert_eq!(prev1.chars().count(), 49);
        assert!(prev1.ends_with('…'));
        assert_eq!(prev1.as_ptr(), prev2.as_ptr());

        let prev_longer = cache.preview_slot(p1, 0, &long_cmd, 64);
        assert_eq!(prev_longer.chars().count(), 65);
        let short_cmd: Arc<str> = Arc::from("C:\\Windows\\System32\\notepad.exe --launch");
        let prev_short = cache.preview_slot(p1, 0, &short_cmd, 64);
        assert_eq!(prev_short.as_ptr(), short_cmd.as_ptr());

        cache.begin_epoch();
        let _ = cache.pid_text(p2);
        let _ = cache.pid_text(p1_recycled);

        cache.begin_epoch();
        let _ = cache.pid_text(p3);
        let _ = cache.pid_text(p1_recycled);

        assert_eq!(cache.len(), 4);
        cache.prune_if_needed();
        assert_eq!(cache.len(), 2);

        let p3_again = cache.pid_text(p3);
        assert_eq!(p3_again.as_ref(), "9999");
        assert_eq!(cache.len(), 2);

        cache.clear();
        assert!(cache.is_empty());
    }
}
