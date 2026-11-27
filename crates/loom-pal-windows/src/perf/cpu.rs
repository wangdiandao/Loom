//! Windows CPU dynamic frequency, static topology/cache metadata, and NT memory-list
//! composition queries (zero WMI/PDH).

use std::collections::BTreeSet;
use std::ffi::c_void;
use std::mem::{offset_of, size_of, zeroed};
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
    CACHE_RELATIONSHIP, GROUP_AFFINITY, GetLogicalProcessorInformationEx, GlobalMemoryStatusEx,
    MEMORYSTATUSEX, RelationAll, RelationCache, RelationGroup, RelationProcessorCore,
    RelationProcessorPackage, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
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
    let (sockets, physical_cores, l1_cache_bytes, l2_cache_bytes, l3_cache_bytes, hetero) =
        query_cpu_topology_and_caches();
    tracing::debug!(
        p_cores = hetero.performance_cores,
        e_cores = hetero.efficiency_cores,
        lpe_cores = hetero.low_power_efficiency_cores,
        logical_tiers = hetero.logical_tiers.len(),
        "probed heterogeneous CPU topology"
    );

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
        core_kinds: Vec::new(),
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

/// Architectural core tier for heterogeneous CPU topologies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CoreTier {
    /// Performance core (or homogeneous standard core).
    Performance,
    /// Efficiency core sharing last-level L3 cache.
    Efficiency,
    /// Low-power efficiency core (lowest efficiency tier or isolated without L3 cache).
    LowPowerEfficiency,
}

/// Raw physical core descriptor extracted from `GetLogicalProcessorInformationEx`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RawCoreTopology {
    /// Processor group index (`GROUP_AFFINITY.Group`).
    pub(crate) group: u16,
    /// Bitmask of logical processors belonging to this physical core within `group`.
    pub(crate) mask: usize,
    /// Windows `PROCESSOR_RELATIONSHIP.EfficiencyClass` (higher = higher performance).
    pub(crate) efficiency_class: u8,
}

/// Raw L3 cache descriptor extracted from `RelationCache` (`Level == 3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RawL3CacheMask {
    /// Processor group index (`GROUP_AFFINITY.Group`).
    pub(crate) group: u16,
    /// Bitmask of logical processors sharing this L3 cache within `group`.
    pub(crate) mask: usize,
}

/// Per-logical-processor heterogeneous classification summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeterogeneousTopology {
    /// Per-logical-CPU tier indexed by global logical processor index (`0..total_logical`).
    pub(crate) logical_tiers: Vec<CoreTier>,
    /// Count of physical Performance (`P`) cores.
    pub(crate) performance_cores: u32,
    /// Count of physical Efficiency (`E`) cores.
    pub(crate) efficiency_cores: u32,
    /// Count of physical Low-Power Efficiency (`LP-E`) cores.
    pub(crate) low_power_efficiency_cores: u32,
}

/// Classifies logical processors into [`CoreTier::Performance`] (`P`),
/// [`CoreTier::Efficiency`] (`E`), and [`CoreTier::LowPowerEfficiency`] (`LP-E`) tiers
/// from `RelationProcessorCore` `EfficiencyClass` values and `RelationCache` L3 sharing masks.
///
/// Classification rules:
/// - Homogeneous CPUs (only one distinct `EfficiencyClass`): all cores are `Performance`.
/// - 3+ distinct `EfficiencyClass` tiers: highest -> `Performance`, lowest -> `LowPowerEfficiency`,
///   intermediate tiers -> `Efficiency`.
/// - 2 distinct `EfficiencyClass` tiers (e.g., Intel Alder/Raptor/Meteor Lake):
///   - highest class -> `Performance`.
///   - lowest class -> `LowPowerEfficiency` when L3 cache records exist on the system and this core
///     does NOT share any L3 cache (SoC-tile LP-E cores); otherwise `Efficiency`.
pub(crate) fn classify_heterogeneous_topology(
    cores: &[RawCoreTopology],
    l3_caches: &[RawL3CacheMask],
    group_logical_counts: &[u32],
) -> HeterogeneousTopology {
    let mut group_offsets: Vec<usize> = Vec::with_capacity(group_logical_counts.len());
    let mut total_logical: usize = 0;
    for &cnt in group_logical_counts {
        group_offsets.push(total_logical);
        total_logical = total_logical.saturating_add(cnt as usize);
    }

    let mut logical_tiers = vec![CoreTier::Performance; total_logical];
    if cores.is_empty() {
        return HeterogeneousTopology {
            logical_tiers,
            performance_cores: 0,
            efficiency_cores: 0,
            low_power_efficiency_cores: 0,
        };
    }

    let distinct_classes: BTreeSet<u8> = cores.iter().map(|c| c.efficiency_class).collect();
    let min_class = *distinct_classes.first().unwrap_or(&0);
    let max_class = *distinct_classes.last().unwrap_or(&0);
    let any_l3_present = !l3_caches.is_empty();

    let mut performance_cores: u32 = 0;
    let mut efficiency_cores: u32 = 0;
    let mut low_power_efficiency_cores: u32 = 0;

    for core in cores {
        let shares_l3 = l3_caches
            .iter()
            .any(|c| c.group == core.group && (c.mask & core.mask) != 0);

        let tier = if distinct_classes.len() <= 1 {
            CoreTier::Performance
        } else if distinct_classes.len() >= 3 {
            if core.efficiency_class == max_class {
                CoreTier::Performance
            } else if core.efficiency_class == min_class {
                CoreTier::LowPowerEfficiency
            } else {
                CoreTier::Efficiency
            }
        } else if core.efficiency_class == max_class {
            CoreTier::Performance
        } else if any_l3_present && !shares_l3 {
            CoreTier::LowPowerEfficiency
        } else {
            CoreTier::Efficiency
        };

        match tier {
            CoreTier::Performance => performance_cores = performance_cores.saturating_add(1),
            CoreTier::Efficiency => efficiency_cores = efficiency_cores.saturating_add(1),
            CoreTier::LowPowerEfficiency => {
                low_power_efficiency_cores = low_power_efficiency_cores.saturating_add(1)
            }
        }

        if let Some(&base_idx) = group_offsets.get(usize::from(core.group)) {
            let group_limit = group_logical_counts
                .get(usize::from(core.group))
                .copied()
                .unwrap_or(0) as usize;
            let mut bits = core.mask;
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if bit < group_limit {
                    let global_idx = base_idx + bit;
                    if let Some(slot) = logical_tiers.get_mut(global_idx) {
                        *slot = tier;
                    }
                }
            }
        }
    }

    HeterogeneousTopology {
        logical_tiers,
        performance_cores,
        efficiency_cores,
        low_power_efficiency_cores,
    }
}

/// Probes live heterogeneous core topology (`P` / `E` / `LP-E`) via
/// `GetLogicalProcessorInformationEx(RelationAll, ...)`.
#[allow(dead_code)]
pub(crate) fn probe_heterogeneous_topology() -> HeterogeneousTopology {
    let (_, _, _, _, _, topo) = query_cpu_topology_and_caches();
    topo
}

/// Queries CPU package count (sockets), physical core count, total L1/L2/L3 cache bytes,
/// and heterogeneous `P`/`E`/`LP-E` core topology via
/// `GetLogicalProcessorInformationEx(RelationAll, ...)`.
fn query_cpu_topology_and_caches() -> (u32, u32, u64, u64, u64, HeterogeneousTopology) {
    let empty_topo = || {
        let count = query_logical_cpu_count() as usize;
        HeterogeneousTopology {
            logical_tiers: vec![CoreTier::Performance; count],
            performance_cores: 0,
            efficiency_cores: 0,
            low_power_efficiency_cores: 0,
        }
    };

    let mut needed: u32 = 0;
    // SAFETY: Null buffer queries the required output byte length.
    unsafe {
        GetLogicalProcessorInformationEx(RelationAll, ptr::null_mut(), &mut needed);
    }
    if needed == 0 {
        // SAFETY: `GetLastError` checks whether buffer size discovery succeeded.
        let err = unsafe { GetLastError() };
        if err != ERROR_INSUFFICIENT_BUFFER {
            return (0, 0, 0, 0, 0, empty_topo());
        }
    }
    if needed == 0 || needed > 4 * 1024 * 1024 {
        return (0, 0, 0, 0, 0, empty_topo());
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
        return (0, 0, 0, 0, 0, empty_topo());
    }

    let valid_bytes = (bytes_len as usize).min(buf.len() * size_of::<u64>());
    let base = buf.as_ptr().cast::<u8>();
    let mut offset = 0usize;

    let mut sockets: u32 = 0;
    let mut physical_cores: u32 = 0;
    let mut l1_bytes: u64 = 0;
    let mut l2_bytes: u64 = 0;
    let mut l3_bytes: u64 = 0;
    let mut raw_cores: Vec<RawCoreTopology> = Vec::new();
    let mut l3_caches: Vec<RawL3CacheMask> = Vec::new();
    let mut active_groups: u16 = 0;

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
            // SAFETY: `info.Relationship == RelationProcessorCore` selects `Processor`.
            let proc_rel = unsafe { info.Anonymous.Processor };
            let mask = proc_rel.GroupMask[0];
            raw_cores.push(RawCoreTopology {
                group: mask.Group,
                mask: mask.Mask,
                efficiency_class: proc_rel.EfficiencyClass,
            });
        } else if info.Relationship == RelationCache {
            // SAFETY: `info.Relationship == RelationCache` selects the `Cache` union variant.
            let cache = unsafe { info.Anonymous.Cache };
            match cache.Level {
                1 => l1_bytes = l1_bytes.saturating_add(u64::from(cache.CacheSize)),
                2 => l2_bytes = l2_bytes.saturating_add(u64::from(cache.CacheSize)),
                3 => {
                    l3_bytes = l3_bytes.saturating_add(u64::from(cache.CacheSize));
                    let group_count = usize::from(cache.GroupCount).max(1);
                    let masks_base_off = offset
                        + offset_of!(SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, Anonymous)
                        + offset_of!(CACHE_RELATIONSHIP, Anonymous);
                    let entry_end = offset + entry_size;
                    for g_idx in 0..group_count {
                        let mask_off = masks_base_off + g_idx * size_of::<GROUP_AFFINITY>();
                        if mask_off + size_of::<GROUP_AFFINITY>() > entry_end {
                            break;
                        }
                        // SAFETY: Verified `mask_off..mask_off + size_of::<GROUP_AFFINITY>()`
                        // lies strictly inside the current `RelationCache` record.
                        let ga = unsafe {
                            ptr::read_unaligned(base.add(mask_off).cast::<GROUP_AFFINITY>())
                        };
                        if ga.Mask != 0 {
                            l3_caches.push(RawL3CacheMask {
                                group: ga.Group,
                                mask: ga.Mask,
                            });
                        }
                    }
                }
                _ => {}
            }
        } else if info.Relationship == RelationGroup {
            // SAFETY: `info.Relationship == RelationGroup` selects `Group`.
            let grp = unsafe { info.Anonymous.Group };
            active_groups = active_groups.max(grp.ActiveGroupCount);
        }

        offset = offset.saturating_add(entry_size);
    }

    let group_count = usize::from(active_groups.max(1));
    let mut group_logical_counts = Vec::with_capacity(group_count);
    for g in 0..group_count {
        // SAFETY: `GetActiveProcessorCount` safely accepts any `u16` group number.
        let cnt =
            unsafe { windows_sys::Win32::System::Threading::GetActiveProcessorCount(g as u16) };
        if cnt > 0 {
            group_logical_counts.push(cnt);
        } else if g == 0 {
            group_logical_counts.push(query_logical_cpu_count());
        }
    }

    let topo = classify_heterogeneous_topology(&raw_cores, &l3_caches, &group_logical_counts);
    (sockets, physical_cores, l1_bytes, l2_bytes, l3_bytes, topo)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_homogeneous_topology_marks_all_cores_performance() {
        let cores = vec![
            RawCoreTopology {
                group: 0,
                mask: 0b0011,
                efficiency_class: 0,
            },
            RawCoreTopology {
                group: 0,
                mask: 0b1100,
                efficiency_class: 0,
            },
        ];
        let l3 = vec![RawL3CacheMask {
            group: 0,
            mask: 0b1111,
        }];
        let topo = classify_heterogeneous_topology(&cores, &l3, &[4]);
        assert_eq!(topo.performance_cores, 2);
        assert_eq!(topo.efficiency_cores, 0);
        assert_eq!(topo.low_power_efficiency_cores, 0);
        assert_eq!(topo.logical_tiers, vec![CoreTier::Performance; 4]);
    }

    #[test]
    fn classify_two_tier_hybrid_topology_separates_p_and_e_cores() {
        // 2 SMT P-cores (LP 0..3, EfficiencyClass=1) + 4 E-cores (LP 4..7, EfficiencyClass=0),
        // all sharing a unified L3 cache (Alder Lake / Raptor Lake layout).
        let cores = vec![
            RawCoreTopology {
                group: 0,
                mask: 0b0000_0011,
                efficiency_class: 1,
            },
            RawCoreTopology {
                group: 0,
                mask: 0b0000_1100,
                efficiency_class: 1,
            },
            RawCoreTopology {
                group: 0,
                mask: 0b0001_0000,
                efficiency_class: 0,
            },
            RawCoreTopology {
                group: 0,
                mask: 0b0010_0000,
                efficiency_class: 0,
            },
            RawCoreTopology {
                group: 0,
                mask: 0b0100_0000,
                efficiency_class: 0,
            },
            RawCoreTopology {
                group: 0,
                mask: 0b1000_0000,
                efficiency_class: 0,
            },
        ];
        let l3 = vec![RawL3CacheMask {
            group: 0,
            mask: 0b1111_1111,
        }];
        let topo = classify_heterogeneous_topology(&cores, &l3, &[8]);
        assert_eq!(topo.performance_cores, 2);
        assert_eq!(topo.efficiency_cores, 4);
        assert_eq!(topo.low_power_efficiency_cores, 0);
        assert_eq!(
            topo.logical_tiers,
            vec![
                CoreTier::Performance,
                CoreTier::Performance,
                CoreTier::Performance,
                CoreTier::Performance,
                CoreTier::Efficiency,
                CoreTier::Efficiency,
                CoreTier::Efficiency,
                CoreTier::Efficiency,
            ]
        );
    }

    #[test]
    fn classify_meteor_lake_three_tier_topology_uses_l3_absence_for_lpe_cores() {
        // 1 SMT P-core (LP 0..1, eff=1, L3) + 2 E-cores (LP 2..3, eff=0, L3)
        // + 2 SoC LP-E cores (LP 4..5, eff=0, NO L3).
        let cores = vec![
            RawCoreTopology {
                group: 0,
                mask: 0b00_0011,
                efficiency_class: 1,
            },
            RawCoreTopology {
                group: 0,
                mask: 0b00_0100,
                efficiency_class: 0,
            },
            RawCoreTopology {
                group: 0,
                mask: 0b00_1000,
                efficiency_class: 0,
            },
            RawCoreTopology {
                group: 0,
                mask: 0b01_0000,
                efficiency_class: 0,
            },
            RawCoreTopology {
                group: 0,
                mask: 0b10_0000,
                efficiency_class: 0,
            },
        ];
        let l3 = vec![RawL3CacheMask {
            group: 0,
            mask: 0b00_1111,
        }];
        let topo = classify_heterogeneous_topology(&cores, &l3, &[6]);
        assert_eq!(topo.performance_cores, 1);
        assert_eq!(topo.efficiency_cores, 2);
        assert_eq!(topo.low_power_efficiency_cores, 2);
        assert_eq!(
            topo.logical_tiers,
            vec![
                CoreTier::Performance,
                CoreTier::Performance,
                CoreTier::Efficiency,
                CoreTier::Efficiency,
                CoreTier::LowPowerEfficiency,
                CoreTier::LowPowerEfficiency,
            ]
        );
    }

    #[test]
    fn classify_explicit_three_class_topology_maps_p_e_and_lpe() {
        let cores = vec![
            RawCoreTopology {
                group: 0,
                mask: 0b001,
                efficiency_class: 2,
            },
            RawCoreTopology {
                group: 0,
                mask: 0b010,
                efficiency_class: 1,
            },
            RawCoreTopology {
                group: 0,
                mask: 0b100,
                efficiency_class: 0,
            },
        ];
        let topo = classify_heterogeneous_topology(&cores, &[], &[3]);
        assert_eq!(topo.performance_cores, 1);
        assert_eq!(topo.efficiency_cores, 1);
        assert_eq!(topo.low_power_efficiency_cores, 1);
        assert_eq!(
            topo.logical_tiers,
            vec![
                CoreTier::Performance,
                CoreTier::Efficiency,
                CoreTier::LowPowerEfficiency,
            ]
        );
    }

    #[test]
    fn classify_multi_group_topology_maps_global_logical_indices() {
        let cores = vec![
            RawCoreTopology {
                group: 0,
                mask: 0b01,
                efficiency_class: 1,
            },
            RawCoreTopology {
                group: 0,
                mask: 0b10,
                efficiency_class: 0,
            },
            RawCoreTopology {
                group: 1,
                mask: 0b01,
                efficiency_class: 0,
            },
        ];
        let l3 = vec![RawL3CacheMask {
            group: 0,
            mask: 0b11,
        }];
        let topo = classify_heterogeneous_topology(&cores, &l3, &[2, 1]);
        assert_eq!(
            topo.logical_tiers,
            vec![
                CoreTier::Performance,
                CoreTier::Efficiency,
                CoreTier::LowPowerEfficiency,
            ]
        );
    }

    #[test]
    fn probe_heterogeneous_topology_matches_logical_cpu_count() {
        let topo = probe_heterogeneous_topology();
        let expected = query_logical_cpu_count() as usize;
        assert_eq!(topo.logical_tiers.len(), expected);
        assert!(
            topo.performance_cores + topo.efficiency_cores + topo.low_power_efficiency_cores >= 1
        );
    }
}
