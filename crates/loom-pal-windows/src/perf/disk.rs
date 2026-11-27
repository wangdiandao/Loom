//! Windows slow-lane physical disk telemetry via `DeviceIoControl` (`IOCTL_DISK_PERFORMANCE`,
//! `IOCTL_STORAGE_QUERY_PROPERTY`, `IOCTL_DISK_GET_LENGTH_INFO`, `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`)
//! guarded by a per-device `300 ms` [`CircuitBreaker`] and mandatory `DRIVE_REMOTE` bypass.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::c_void;
use std::mem::{offset_of, size_of};
use std::ptr;
use std::sync::Arc;
use std::time::Instant;

use loom_types::{DiskDeviceSample, DiskMediaType};
use parking_lot::Mutex;
use windows_sys::Win32::Foundation::HANDLE;
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
use windows_sys::Win32::System::WindowsProgramming::{DRIVE_CDROM, DRIVE_REMOTE};

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

/// Computes `(active_time_permille, read_bytes_per_sec, write_bytes_per_sec, avg_response_us)`
/// from two consecutive `DISK_PERFORMANCE` samples.
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

    let dt_query_100ns = (curr.QueryTime - prev.query_time_100ns).max(0) as u64;
    let dt_wall_100ns = (now.saturating_duration_since(prev.sampled_at).as_nanos() / 100) as u64;
    let dt_100ns = if dt_query_100ns > 0 {
        dt_query_100ns
    } else {
        dt_wall_100ns
    };
    if dt_100ns == 0 {
        return (0, 0, 0, 0);
    }

    let d_read = (curr.BytesRead - prev.bytes_read).max(0) as u64;
    let d_write = (curr.BytesWritten - prev.bytes_written).max(0) as u64;
    let read_bps =
        ((u128::from(d_read) * 10_000_000) / u128::from(dt_100ns)).min(u128::from(u64::MAX)) as u64;
    let write_bps = ((u128::from(d_write) * 10_000_000) / u128::from(dt_100ns))
        .min(u128::from(u64::MAX)) as u64;

    let d_idle = (curr.IdleTime - prev.idle_time_100ns).max(0) as u64;
    let active_100ns = dt_100ns.saturating_sub(d_idle);
    let active_permille =
        ((u128::from(active_100ns) * 1_000) / u128::from(dt_100ns)).min(1_000) as u16;

    let d_ops = u64::from(curr.ReadCount.wrapping_sub(prev.read_count))
        .saturating_add(u64::from(curr.WriteCount.wrapping_sub(prev.write_count)));
    let d_io_100ns = ((curr.ReadTime - prev.read_time_100ns).max(0) as u64)
        .saturating_add((curr.WriteTime - prev.write_time_100ns).max(0) as u64);
    let avg_response_us = if d_ops > 0 {
        d_io_100ns / (d_ops.saturating_mul(10))
    } else {
        0
    };

    (active_permille, read_bps, write_bps, avg_response_us)
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
