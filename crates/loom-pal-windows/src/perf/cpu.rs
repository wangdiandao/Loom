//! Windows CPU dynamic frequency, static topology/cache metadata, and NT memory-list
//! composition queries (zero WMI/PDH).

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr;
use std::sync::Arc;

use loom_types::{CpuStaticInfo, HardwareStaticInfo, SystemPerfSample};
use windows_sys::Wdk::System::SystemInformation::NtQuerySystemInformation;
use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, GetLastError, STATUS_SUCCESS,
};
use windows_sys::Win32::System::Power::{
    CallNtPowerInformation, PROCESSOR_POWER_INFORMATION, ProcessorInformation,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_DWORD, REG_SZ, RegCloseKey, RegOpenKeyExW,
    RegQueryValueExW,
};
use windows_sys::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, GlobalMemoryStatusEx, MEMORYSTATUSEX, RelationAll,
    RelationCache, RelationProcessorCore, RelationProcessorPackage,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};
use windows_sys::Win32::System::Threading::{IsProcessorFeaturePresent, PF_VIRT_FIRMWARE_ENABLED};

use crate::fast_sampler::query_logical_cpu_count;
use crate::handle::to_wide_null;
use crate::perf::smbios::probe_memory_hardware_info;

/// NT `SYSTEM_INFORMATION_CLASS::SystemMemoryListInformation` (`80`).
const SYSTEM_MEMORY_LIST_INFORMATION_CLASS: i32 = 80;

/// Windows NT `SYSTEM_MEMORY_LIST_INFORMATION` ABI layout (Vista / Windows 10 / 11).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NtSystemMemoryListInformation {
    zero_page_count: usize,
    free_page_count: usize,
    modified_page_count: usize,
    modified_no_write_page_count: usize,
    bad_page_count: usize,
    page_count_by_priority: [usize; 8],
    repurposed_pages_by_priority: [usize; 8],
    modified_page_count_page_file: usize,
}

/// Samples current dynamic and base CPU frequencies (MHz) across all logical processors via
/// `CallNtPowerInformation(ProcessorInformation, ...)`.
///
/// Reuses `power_buf` across calls to avoid per-tick heap allocations.
pub(crate) fn sample_cpu_frequencies(
    power_buf: &mut Vec<PROCESSOR_POWER_INFORMATION>,
    logical_cpu_count: u32,
) -> (u32, u32) {
    let count = (logical_cpu_count as usize).max(1);
    if power_buf.len() < count {
        power_buf.resize(count, PROCESSOR_POWER_INFORMATION::default());
    }

    let out_bytes = (count * size_of::<PROCESSOR_POWER_INFORMATION>()) as u32;
    // SAFETY: `power_buf` has at least `count` initialized `PROCESSOR_POWER_INFORMATION` slots.
    let status = unsafe {
        CallNtPowerInformation(
            ProcessorInformation,
            ptr::null(),
            0,
            power_buf.as_mut_ptr().cast::<c_void>(),
            out_bytes,
        )
    };
    if status != STATUS_SUCCESS {
        return (0, 0);
    }

    let mut cur_sum: u64 = 0;
    let mut cur_nonzero: u64 = 0;
    let mut max_base: u32 = 0;

    for info in &power_buf[..count] {
        if info.CurrentMhz > 0 {
            cur_sum = cur_sum.saturating_add(u64::from(info.CurrentMhz));
            cur_nonzero += 1;
        }
        if info.MaxMhz > max_base {
            max_base = info.MaxMhz;
        }
    }

    let current_mhz = cur_sum
        .checked_div(cur_nonzero)
        .map_or(max_base, |avg| avg as u32);

    (current_mhz, max_base)
}

/// Populates `mem_modified_bytes`, `mem_standby_bytes`, and `mem_free_bytes` using
/// `NtQuerySystemInformation(SystemMemoryListInformation)` with a safe fallback when
/// the kernel denies access (e.g., restricted tokens without `SeProfileSingleProcessPrivilege`).
pub(crate) fn populate_memory_list_composition(
    sample: &mut SystemPerfSample,
    page_size: u64,
    system_cache_pages: usize,
) {
    let page = if page_size > 0 { page_size } else { 4096 };
    let mut mem_list = NtSystemMemoryListInformation::default();
    let mut return_len: u32 = 0;

    // SAFETY: `mem_list` is POD and matches the exact `SYSTEM_MEMORY_LIST_INFORMATION` ABI.
    let status = unsafe {
        NtQuerySystemInformation(
            SYSTEM_MEMORY_LIST_INFORMATION_CLASS,
            (&mut mem_list as *mut NtSystemMemoryListInformation).cast::<c_void>(),
            size_of::<NtSystemMemoryListInformation>() as u32,
            &mut return_len,
        )
    };

    if status == STATUS_SUCCESS {
        let modified_pages = mem_list
            .modified_page_count
            .saturating_add(mem_list.modified_no_write_page_count);
        let standby_pages = mem_list
            .page_count_by_priority
            .iter()
            .copied()
            .fold(0usize, usize::saturating_add);
        let free_pages = mem_list
            .zero_page_count
            .saturating_add(mem_list.free_page_count);

        sample.mem_modified_bytes = (modified_pages as u64).saturating_mul(page);
        sample.mem_standby_bytes = (standby_pages as u64).saturating_mul(page);
        sample.mem_free_bytes = (free_pages as u64).saturating_mul(page);
    } else {
        // Safe fallback derived from `GlobalMemoryStatusEx` + `GetPerformanceInfo`.
        let standby = (system_cache_pages as u64)
            .saturating_mul(page)
            .min(sample.mem_available_bytes);
        sample.mem_modified_bytes = 0;
        sample.mem_standby_bytes = standby;
        sample.mem_free_bytes = sample.mem_available_bytes.saturating_sub(standby);
    }
}

/// Probes static CPU topology, cache hierarchy, brand string, base frequency, and
/// virtualization status.
pub fn probe_cpu_static_info() -> CpuStaticInfo {
    let logical_processors = query_logical_cpu_count();
    let (brand_name, reg_mhz) = read_cpu_registry_metadata();
    let (sockets, physical_cores, l1_cache_bytes, l2_cache_bytes, l3_cache_bytes) =
        query_cpu_topology_and_caches();

    let mut power_buf = Vec::new();
    let (_, power_base_mhz) = sample_cpu_frequencies(&mut power_buf, logical_processors);
    let base_freq_mhz = if power_base_mhz > 0 {
        power_base_mhz
    } else {
        reg_mhz
    };

    // SAFETY: `IsProcessorFeaturePresent` takes a documented `PF_*` constant.
    let virt_enabled = unsafe { IsProcessorFeaturePresent(PF_VIRT_FIRMWARE_ENABLED) } != 0;

    CpuStaticInfo {
        brand_name: Arc::from(brand_name),
        base_freq_mhz,
        sockets: sockets.max(1),
        physical_cores: if physical_cores > 0 {
            physical_cores
        } else {
            logical_processors
        },
        logical_processors,
        virtualization_enabled: Some(virt_enabled),
        l1_cache_bytes,
        l2_cache_bytes,
        l3_cache_bytes,
    }
}

/// Probes combined static CPU and SMBIOS physical memory hardware metadata.
pub fn probe_hardware_static_info() -> HardwareStaticInfo {
    let cpu = probe_cpu_static_info();

    // SAFETY: `MEMORYSTATUSEX` is POD; `dwLength` is set before calling `GlobalMemoryStatusEx`.
    let mut mem_status: MEMORYSTATUSEX = unsafe { zeroed() };
    mem_status.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    let mem_total_bytes = if unsafe { GlobalMemoryStatusEx(&mut mem_status) } != 0 {
        mem_status.ullTotalPhys
    } else {
        0
    };

    let memory = probe_memory_hardware_info(mem_total_bytes);
    HardwareStaticInfo { cpu, memory }
}

/// Queries CPU package count (sockets), physical core count, and total L1/L2/L3 cache bytes
/// via `GetLogicalProcessorInformationEx(RelationAll, ...)`.
fn query_cpu_topology_and_caches() -> (u32, u32, u64, u64, u64) {
    let mut needed: u32 = 0;
    // SAFETY: Null buffer queries the required output byte length.
    unsafe {
        GetLogicalProcessorInformationEx(RelationAll, ptr::null_mut(), &mut needed);
    }
    if needed == 0 {
        // SAFETY: `GetLastError` checks whether buffer size discovery succeeded.
        let err = unsafe { GetLastError() };
        if err != ERROR_INSUFFICIENT_BUFFER {
            return (0, 0, 0, 0, 0);
        }
    }
    if needed == 0 || needed > 4 * 1024 * 1024 {
        return (0, 0, 0, 0, 0);
    }

    // Allocate 8-byte aligned backing storage.
    let mut buf: Vec<u64> = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
    let mut bytes_len = (buf.len() * size_of::<u64>()) as u32;
    // SAFETY: `buf` is 8-byte aligned and writable for `bytes_len` bytes.
    let ok = unsafe {
        GetLogicalProcessorInformationEx(
            RelationAll,
            buf.as_mut_ptr()
                .cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>(),
            &mut bytes_len,
        )
    };
    if ok == 0 {
        return (0, 0, 0, 0, 0);
    }

    let valid_bytes = (bytes_len as usize).min(buf.len() * size_of::<u64>());
    let base = buf.as_ptr().cast::<u8>();
    let mut offset = 0usize;

    let mut sockets: u32 = 0;
    let mut physical_cores: u32 = 0;
    let mut l1_bytes: u64 = 0;
    let mut l2_bytes: u64 = 0;
    let mut l3_bytes: u64 = 0;

    while offset + 8 <= valid_bytes {
        // SAFETY: Header (`Relationship` + `Size`) lies within `valid_bytes`.
        let info = unsafe {
            ptr::read_unaligned(
                base.add(offset)
                    .cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>(),
            )
        };
        let entry_size = info.Size as usize;
        if entry_size < 8 || offset.saturating_add(entry_size) > valid_bytes {
            break;
        }

        if info.Relationship == RelationProcessorPackage {
            sockets = sockets.saturating_add(1);
        } else if info.Relationship == RelationProcessorCore {
            physical_cores = physical_cores.saturating_add(1);
        } else if info.Relationship == RelationCache {
            // SAFETY: `info.Relationship == RelationCache` selects the `Cache` union variant.
            let cache = unsafe { info.Anonymous.Cache };
            match cache.Level {
                1 => l1_bytes = l1_bytes.saturating_add(u64::from(cache.CacheSize)),
                2 => l2_bytes = l2_bytes.saturating_add(u64::from(cache.CacheSize)),
                3 => l3_bytes = l3_bytes.saturating_add(u64::from(cache.CacheSize)),
                _ => {}
            }
        }

        offset = offset.saturating_add(entry_size);
    }

    (sockets, physical_cores, l1_bytes, l2_bytes, l3_bytes)
}

/// Reads `ProcessorNameString` and `~MHz` from `HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0`.
fn read_cpu_registry_metadata() -> (String, u32) {
    let subkey = to_wide_null(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0");
    let mut hkey: HKEY = ptr::null_mut();

    // SAFETY: Standard read-only registry open under `HKEY_LOCAL_MACHINE`.
    let rc = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, subkey.as_ptr(), 0, KEY_READ, &mut hkey) };
    if rc != ERROR_SUCCESS || hkey.is_null() {
        return (String::new(), 0);
    }

    let brand = read_reg_string(hkey, "ProcessorNameString").unwrap_or_default();
    let mhz = read_reg_dword(hkey, "~MHz").unwrap_or(0);

    // SAFETY: `hkey` was successfully opened by `RegOpenKeyExW`.
    unsafe {
        RegCloseKey(hkey);
    }

    (brand.trim().to_owned(), mhz)
}

/// Reads a `REG_SZ` string value from an open registry key.
pub(crate) fn read_reg_string(hkey: HKEY, value_name: &str) -> Option<String> {
    let wide_name = to_wide_null(value_name);
    let mut reg_type: u32 = 0;
    let mut data_bytes: u32 = 0;

    // SAFETY: Size discovery call with null output pointer.
    let rc = unsafe {
        RegQueryValueExW(
            hkey,
            wide_name.as_ptr(),
            ptr::null(),
            &mut reg_type,
            ptr::null_mut(),
            &mut data_bytes,
        )
    };
    if rc != ERROR_SUCCESS || reg_type != REG_SZ || data_bytes == 0 || data_bytes > 65_536 {
        return None;
    }

    let mut wide_buf = vec![0u16; (data_bytes as usize).div_ceil(size_of::<u16>())];
    let mut out_bytes = (wide_buf.len() * size_of::<u16>()) as u32;
    // SAFETY: `wide_buf` is allocated to hold at least `data_bytes`.
    let rc2 = unsafe {
        RegQueryValueExW(
            hkey,
            wide_name.as_ptr(),
            ptr::null(),
            &mut reg_type,
            wide_buf.as_mut_ptr().cast::<u8>(),
            &mut out_bytes,
        )
    };
    if rc2 != ERROR_SUCCESS || reg_type != REG_SZ {
        return None;
    }

    let code_units = (out_bytes as usize / size_of::<u16>()).min(wide_buf.len());
    let slice = &wide_buf[..code_units];
    let len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    Some(String::from_utf16_lossy(&slice[..len]))
}

/// Reads a `REG_DWORD` `u32` value from an open registry key.
fn read_reg_dword(hkey: HKEY, value_name: &str) -> Option<u32> {
    let wide_name = to_wide_null(value_name);
    let mut reg_type: u32 = 0;
    let mut value: u32 = 0;
    let mut data_bytes = size_of::<u32>() as u32;

    // SAFETY: Output buffer is a valid 4-byte `u32`.
    let rc = unsafe {
        RegQueryValueExW(
            hkey,
            wide_name.as_ptr(),
            ptr::null(),
            &mut reg_type,
            (&mut value as *mut u32).cast::<u8>(),
            &mut data_bytes,
        )
    };
    if rc == ERROR_SUCCESS && reg_type == REG_DWORD && data_bytes == size_of::<u32>() as u32 {
        Some(value)
    } else {
        None
    }
}
