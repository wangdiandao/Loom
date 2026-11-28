//! Windows slow-lane physical disk telemetry via `DeviceIoControl` (`IOCTL_DISK_PERFORMANCE`,
//! `IOCTL_STORAGE_QUERY_PROPERTY`, `IOCTL_DISK_GET_LENGTH_INFO`, `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`)
//! guarded by a per-device `300 ms` [`CircuitBreaker`] and mandatory `DRIVE_REMOTE` bypass.

mod ioctl;
mod safety;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Instant;

use loom_types::{DiskDeviceSample, DiskMediaType};
use parking_lot::Mutex;
use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDriveStringsW};
use windows_sys::Win32::System::Ioctl::DISK_PERFORMANCE;
use windows_sys::Win32::System::WindowsProgramming::{DRIVE_CDROM, DRIVE_REMOTE};

use crate::circuit_breaker::CircuitBreaker;

use self::ioctl::{
    StaticDiskInfo, open_device_metadata, query_disk_performance, query_static_disk_info,
    query_volume_disk_extents, query_volumes_perf,
};
use self::safety::HddActivityGuard;

/// Maximum physical disk indices scanned beyond those discovered via logical volumes.
const MAX_EXTRA_PHYSICAL_DISKS: u32 = 16;
/// Stop scanning extra physical disk indices after this many consecutive misses.
const MAX_CONSECUTIVE_MISSES: u32 = 4;

/// Previous `DISK_PERFORMANCE` counters for per-disk delta calculation.
#[derive(Debug, Clone, Copy)]
pub(super) struct PrevDiskCounters {
    bytes_read: i64,
    bytes_written: i64,
    read_time_100ns: i64,
    write_time_100ns: i64,
    idle_time_100ns: i64,
    read_count: u32,
    write_count: u32,
    query_time_100ns: i64,
    sampled_at: Instant,
}

/// Stateful physical disk prober owned by [`crate::WindowsSlowProber`].
#[derive(Debug, Default)]
pub(crate) struct DiskPerformanceProber {
    state: Mutex<DiskProberState>,
}

#[derive(Debug, Default)]
struct DiskProberState {
    static_cache: HashMap<u32, StaticDiskInfo>,
    prev_perf: HashMap<u32, PrevDiskCounters>,
    hdd_activity: HddActivityGuard,
}

/// Logical volume mapping result for a single physical disk index.
#[derive(Debug, Default)]
struct DiskVolumeMapping {
    drive_letters: Vec<char>,
    extent_bytes: u64,
}

impl DiskPerformanceProber {
    /// Creates an empty disk prober with fresh metadata and counter caches.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Probes all local physical disks (`0..N`), bypassing `DRIVE_REMOTE`/`DRIVE_CDROM`
    /// volumes and isolating every physical disk behind `circuit_breaker` (`disk:<N>`).
    pub(crate) fn probe(&self, circuit_breaker: &CircuitBreaker) -> Vec<DiskDeviceSample> {
        let vol_map = enumerate_volume_disk_map(circuit_breaker);

        let mut candidate_indices: BTreeSet<u32> = vol_map.keys().copied().collect();
        let max_known = candidate_indices.iter().next_back().copied().unwrap_or(0);
        let scan_limit = max_known
            .saturating_add(MAX_CONSECUTIVE_MISSES)
            .min(MAX_EXTRA_PHYSICAL_DISKS);
        for idx in 0..scan_limit {
            candidate_indices.insert(idx);
        }

        let mut state = self.state.lock();
        let mut results = Vec::with_capacity(candidate_indices.len());
        let mut consecutive_misses = 0u32;

        for index in candidate_indices {
            let mapping = vol_map.get(&index);
            let drive_letters = format_drive_letters(mapping.map(|m| m.drive_letters.as_slice()));
            let extent_bytes = mapping.map_or(0, |m| m.extent_bytes);
            let target = format!("disk:{index}");

            // If this physical disk's circuit breaker is open, skip ALL IOCTLs and emit
            // cached static metadata with `circuit_open = true`.
            if circuit_breaker.check(&target).is_err() {
                if let Some(cached) = state.static_cache.get(&index) {
                    results.push(DiskDeviceSample {
                        index,
                        model: Arc::clone(&cached.model),
                        media_type: cached.media_type,
                        drive_letters,
                        capacity_bytes: cached.capacity_bytes.max(extent_bytes),
                        active_time_permille: 0,
                        read_bytes_per_sec: 0,
                        write_bytes_per_sec: 0,
                        avg_response_us: 0,
                        circuit_open: true,
                    });
                    consecutive_misses = 0;
                } else if mapping.is_some() {
                    results.push(DiskDeviceSample {
                        index,
                        model: Arc::from(format!("Disk {index}")),
                        media_type: DiskMediaType::Unknown,
                        drive_letters,
                        capacity_bytes: extent_bytes,
                        active_time_permille: 0,
                        read_bytes_per_sec: 0,
                        write_bytes_per_sec: 0,
                        avg_response_us: 0,
                        circuit_open: true,
                    });
                    consecutive_misses = 0;
                }
                continue;
            }

            let started = Instant::now();
            let device_path = format!(r"\\.\PhysicalDrive{index}");
            let Some(handle) = open_device_metadata(&device_path) else {
                // Physical disk does not exist or cannot be opened.
                if mapping.is_none() {
                    consecutive_misses += 1;
                    if consecutive_misses >= MAX_CONSECUTIVE_MISSES && index > max_known {
                        break;
                    }
                    continue;
                }
                // Drive letters map here even though PhysicalDrive open failed: try volume counters.
                let fallback_perf = mapping.and_then(|m| query_volumes_perf(&m.drive_letters));
                let elapsed = started.elapsed();
                let tripped = circuit_breaker.record_failure_or_timeout(&target, elapsed);
                if !tripped {
                    circuit_breaker.record_success(&target);
                }
                let (active, read_bps, write_bps, avg_us) = match fallback_perf {
                    Some(perf) => {
                        compute_disk_delta(&mut state.prev_perf, index, perf, Instant::now())
                    }
                    None => (0, 0, 0, 0),
                };
                let cached = state.static_cache.get(&index);
                results.push(DiskDeviceSample {
                    index,
                    model: cached
                        .map(|c| Arc::clone(&c.model))
                        .unwrap_or_else(|| Arc::from(format!("Disk {index}"))),
                    media_type: cached.map_or(DiskMediaType::Unknown, |c| c.media_type),
                    drive_letters,
                    capacity_bytes: cached
                        .map_or(extent_bytes, |c| c.capacity_bytes.max(extent_bytes)),
                    active_time_permille: active,
                    read_bytes_per_sec: read_bps,
                    write_bytes_per_sec: write_bps,
                    avg_response_us: avg_us,
                    circuit_open: tripped,
                });
                consecutive_misses = 0;
                continue;
            };
            consecutive_misses = 0;

            let static_info = state
                .static_cache
                .entry(index)
                .or_insert_with(|| query_static_disk_info(handle.as_raw(), index, extent_bytes))
                .clone();

            let capacity_bytes = static_info.capacity_bytes.max(extent_bytes);
            if capacity_bytes > static_info.capacity_bytes
                && let Some(entry) = state.static_cache.get_mut(&index)
            {
                entry.capacity_bytes = capacity_bytes;
            }

            let perf_opt = query_disk_performance(handle.as_raw())
                .or_else(|| mapping.and_then(|m| query_volumes_perf(&m.drive_letters)));

            let elapsed = started.elapsed();
            let tripped = circuit_breaker.record_failure_or_timeout(&target, elapsed);
            if tripped {
                tracing::warn!(
                    target = %target,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "slow-lane disk IOCTL exceeded 300ms budget; circuit breaker tripped"
                );
            } else {
                circuit_breaker.record_success(&target);
            }

            let (active_time_permille, read_bytes_per_sec, write_bytes_per_sec, avg_response_us) =
                match perf_opt {
                    Some(perf) => {
                        let _hdd_active = state.hdd_activity.should_allow_extra_query(
                            index,
                            static_info.media_type,
                            perf.ReadCount,
                            perf.WriteCount,
                        );
                        compute_disk_delta(&mut state.prev_perf, index, perf, Instant::now())
                    }
                    None => (0, 0, 0, 0),
                };

            results.push(DiskDeviceSample {
                index,
                model: static_info.model,
                media_type: static_info.media_type,
                drive_letters,
                capacity_bytes,
                active_time_permille,
                read_bytes_per_sec,
                write_bytes_per_sec,
                avg_response_us,
                circuit_open: tripped,
            });
        }

        // Prune stale counter & static metadata entries for removed disks (e.g., unplugged USB drives).
        let live: BTreeSet<u32> = results.iter().map(|d| d.index).collect();
        state.static_cache.retain(|idx, _| live.contains(idx));
        state.prev_perf.retain(|idx, _| live.contains(idx));
        state.hdd_activity.prune(&live);

        results
    }
}

/// Maps local drive letters (skipping `DRIVE_REMOTE` and `DRIVE_CDROM`) to physical disk
/// indices via `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`.
fn enumerate_volume_disk_map(circuit_breaker: &CircuitBreaker) -> BTreeMap<u32, DiskVolumeMapping> {
    let mut map: BTreeMap<u32, DiskVolumeMapping> = BTreeMap::new();
    let mut stack_buf = [0u16; 256];
    let mut heap_buf: Vec<u16> = Vec::new();

    // SAFETY: `stack_buf` is writable for `stack_buf.len()` UTF-16 code units.
    let len = unsafe { GetLogicalDriveStringsW(stack_buf.len() as u32, stack_buf.as_mut_ptr()) };
    if len == 0 {
        return map;
    }
    let wide_slice: &[u16] = if (len as usize) < stack_buf.len() {
        &stack_buf[..len as usize]
    } else {
        heap_buf.resize(len as usize + 1, 0);
        // SAFETY: Resized to exact required buffer length.
        let len2 = unsafe { GetLogicalDriveStringsW(heap_buf.len() as u32, heap_buf.as_mut_ptr()) };
        if len2 == 0 {
            return map;
        }
        &heap_buf[..(len2 as usize).min(heap_buf.len())]
    };

    let mut pos = 0usize;
    while pos < wide_slice.len() && wide_slice[pos] != 0 {
        let start = pos;
        while pos < wide_slice.len() && wide_slice[pos] != 0 {
            pos += 1;
        }
        let root_slice = &wide_slice[start..pos];
        pos += 1; // skip NUL

        if root_slice.len() < 2 {
            continue;
        }
        let drive_char = char::from_u32(u32::from(root_slice[0]))
            .map(|c| c.to_ascii_uppercase())
            .filter(|c| c.is_ascii_alphabetic());
        let Some(letter) = drive_char else {
            continue;
        };

        let root_wide: [u16; 4] = [
            u16::from(letter as u8),
            u16::from(b':'),
            u16::from(b'\\'),
            0,
        ];
        // SAFETY: `root_wide` is a valid null-terminated UTF-16 root string (`X:\`).
        let drive_type = unsafe { GetDriveTypeW(root_wide.as_ptr()) };
        if drive_type == DRIVE_REMOTE || drive_type == DRIVE_CDROM {
            // HARD RED LINE: strictly bypass remote/network and optical drives.
            continue;
        }

        let vol_target = format!("vol:{letter}:");
        if circuit_breaker.check(&vol_target).is_err() {
            continue;
        }

        let started = Instant::now();
        let vol_path = format!(r"\\.\{letter}:");
        let extents = open_device_metadata(&vol_path)
            .map(|h| query_volume_disk_extents(h.as_raw()))
            .unwrap_or_default();
        let elapsed = started.elapsed();
        if !circuit_breaker.record_failure_or_timeout(&vol_target, elapsed) {
            circuit_breaker.record_success(&vol_target);
        }

        for (disk_num, extent_len) in extents {
            let entry = map.entry(disk_num).or_default();
            if !entry.drive_letters.contains(&letter) {
                entry.drive_letters.push(letter);
            }
            entry.extent_bytes = entry.extent_bytes.saturating_add(extent_len);
        }
    }

    for entry in map.values_mut() {
        entry.drive_letters.sort_unstable();
    }
    map
}

/// Computes a non-negative `u64` delta between two monotonic `i64` kernel counters,
/// returning `None` if either counter is negative or a counter reset occurred (`curr < prev`).
#[inline]
fn checked_i64_delta(curr: i64, prev: i64) -> Option<u64> {
    let curr_u64 = u64::try_from(curr).ok()?;
    let prev_u64 = u64::try_from(prev).ok()?;
    curr_u64.checked_sub(prev_u64)
}

/// Computes `(active_time_permille, read_bytes_per_sec, write_bytes_per_sec, avg_response_us)`
/// from two consecutive `DISK_PERFORMANCE` samples using `checked_sub` counter-reset safety.
pub(super) fn compute_disk_delta(
    prev_map: &mut HashMap<u32, PrevDiskCounters>,
    index: u32,
    curr: DISK_PERFORMANCE,
    now: Instant,
) -> (u16, u64, u64, u64) {
    let next_entry = PrevDiskCounters {
        bytes_read: curr.BytesRead,
        bytes_written: curr.BytesWritten,
        read_time_100ns: curr.ReadTime,
        write_time_100ns: curr.WriteTime,
        idle_time_100ns: curr.IdleTime,
        read_count: curr.ReadCount,
        write_count: curr.WriteCount,
        query_time_100ns: curr.QueryTime,
        sampled_at: now,
    };

    let Some(prev) = prev_map.insert(index, next_entry) else {
        return (0, 0, 0, 0);
    };

    let dt_query_100ns = checked_i64_delta(curr.QueryTime, prev.query_time_100ns).unwrap_or(0);
    let dt_wall_100ns = (now.saturating_duration_since(prev.sampled_at).as_nanos() / 100) as u64;
    let dt_100ns = if dt_query_100ns > 0 {
        dt_query_100ns
    } else {
        dt_wall_100ns
    };
    if dt_100ns == 0 {
        return (0, 0, 0, 0);
    }

    let (Some(d_read), Some(d_write)) = (
        checked_i64_delta(curr.BytesRead, prev.bytes_read),
        checked_i64_delta(curr.BytesWritten, prev.bytes_written),
    ) else {
        // Counter reset (e.g., controller reset or device re-enumeration): baseline was already
        // refreshed above, so emit zeros instead of underflowing or fabricating a spike.
        return (0, 0, 0, 0);
    };

    let read_bps =
        ((u128::from(d_read) * 10_000_000) / u128::from(dt_100ns)).min(u128::from(u64::MAX)) as u64;
    let write_bps = ((u128::from(d_write) * 10_000_000) / u128::from(dt_100ns))
        .min(u128::from(u64::MAX)) as u64;

    let d_idle = checked_i64_delta(curr.IdleTime, prev.idle_time_100ns).unwrap_or(0);
    let active_100ns = dt_100ns.saturating_sub(d_idle);
    let active_permille =
        ((u128::from(active_100ns) * 1_000) / u128::from(dt_100ns)).min(1_000) as u16;

    let d_ops = match (
        curr.ReadCount.checked_sub(prev.read_count),
        curr.WriteCount.checked_sub(prev.write_count),
    ) {
        (Some(r), Some(w)) => u64::from(r).saturating_add(u64::from(w)),
        _ => 0,
    };
    let d_io_100ns = checked_i64_delta(curr.ReadTime, prev.read_time_100ns)
        .unwrap_or(0)
        .saturating_add(checked_i64_delta(curr.WriteTime, prev.write_time_100ns).unwrap_or(0));
    let avg_response_us = d_io_100ns
        .checked_div(d_ops.saturating_mul(10))
        .unwrap_or(0);

    (active_permille, read_bps, write_bps, avg_response_us)
}

/// Formats sorted drive letters as `"C: D:"`.
fn format_drive_letters(letters: Option<&[char]>) -> Arc<str> {
    let Some(letters) = letters else {
        return Arc::from("");
    };
    if letters.is_empty() {
        return Arc::from("");
    }
    let mut s = String::with_capacity(letters.len() * 3);
    for (idx, &ch) in letters.iter().enumerate() {
        if idx > 0 {
            s.push(' ');
        }
        s.push(ch);
        s.push(':');
    }
    Arc::from(s)
}
