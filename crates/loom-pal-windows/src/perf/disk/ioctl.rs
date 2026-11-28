//! Low-level physical disk & volume `DeviceIoControl` helpers using stack-allocated buffers.

use std::ffi::c_void;
use std::mem::{offset_of, size_of};
use std::ptr;
use std::sync::Arc;

use loom_types::DiskMediaType;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{
    BusTypeNvme, CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE,
    IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DEVICE_SEEK_PENALTY_DESCRIPTOR, DISK_EXTENT, DISK_GEOMETRY_EX, DISK_PERFORMANCE,
    GET_LENGTH_INFORMATION, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_DISK_GET_LENGTH_INFO,
    IOCTL_DISK_PERFORMANCE, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
    STORAGE_DEVICE_DESCRIPTOR, STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
    StorageDeviceSeekPenaltyProperty, VOLUME_DISK_EXTENTS,
};

use crate::handle::{OwnedHandle, to_wide_null};

/// Cached static attributes for a physical disk (`\\.\PhysicalDrive{N}`).
#[derive(Debug, Clone)]
pub(super) struct StaticDiskInfo {
    pub(super) model: Arc<str>,
    pub(super) media_type: DiskMediaType,
    pub(super) capacity_bytes: u64,
}

/// Opens a device or volume with metadata-only (`dwDesiredAccess = 0`) non-exclusive access,
/// which works without Administrator elevation on Windows 10/11.
pub(super) fn open_device_metadata(path: &str) -> Option<OwnedHandle> {
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
pub(super) fn query_volume_disk_extents(handle: HANDLE) -> Vec<(u32, u64)> {
    // 8-byte aligned stack buffer holding `VOLUME_DISK_EXTENTS` header + up to 16 `DISK_EXTENT`s.
    const BUF_U64S: usize = (size_of::<VOLUME_DISK_EXTENTS>() + 15 * size_of::<DISK_EXTENT>())
        .div_ceil(size_of::<u64>());
    let mut buf = [0u64; BUF_U64S];
    let mut bytes_returned: u32 = 0;

    // SAFETY: `buf` is 8-byte aligned and sized for `VOLUME_DISK_EXTENTS` + trailing extents.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            ptr::null(),
            0,
            buf.as_mut_ptr().cast::<c_void>(),
            std::mem::size_of_val(&buf) as u32,
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
pub(super) fn query_static_disk_info(
    handle: HANDLE,
    index: u32,
    fallback_capacity: u64,
) -> StaticDiskInfo {
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
    let mut out_buf = [0u8; 1024];
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
pub(super) fn query_disk_performance(handle: HANDLE) -> Option<DISK_PERFORMANCE> {
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
pub(super) fn query_volumes_perf(drive_letters: &[char]) -> Option<DISK_PERFORMANCE> {
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

/// Extracts a trimmed ASCII C-string at `offset` inside `buf`.
fn read_c_string_at(buf: &[u8], offset: usize) -> String {
    if offset == 0 || offset >= buf.len() {
        return String::new();
    }
    let slice = &buf[offset..];
    let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    String::from_utf8_lossy(&slice[..end]).trim().to_owned()
}
