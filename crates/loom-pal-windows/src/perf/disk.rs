//! Windows slow-lane physical disk telemetry via `DeviceIoControl` (`IOCTL_DISK_PERFORMANCE`,
//! `IOCTL_STORAGE_QUERY_PROPERTY`, `IOCTL_DISK_GET_LENGTH_INFO`, `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`)
//! guarded by a per-device `300 ms` [`CircuitBreaker`] and mandatory `DRIVE_REMOTE` bypass.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::c_void;
use std::mem::{offset_of, size_of, transmute, zeroed};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use loom_types::{DiskDeviceSample, DiskMediaType};
use parking_lot::Mutex;
use windows_sys::Win32::Foundation::{DEVPROPKEY, ERROR_SUCCESS, HANDLE, HMODULE};
use windows_sys::Win32::Storage::FileSystem::{
    BusTypeNvme, CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDriveTypeW,
    GetLogicalDriveStringsW, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DEVICE_SEEK_PENALTY_DESCRIPTOR, DISK_EXTENT, DISK_GEOMETRY_EX, DISK_PERFORMANCE,
    GET_LENGTH_INFORMATION, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_DISK_GET_LENGTH_INFO,
    IOCTL_DISK_PERFORMANCE, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
    STORAGE_DEVICE_DESCRIPTOR, STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
    StorageDeviceSeekPenaltyProperty, VOLUME_DISK_EXTENTS,
};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::WindowsProgramming::{DRIVE_CDROM, DRIVE_REMOTE};
use windows_sys::core::GUID;

use crate::circuit_breaker::CircuitBreaker;
use crate::handle::{OwnedHandle, to_wide_null};

/// Maximum physical disk indices scanned beyond those discovered via logical volumes.
const MAX_EXTRA_PHYSICAL_DISKS: u32 = 16;
/// Stop scanning extra physical disk indices after this many consecutive misses.
const MAX_CONSECUTIVE_MISSES: u32 = 4;

/// Cached static attributes for a physical disk (`\\.\PhysicalDrive{N}`).
#[derive(Debug, Clone)]
struct StaticDiskInfo {
    model: Arc<str>,
    media_type: DiskMediaType,
    capacity_bytes: u64,
}

/// Previous `DISK_PERFORMANCE` counters for per-disk delta calculation.
#[derive(Debug, Clone, Copy)]
struct PrevDiskCounters {
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

        // Prune stale counter entries for removed disks (e.g., unplugged USB drives).
        let live: BTreeSet<u32> = results.iter().map(|d| d.index).collect();
        state.prev_perf.retain(|idx, _| live.contains(idx));
        state.hdd_activity.prune(&live);

        results
    }
}

/// Maps local drive letters (skipping `DRIVE_REMOTE` and `DRIVE_CDROM`) to physical disk
/// indices via `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`.
fn enumerate_volume_disk_map(circuit_breaker: &CircuitBreaker) -> BTreeMap<u32, DiskVolumeMapping> {
    let mut map: BTreeMap<u32, DiskVolumeMapping> = BTreeMap::new();
    let mut wide_buf = vec![0u16; 256];

    // SAFETY: `wide_buf` is writable for `wide_buf.len()` UTF-16 code units.
    let len = unsafe { GetLogicalDriveStringsW(wide_buf.len() as u32, wide_buf.as_mut_ptr()) };
    if len == 0 {
        return map;
    }
    if len as usize >= wide_buf.len() {
        wide_buf.resize(len as usize + 1, 0);
        // SAFETY: Resized to exact required buffer length.
        let len2 = unsafe { GetLogicalDriveStringsW(wide_buf.len() as u32, wide_buf.as_mut_ptr()) };
        if len2 == 0 {
            return map;
        }
    }

    let mut pos = 0usize;
    while pos < wide_buf.len() && wide_buf[pos] != 0 {
        let start = pos;
        while pos < wide_buf.len() && wide_buf[pos] != 0 {
            pos += 1;
        }
        let root_slice = &wide_buf[start..pos];
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

/// Opens a device or volume with metadata-only (`dwDesiredAccess = 0`) non-exclusive access,
/// which works without Administrator elevation on Windows 10/11.
fn open_device_metadata(path: &str) -> Option<OwnedHandle> {
    let wide = to_wide_null(path);
    // SAFETY: Standard Win32 `CreateFileW` metadata-only device open (`0` access).
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    OwnedHandle::from_raw(raw)
}

/// Queries `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` returning `(disk_number, extent_length_bytes)`.
fn query_volume_disk_extents(handle: HANDLE) -> Vec<(u32, u64)> {
    // Allocate space for up to 16 extents (`VOLUME_DISK_EXTENTS` header + 16 `DISK_EXTENT`s).
    let buf_bytes = size_of::<VOLUME_DISK_EXTENTS>() + 15 * size_of::<DISK_EXTENT>();
    let mut buf: Vec<u64> = vec![0u64; buf_bytes.div_ceil(size_of::<u64>())];
    let mut bytes_returned: u32 = 0;

    // SAFETY: `buf` is 8-byte aligned and sized for `VOLUME_DISK_EXTENTS` + trailing extents.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            ptr::null(),
            0,
            buf.as_mut_ptr().cast::<c_void>(),
            (buf.len() * size_of::<u64>()) as u32,
            &mut bytes_returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 || (bytes_returned as usize) < size_of::<VOLUME_DISK_EXTENTS>() {
        return Vec::new();
    }

    let base = buf.as_ptr().cast::<u8>();
    // SAFETY: `bytes_returned >= size_of::<VOLUME_DISK_EXTENTS>()` and `buf` is 8-byte aligned.
    let count = unsafe { ptr::read_unaligned(base.cast::<u32>()) } as usize;
    let extents_offset = offset_of!(VOLUME_DISK_EXTENTS, Extents);
    let max_fit =
        (bytes_returned as usize).saturating_sub(extents_offset) / size_of::<DISK_EXTENT>();
    let n = count.min(max_fit);

    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let ptr_ext = unsafe {
            base.add(extents_offset + i * size_of::<DISK_EXTENT>())
                .cast::<DISK_EXTENT>()
        };
        // SAFETY: Bounds validated via `n <= max_fit`.
        let ext = unsafe { ptr::read_unaligned(ptr_ext) };
        out.push((ext.DiskNumber, ext.ExtentLength.max(0) as u64));
    }
    out
}

/// Queries static disk model, media type (`Nvme`/`Ssd`/`Hdd`), and capacity in bytes.
fn query_static_disk_info(handle: HANDLE, index: u32, fallback_capacity: u64) -> StaticDiskInfo {
    let (model, bus_type) = query_storage_device_descriptor(handle);
    let media_type = if bus_type == Some(BusTypeNvme) {
        DiskMediaType::Nvme
    } else {
        match query_seek_penalty(handle) {
            Some(false) => DiskMediaType::Ssd,
            Some(true) => DiskMediaType::Hdd,
            None => DiskMediaType::Unknown,
        }
    };

    let capacity_bytes = query_disk_capacity_bytes(handle).unwrap_or(fallback_capacity);
    let model_str = if model.is_empty() {
        format!("Disk {index}")
    } else {
        model
    };

    StaticDiskInfo {
        model: Arc::from(model_str),
        media_type,
        capacity_bytes,
    }
}

/// Queries `IOCTL_STORAGE_QUERY_PROPERTY(StorageDeviceProperty)` for vendor/product model and bus type.
fn query_storage_device_descriptor(handle: HANDLE) -> (String, Option<i32>) {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut out_buf = vec![0u8; 1024];
    let mut returned: u32 = 0;

    // SAFETY: `query` and `out_buf` are valid buffers for `IOCTL_STORAGE_QUERY_PROPERTY`.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            (&query as *const STORAGE_PROPERTY_QUERY).cast::<c_void>(),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            out_buf.as_mut_ptr().cast::<c_void>(),
            out_buf.len() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 || (returned as usize) < size_of::<STORAGE_DEVICE_DESCRIPTOR>() {
        return (String::new(), None);
    }

    let valid = &out_buf[..(returned as usize).min(out_buf.len())];
    // SAFETY: `valid.len() >= size_of::<STORAGE_DEVICE_DESCRIPTOR>()`.
    let desc = unsafe { ptr::read_unaligned(valid.as_ptr().cast::<STORAGE_DEVICE_DESCRIPTOR>()) };

    let vendor = read_c_string_at(valid, desc.VendorIdOffset as usize);
    let product = read_c_string_at(valid, desc.ProductIdOffset as usize);

    let model = match (vendor.is_empty(), product.is_empty()) {
        (false, false) => {
            if product
                .to_ascii_lowercase()
                .starts_with(&vendor.to_ascii_lowercase())
            {
                product
            } else {
                format!("{vendor} {product}")
            }
        }
        (true, false) => product,
        (false, true) => vendor,
        (true, true) => String::new(),
    };

    (model, Some(desc.BusType))
}

/// Queries `IOCTL_STORAGE_QUERY_PROPERTY(StorageDeviceSeekPenaltyProperty)`.
fn query_seek_penalty(handle: HANDLE) -> Option<bool> {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut out = DEVICE_SEEK_PENALTY_DESCRIPTOR::default();
    let mut returned: u32 = 0;

    // SAFETY: `query` and `out` match the documented seek-penalty IOCTL ABI.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            (&query as *const STORAGE_PROPERTY_QUERY).cast::<c_void>(),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            (&mut out as *mut DEVICE_SEEK_PENALTY_DESCRIPTOR).cast::<c_void>(),
            size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok != 0 && (returned as usize) >= size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() {
        Some(out.IncursSeekPenalty)
    } else {
        None
    }
}

/// Queries physical disk capacity in bytes via `IOCTL_DISK_GET_LENGTH_INFO` with
/// `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX` fallback.
fn query_disk_capacity_bytes(handle: HANDLE) -> Option<u64> {
    let mut len_info = GET_LENGTH_INFORMATION::default();
    let mut returned: u32 = 0;
    // SAFETY: `len_info` matches `IOCTL_DISK_GET_LENGTH_INFO`.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_GET_LENGTH_INFO,
            ptr::null(),
            0,
            (&mut len_info as *mut GET_LENGTH_INFORMATION).cast::<c_void>(),
            size_of::<GET_LENGTH_INFORMATION>() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok != 0 && len_info.Length > 0 {
        return Some(len_info.Length as u64);
    }

    let mut geom_buf = [0u64; 32]; // 256 bytes, 8-byte aligned
    let mut returned2: u32 = 0;
    // SAFETY: `geom_buf` is larger than `DISK_GEOMETRY_EX` and 8-byte aligned.
    let ok2 = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
            ptr::null(),
            0,
            geom_buf.as_mut_ptr().cast::<c_void>(),
            (geom_buf.len() * size_of::<u64>()) as u32,
            &mut returned2,
            ptr::null_mut(),
        )
    };
    if ok2 != 0 && (returned2 as usize) >= size_of::<DISK_GEOMETRY_EX>() {
        // SAFETY: `returned2 >= size_of::<DISK_GEOMETRY_EX>()`.
        let geom = unsafe { ptr::read_unaligned(geom_buf.as_ptr().cast::<DISK_GEOMETRY_EX>()) };
        if geom.DiskSize > 0 {
            return Some(geom.DiskSize as u64);
        }
    }

    None
}

/// Queries `IOCTL_DISK_PERFORMANCE` on a physical disk or volume handle.
fn query_disk_performance(handle: HANDLE) -> Option<DISK_PERFORMANCE> {
    let mut perf = DISK_PERFORMANCE::default();
    let mut returned: u32 = 0;
    // SAFETY: `perf` is POD and matches `IOCTL_DISK_PERFORMANCE`.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_PERFORMANCE,
            ptr::null(),
            0,
            (&mut perf as *mut DISK_PERFORMANCE).cast::<c_void>(),
            size_of::<DISK_PERFORMANCE>() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok != 0 && (returned as usize) >= size_of::<DISK_PERFORMANCE>() {
        Some(perf)
    } else {
        None
    }
}

/// Fallback: aggregates `IOCTL_DISK_PERFORMANCE` across mounted local volumes on a disk.
fn query_volumes_perf(drive_letters: &[char]) -> Option<DISK_PERFORMANCE> {
    let mut agg = DISK_PERFORMANCE::default();
    let mut any = false;
    for &letter in drive_letters {
        let path = format!(r"\\.\{letter}:");
        let Some(handle) = open_device_metadata(&path) else {
            continue;
        };
        let Some(p) = query_disk_performance(handle.as_raw()) else {
            continue;
        };
        agg.BytesRead = agg.BytesRead.saturating_add(p.BytesRead);
        agg.BytesWritten = agg.BytesWritten.saturating_add(p.BytesWritten);
        agg.ReadTime = agg.ReadTime.saturating_add(p.ReadTime);
        agg.WriteTime = agg.WriteTime.saturating_add(p.WriteTime);
        agg.IdleTime = agg.IdleTime.saturating_add(p.IdleTime);
        agg.ReadCount = agg.ReadCount.wrapping_add(p.ReadCount);
        agg.WriteCount = agg.WriteCount.wrapping_add(p.WriteCount);
        agg.QueryTime = agg.QueryTime.max(p.QueryTime);
        any = true;
    }
    any.then_some(agg)
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
fn compute_disk_delta(
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

/// Inbox Windows storage controller drivers permitted for optional SMART / temperature IOCTLs.
///
/// Vendor RAID / Intel VMD controllers (such as `iaStorVD`, `iaStorAC`, `nvraid`, `megaraid`)
/// are excluded because vendor pass-through IOCTLs can stall or destabilize firmware queues.
pub(crate) const ALLOWED_STORAGE_CONTROLLER_DRIVERS: &[&str] = &["stornvme", "storahci", "atapi"];

/// `DEVPKEY_Device_Service` (`{a45c254e-df1c-4efd-8020-67d146a850e0}`, pid `6`).
const DEVPKEY_DEVICE_SERVICE: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0xa45c_254e_df1c_4efd_8020_67d1_46a8_50e0),
    pid: 6,
};
/// `CONFIGRET` success value (`CR_SUCCESS`).
const CR_SUCCESS: u32 = 0;
/// `CONFIGRET` buffer-too-small value (`CR_BUFFER_SMALL`).
const CR_BUFFER_SMALL: u32 = 26;
/// `CM_LOCATE_DEVNODE_NORMAL`.
const CM_LOCATE_DEVNODE_NORMAL: u32 = 0;
/// `DEVPROP_TYPE_STRING`.
const DEVPROP_TYPE_STRING: u32 = 18;
/// `CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE`.
const CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE: i32 = 1;
/// `CM_NOTIFY_ACTION_DEVICEQUERYREMOVE`.
pub(crate) const CM_NOTIFY_ACTION_DEVICEQUERYREMOVE: i32 = 2;
/// `CM_NOTIFY_ACTION_DEVICEREMOVEPENDING`.
pub(crate) const CM_NOTIFY_ACTION_DEVICEREMOVEPENDING: i32 = 4;
/// `CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE`.
pub(crate) const CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE: i32 = 5;

type FarProc = unsafe extern "system" fn() -> isize;
type FnCmLocateDevNodeW = unsafe extern "system" fn(
    pdn_dev_inst: *mut u32,
    p_device_id: *const u16,
    ul_flags: u32,
) -> u32;
type FnCmGetParent =
    unsafe extern "system" fn(pdn_dev_inst: *mut u32, dn_dev_inst: u32, ul_flags: u32) -> u32;
type FnCmGetDevNodePropertyW = unsafe extern "system" fn(
    dn_dev_inst: u32,
    property_key: *const DEVPROPKEY,
    property_type: *mut u32,
    property_buffer: *mut u8,
    property_buffer_size: *mut u32,
    ul_flags: u32,
) -> u32;
type FnCmNotifyCallback = unsafe extern "system" fn(
    h_notify: isize,
    context: *const c_void,
    action: i32,
    event_data: *const c_void,
    event_data_size: u32,
) -> u32;
type FnCmRegisterNotification = unsafe extern "system" fn(
    p_filter: *const CmNotifyFilter,
    p_context: *const c_void,
    p_callback: Option<FnCmNotifyCallback>,
    p_notify_context: *mut isize,
) -> u32;
type FnCmUnregisterNotification = unsafe extern "system" fn(notify_context: isize) -> u32;

/// Exact Windows `CM_NOTIFY_FILTER` ABI layout (`cfgmgr32.h`).
#[repr(C)]
#[derive(Clone, Copy)]
struct CmNotifyFilter {
    cb_size: u32,
    flags: u32,
    filter_type: i32,
    reserved: u32,
    u: CmNotifyFilterUnion,
}

#[repr(C)]
#[derive(Clone, Copy)]
union CmNotifyFilterUnion {
    device_handle: HANDLE,
    _instance_id_sizing: [u16; 200],
}

/// Dynamically resolved `cfgmgr32.dll` PnP device property and notification entry points.
struct CfgMgrApi {
    _module: HMODULE,
    cm_locate_dev_node_w: FnCmLocateDevNodeW,
    cm_get_parent: FnCmGetParent,
    cm_get_dev_node_property_w: FnCmGetDevNodePropertyW,
    cm_register_notification: Option<FnCmRegisterNotification>,
    cm_unregister_notification: Option<FnCmUnregisterNotification>,
}

// Function pointers and process-lifetime HMODULE from `cfgmgr32.dll` are thread-safe.
unsafe impl Send for CfgMgrApi {}
unsafe impl Sync for CfgMgrApi {}

/// Lazily loads `cfgmgr32.dll` once for process lifetime.
fn cfgmgr_api() -> Option<&'static CfgMgrApi> {
    static API: OnceLock<Option<CfgMgrApi>> = OnceLock::new();
    API.get_or_init(|| {
        let dll = to_wide_null("cfgmgr32.dll");
        // SAFETY: `cfgmgr32.dll` is a core Windows PnP system DLL loaded strictly from `System32`.
        let module =
            unsafe { LoadLibraryExW(dll.as_ptr(), ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if module.is_null() {
            return None;
        }
        let locate = unsafe { GetProcAddress(module, c"CM_Locate_DevNodeW".as_ptr().cast()) }?;
        let parent = unsafe { GetProcAddress(module, c"CM_Get_Parent".as_ptr().cast()) }?;
        let prop = unsafe { GetProcAddress(module, c"CM_Get_DevNode_PropertyW".as_ptr().cast()) }?;
        let reg = unsafe { GetProcAddress(module, c"CM_Register_Notification".as_ptr().cast()) };
        let unreg =
            unsafe { GetProcAddress(module, c"CM_Unregister_Notification".as_ptr().cast()) };

        // SAFETY: All symbols match their documented `cfgmgr32.h` prototypes.
        unsafe {
            Some(CfgMgrApi {
                _module: module,
                cm_locate_dev_node_w: transmute::<FarProc, FnCmLocateDevNodeW>(locate),
                cm_get_parent: transmute::<FarProc, FnCmGetParent>(parent),
                cm_get_dev_node_property_w: transmute::<FarProc, FnCmGetDevNodePropertyW>(prop),
                cm_register_notification: reg
                    .map(|p| transmute::<FarProc, FnCmRegisterNotification>(p)),
                cm_unregister_notification: unreg
                    .map(|p| transmute::<FarProc, FnCmUnregisterNotification>(p)),
            })
        }
    })
    .as_ref()
}

/// Returns `true` if `service_name` matches Loom's inbox storage controller allowlist
/// (`stornvme`, `storahci`, `atapi`), rejecting unsafe RAID/VMD controllers (e.g. `iaStorVD`).
pub(crate) fn is_storage_controller_driver_allowed(service_name: &str) -> bool {
    let trimmed = service_name.trim();
    if trimmed.is_empty() {
        return false;
    }
    ALLOWED_STORAGE_CONTROLLER_DRIVERS
        .iter()
        .any(|allowed| trimmed.eq_ignore_ascii_case(allowed))
}

/// Queries `DEVPKEY_Device_Service` for a `DEVINST` via dynamically loaded `cfgmgr32.dll`.
fn query_devnode_service(api: &CfgMgrApi, devinst: u32) -> Option<String> {
    let mut prop_type: u32 = 0;
    let mut needed_bytes: u32 = 0;
    // SAFETY: Null buffer queries required byte length for `DEVPKEY_Device_Service`.
    let rc = unsafe {
        (api.cm_get_dev_node_property_w)(
            devinst,
            &DEVPKEY_DEVICE_SERVICE,
            &mut prop_type,
            ptr::null_mut(),
            &mut needed_bytes,
            0,
        )
    };
    if (rc != CR_BUFFER_SMALL && rc != CR_SUCCESS) || needed_bytes == 0 || needed_bytes > 4096 {
        return None;
    }

    let mut wide_buf = vec![0u16; (needed_bytes as usize).div_ceil(size_of::<u16>())];
    let mut out_bytes = (wide_buf.len() * size_of::<u16>()) as u32;
    // SAFETY: `wide_buf` is sized for at least `needed_bytes`.
    let rc2 = unsafe {
        (api.cm_get_dev_node_property_w)(
            devinst,
            &DEVPKEY_DEVICE_SERVICE,
            &mut prop_type,
            wide_buf.as_mut_ptr().cast::<u8>(),
            &mut out_bytes,
            0,
        )
    };
    if rc2 != CR_SUCCESS || prop_type != DEVPROP_TYPE_STRING {
        return None;
    }
    let code_units = (out_bytes as usize / size_of::<u16>()).min(wide_buf.len());
    let slice = &wide_buf[..code_units];
    let len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    let service = String::from_utf16_lossy(&slice[..len]).trim().to_owned();
    (!service.is_empty()).then_some(service)
}

/// Resolves the parent storage controller's `DEVPKEY_Device_Service` driver name for a disk
/// PnP Device Instance ID using `CM_Locate_DevNodeW` -> `CM_Get_Parent` -> `CM_Get_DevNode_PropertyW`.
#[allow(dead_code)]
pub(crate) fn query_parent_controller_service(disk_dev_instance_id: &str) -> Option<String> {
    if disk_dev_instance_id.trim().is_empty() {
        return None;
    }
    let api = cfgmgr_api()?;
    let wide_id = to_wide_null(disk_dev_instance_id);
    let mut devinst: u32 = 0;
    // SAFETY: `wide_id` is null-terminated UTF-16.
    let rc = unsafe {
        (api.cm_locate_dev_node_w)(&mut devinst, wide_id.as_ptr(), CM_LOCATE_DEVNODE_NORMAL)
    };
    if rc != CR_SUCCESS || devinst == 0 {
        return None;
    }

    let mut parent: u32 = 0;
    // SAFETY: `devinst` was located successfully above.
    let rc_parent = unsafe { (api.cm_get_parent)(&mut parent, devinst, 0) };
    if rc_parent != CR_SUCCESS || parent == 0 {
        return None;
    }
    query_devnode_service(api, parent)
}

/// Returns `true` only when a disk's parent PnP storage controller runs an allowlisted inbox
/// driver (`stornvme`, `storahci`, `atapi`), safely gating optional SMART/temperature IOCTLs.
#[allow(dead_code)]
pub(crate) fn is_disk_controller_smart_safe(disk_dev_instance_id: &str) -> bool {
    query_parent_controller_service(disk_dev_instance_id)
        .is_some_and(|svc| is_storage_controller_driver_allowed(&svc))
}

/// Rotational HDD activity-change guard preventing extra SMART/temperature queries from
/// spinning up idle/parked mechanical hard drives.
#[derive(Debug, Default)]
pub(crate) struct HddActivityGuard {
    last_ops: HashMap<u32, (u32, u32)>,
}

impl HddActivityGuard {
    /// Returns `true` if extra optional queries (such as SMART or temperature) are permitted.
    ///
    /// - Non-rotational media (`Nvme`, `Ssd`, `Unknown`) are always allowed.
    /// - Rotational HDDs (`DiskMediaType::Hdd`) are allowed ONLY when `read_count` or
    ///   `write_count` changed since the previous sample (first baseline sample returns `false`
    ///   so a spun-down HDD is never woken unnecessarily).
    pub(crate) fn should_allow_extra_query(
        &mut self,
        disk_index: u32,
        media_type: DiskMediaType,
        read_count: u32,
        write_count: u32,
    ) -> bool {
        if media_type != DiskMediaType::Hdd {
            return true;
        }
        let current = (read_count, write_count);
        match self.last_ops.insert(disk_index, current) {
            None => false,
            Some(prev) => prev != current,
        }
    }

    /// Prunes entries for removed physical disks.
    pub(crate) fn prune(&mut self, live_indices: &BTreeSet<u32>) {
        self.last_ops.retain(|idx, _| live_indices.contains(idx));
    }
}

/// Returns `true` if a `CM_NOTIFY_ACTION` indicates safe-eject / device removal.
#[inline]
pub(crate) fn is_safe_eject_action(action: i32) -> bool {
    matches!(
        action,
        CM_NOTIFY_ACTION_DEVICEQUERYREMOVE
            | CM_NOTIFY_ACTION_DEVICEREMOVEPENDING
            | CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE
    )
}

/// `CM_Register_Notification` callback that records removal requests without ever vetoing eject.
unsafe extern "system" fn safe_eject_notify_callback(
    _h_notify: isize,
    context: *const c_void,
    action: i32,
    _event_data: *const c_void,
    _event_data_size: u32,
) -> u32 {
    if !context.is_null() && is_safe_eject_action(action) {
        // SAFETY: `context` points to a heap-allocated `Arc<AtomicBool>` kept alive until
        // after `CM_Unregister_Notification` finishes waiting for active callbacks.
        let flag = unsafe { &*(context.cast::<Arc<AtomicBool>>()) };
        flag.store(true, Ordering::Release);
    }
    ERROR_SUCCESS
}

/// RAII `CM_Register_Notification` device-handle safe-eject guard.
///
/// Listens for `CM_NOTIFY_ACTION_DEVICEQUERYREMOVE` / `DEVICEREMOVEPENDING` /
/// `DEVICEREMOVECOMPLETE` on an open device handle so callers can promptly close handles and
/// never block Windows USB "Safely Remove Hardware".
#[allow(dead_code)]
pub(crate) struct SafeEjectGuard {
    notify_handle: isize,
    unregister: FnCmUnregisterNotification,
    flag: Arc<AtomicBool>,
    context_ptr: *mut Arc<AtomicBool>,
}

// `SafeEjectGuard` owns a kernel notification handle and an `Arc<AtomicBool>` context.
unsafe impl Send for SafeEjectGuard {}
unsafe impl Sync for SafeEjectGuard {}

#[allow(dead_code)]
impl SafeEjectGuard {
    /// Registers a `CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE` notification for `device_handle`.
    ///
    /// Returns `None` if `cfgmgr32.dll` notifications are unavailable or registration fails.
    pub(crate) fn register(device_handle: HANDLE) -> Option<Self> {
        if device_handle.is_null() {
            return None;
        }
        let api = cfgmgr_api()?;
        let register = api.cm_register_notification?;
        let unregister = api.cm_unregister_notification?;

        let flag = Arc::new(AtomicBool::new(false));
        let context_ptr = Box::into_raw(Box::new(Arc::clone(&flag)));

        let mut filter: CmNotifyFilter = unsafe { zeroed() };
        filter.cb_size = size_of::<CmNotifyFilter>() as u32;
        filter.flags = 0;
        filter.filter_type = CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE;
        filter.reserved = 0;
        filter.u.device_handle = device_handle;

        let mut notify_handle: isize = 0;
        // SAFETY: `filter` matches `CM_NOTIFY_FILTER` for `DEVICEHANDLE`, and `context_ptr`
        // remains valid until after `CM_Unregister_Notification` completes in `Drop`.
        let rc = unsafe {
            register(
                &filter,
                context_ptr.cast::<c_void>(),
                Some(safe_eject_notify_callback),
                &mut notify_handle,
            )
        };
        if rc != CR_SUCCESS || notify_handle == 0 {
            // SAFETY: Registration failed, so no callback can run; reclaim `context_ptr`.
            unsafe {
                drop(Box::from_raw(context_ptr));
            }
            return None;
        }

        Some(Self {
            notify_handle,
            unregister,
            flag,
            context_ptr,
        })
    }

    /// Returns `true` once Windows has signaled a safe-eject / removal event for this handle.
    #[inline]
    pub(crate) fn is_removal_requested(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Returns a cloned atomic flag that callers can poll across threads.
    #[inline]
    pub(crate) fn removal_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.flag)
    }
}

impl Drop for SafeEjectGuard {
    fn drop(&mut self) {
        if self.notify_handle != 0 {
            // SAFETY: `CM_Unregister_Notification` synchronizes with any active callback before
            // returning, guaranteeing `context_ptr` is no longer dereferenced afterwards.
            unsafe {
                let _ = (self.unregister)(self.notify_handle);
            }
            self.notify_handle = 0;
        }
        if !self.context_ptr.is_null() {
            // SAFETY: Reclaims the `Box<Arc<AtomicBool>>` created in `register`.
            unsafe {
                drop(Box::from_raw(self.context_ptr));
            }
            self.context_ptr = ptr::null_mut();
        }
    }
}

/// Extracts a trimmed ASCII C-string at `offset` inside `buf`.
fn read_c_string_at(buf: &[u8], offset: usize) -> String {
    if offset == 0 || offset >= buf.len() {
        return String::new();
    }
    let slice = &buf[offset..];
    let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    String::from_utf8_lossy(&slice[..end]).trim().to_owned()
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn storage_controller_driver_allowlist_permits_inbox_and_rejects_raid_vmd() {
        assert!(is_storage_controller_driver_allowed("stornvme"));
        assert!(is_storage_controller_driver_allowed("StorNVMe"));
        assert!(is_storage_controller_driver_allowed("storahci"));
        assert!(is_storage_controller_driver_allowed("ATAPI"));
        assert!(is_storage_controller_driver_allowed(" stornvme "));

        assert!(!is_storage_controller_driver_allowed("iaStorVD"));
        assert!(!is_storage_controller_driver_allowed("iaStorAC"));
        assert!(!is_storage_controller_driver_allowed("nvraid"));
        assert!(!is_storage_controller_driver_allowed("megaraid"));
        assert!(!is_storage_controller_driver_allowed(""));
        assert!(!is_storage_controller_driver_allowed("   "));
    }

    #[test]
    fn hdd_activity_guard_skips_idle_hdds_and_allows_non_rotational_media() {
        let mut guard = HddActivityGuard::default();

        // NVMe and SSD are always permitted even with zero I/O changes.
        assert!(guard.should_allow_extra_query(0, DiskMediaType::Nvme, 10, 20));
        assert!(guard.should_allow_extra_query(0, DiskMediaType::Nvme, 10, 20));
        assert!(guard.should_allow_extra_query(1, DiskMediaType::Ssd, 5, 5));

        // Rotational HDD: baseline tick is conservatively skipped; unchanged counters stay skipped.
        assert!(!guard.should_allow_extra_query(2, DiskMediaType::Hdd, 100, 200));
        assert!(!guard.should_allow_extra_query(2, DiskMediaType::Hdd, 100, 200));
        // Activity change permits extra query.
        assert!(guard.should_allow_extra_query(2, DiskMediaType::Hdd, 101, 200));
        assert!(guard.should_allow_extra_query(2, DiskMediaType::Hdd, 101, 205));
        // Returning to idle blocks extra query again.
        assert!(!guard.should_allow_extra_query(2, DiskMediaType::Hdd, 101, 205));

        // Pruning drops removed disks cleanly.
        let live: BTreeSet<u32> = [0, 1].into_iter().collect();
        guard.prune(&live);
        assert!(!guard.should_allow_extra_query(2, DiskMediaType::Hdd, 101, 205));
    }

    #[test]
    fn safe_eject_action_classification_and_callback_update_flag() {
        assert!(is_safe_eject_action(CM_NOTIFY_ACTION_DEVICEQUERYREMOVE));
        assert!(is_safe_eject_action(CM_NOTIFY_ACTION_DEVICEREMOVEPENDING));
        assert!(is_safe_eject_action(CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE));
        assert!(!is_safe_eject_action(0)); // DEVICEINTERFACEARRIVAL
        assert!(!is_safe_eject_action(3)); // DEVICEQUERYREMOVEFAILED

        let flag = Arc::new(AtomicBool::new(false));
        let ptr = (&flag as *const Arc<AtomicBool>).cast::<c_void>();
        // Non-eject action leaves flag false and returns ERROR_SUCCESS.
        let rc1 = unsafe { safe_eject_notify_callback(0, ptr, 0, ptr::null(), 0) };
        assert_eq!(rc1, ERROR_SUCCESS);
        assert!(!flag.load(Ordering::Acquire));

        // DEVICEQUERYREMOVE sets flag true without vetoing (ERROR_SUCCESS).
        let rc2 = unsafe {
            safe_eject_notify_callback(0, ptr, CM_NOTIFY_ACTION_DEVICEQUERYREMOVE, ptr::null(), 0)
        };
        assert_eq!(rc2, ERROR_SUCCESS);
        assert!(flag.load(Ordering::Acquire));

        // Null handle registration fails gracefully.
        assert!(SafeEjectGuard::register(ptr::null_mut()).is_none());
    }

    #[test]
    fn compute_disk_delta_uses_checked_sub_and_handles_counter_resets() {
        let mut prev = HashMap::new();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        let t2 = t1 + Duration::from_secs(1);
        let t3 = t2 + Duration::from_secs(1);

        let base = DISK_PERFORMANCE {
            BytesRead: 1_000_000,
            BytesWritten: 500_000,
            ReadTime: 20_000,
            WriteTime: 10_000,
            IdleTime: 5_000_000,
            ReadCount: 100,
            WriteCount: 50,
            QueryTime: 10_000_000,
            ..DISK_PERFORMANCE::default()
        };
        assert_eq!(compute_disk_delta(&mut prev, 0, base, t0), (0, 0, 0, 0));

        // Normal 1-second delta: 400KB read, 200KB write, 60% idle => 40% active (400 permille),
        // 10 ops taking 100_000 * 100ns = 10_000 us => 1_000 us avg response.
        let normal = DISK_PERFORMANCE {
            BytesRead: 1_400_000,
            BytesWritten: 700_000,
            ReadTime: 80_000,
            WriteTime: 50_000,
            IdleTime: 11_000_000,
            ReadCount: 106,
            WriteCount: 54,
            QueryTime: 20_000_000,
            ..DISK_PERFORMANCE::default()
        };
        assert_eq!(
            compute_disk_delta(&mut prev, 0, normal, t1),
            (400, 400_000, 200_000, 1_000)
        );

        // Counter reset (BytesRead/Write and ReadCount/WriteCount drop below previous):
        // must return zeros instead of underflowing or wrapping u32 ReadCount.
        let reset = DISK_PERFORMANCE {
            BytesRead: 10_000,
            BytesWritten: 5_000,
            ReadTime: 1_000,
            WriteTime: 500,
            IdleTime: 1_000_000,
            ReadCount: 2,
            WriteCount: 1,
            QueryTime: 5_000_000,
            ..DISK_PERFORMANCE::default()
        };
        assert_eq!(
            compute_disk_delta(&mut prev, 0, reset, t2),
            (0, 0, 0, 0),
            "counter reset must emit zeros rather than spikes"
        );

        // Next monotonic tick recovers cleanly from the reset baseline.
        let recovered = DISK_PERFORMANCE {
            BytesRead: 110_000,
            BytesWritten: 55_000,
            ReadTime: 11_000,
            WriteTime: 10_500,
            IdleTime: 8_500_000,
            ReadCount: 4,
            WriteCount: 3,
            QueryTime: 15_000_000,
            ..DISK_PERFORMANCE::default()
        };
        assert_eq!(
            compute_disk_delta(&mut prev, 0, recovered, t3),
            (250, 100_000, 50_000, 500)
        );
    }
}
