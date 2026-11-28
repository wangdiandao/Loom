//! Single-group (`<= 64` CPUs) and multi-group (`> 64` CPUs) Windows processor affinity queries
//! and updates (`GetProcessAffinityMask`, `GetProcessGroupAffinity`, `SetProcessDefaultCpuSetMasks`).

use std::ptr;

use loom_pal_api::PalError;
use loom_types::{CpuAffinityMask, ProcessKey};
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, GetLastError, HANDLE};
use windows_sys::Win32::System::SystemInformation::GROUP_AFFINITY;
use windows_sys::Win32::System::Threading::{
    GetActiveProcessorCount, GetActiveProcessorGroupCount, GetProcessAffinityMask,
    GetProcessDefaultCpuSetMasks, GetProcessGroupAffinity, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SET_INFORMATION, SetProcessAffinityMask, SetProcessDefaultCpuSetMasks,
};

use crate::handle::{map_win32_error, open_verified_process};

pub(super) fn get_affinity(
    key: ProcessKey,
    logical_cpus: u32,
) -> Result<CpuAffinityMask, PalError> {
    let handle = open_verified_process(key, PROCESS_QUERY_LIMITED_INFORMATION)?;

    if logical_cpus <= 64 {
        let mut proc_mask: usize = 0;
        let mut sys_mask: usize = 0;
        // SAFETY: `handle` has `PROCESS_QUERY_LIMITED_INFORMATION` and out pointers are stack locals.
        let ok = unsafe { GetProcessAffinityMask(handle.as_raw(), &mut proc_mask, &mut sys_mask) };
        if ok == 0 {
            // SAFETY: Reads last Win32 error set by `GetProcessAffinityMask`.
            let err = unsafe { GetLastError() };
            return Err(map_win32_error(err, Some(key), "GetProcessAffinityMask"));
        }

        let mut mask = CpuAffinityMask::empty(logical_cpus);
        if let Some(first) = mask.groups.first_mut() {
            *first = proc_mask as u64;
        }
        return Ok(mask);
    }

    // Multi-group (>64 logical CPUs) path via `GetProcessGroupAffinity`.
    let group_counts = query_processor_group_counts();
    let active_groups = query_process_group_affinity(handle.as_raw(), key)?;

    let single_group_proc_mask = if active_groups.len() == 1 {
        let mut proc_mask: usize = 0;
        let mut sys_mask: usize = 0;
        // SAFETY: `handle` has `PROCESS_QUERY_LIMITED_INFORMATION` and out pointers are stack locals.
        let ok = unsafe { GetProcessAffinityMask(handle.as_raw(), &mut proc_mask, &mut sys_mask) };
        if ok != 0 && proc_mask != 0 {
            Some(proc_mask as u64)
        } else {
            None
        }
    } else {
        None
    };

    let cpu_set_masks = if active_groups.len() > 1 {
        query_process_default_cpu_set_masks(handle.as_raw())
    } else {
        Vec::new()
    };

    Ok(build_affinity_mask_from_groups(
        logical_cpus,
        &group_counts,
        &active_groups,
        single_group_proc_mask,
        &cpu_set_masks,
    ))
}

pub(super) fn set_affinity(
    key: ProcessKey,
    mask: &CpuAffinityMask,
    logical_cpus: u32,
) -> Result<(), PalError> {
    if mask.is_empty() {
        return Err(PalError::InvalidArgument(
            "CPU affinity mask must enable at least one processor".into(),
        ));
    }

    if logical_cpus <= 64 {
        let group0 = mask.groups.first().copied().unwrap_or(0) as usize;
        if group0 == 0 {
            return Err(PalError::InvalidArgument(
                "primary processor group affinity mask cannot be empty".into(),
            ));
        }

        let handle = open_verified_process(key, PROCESS_SET_INFORMATION)?;
        // SAFETY: `handle` is validated and opened with `PROCESS_SET_INFORMATION`.
        let ok = unsafe { SetProcessAffinityMask(handle.as_raw(), group0) };
        if ok == 0 {
            // SAFETY: Reads last Win32 error set by `SetProcessAffinityMask`.
            let err = unsafe { GetLastError() };
            return Err(map_win32_error(err, Some(key), "SetProcessAffinityMask"));
        }

        return Ok(());
    }

    // Multi-group (>64 logical CPUs) path.
    let group_counts = query_processor_group_counts();
    let group_affinities = affinity_mask_to_group_affinities(mask, &group_counts);
    if group_affinities.is_empty() {
        return Err(PalError::InvalidArgument(
            "CPU affinity mask enables no active processors".into(),
        ));
    }

    let handle = open_verified_process(key, PROCESS_SET_INFORMATION)?;

    // When targeting a single processor group that the process already belongs to exclusively,
    // `SetProcessAffinityMask` enforces hard per-CPU affinity directly.
    if group_affinities.len() == 1
        && let Ok(current_groups) = query_process_group_affinity(handle.as_raw(), key)
        && current_groups.as_slice() == [group_affinities[0].Group]
    {
        // SAFETY: `handle` is validated and opened with `PROCESS_SET_INFORMATION`.
        let ok = unsafe { SetProcessAffinityMask(handle.as_raw(), group_affinities[0].Mask) };
        if ok != 0 {
            return Ok(());
        }
    }

    // Cross-group / multi-group affinity via `SetProcessDefaultCpuSetMasks`.
    // SAFETY: `group_affinities` is a valid contiguous slice of `GROUP_AFFINITY`.
    let ok = unsafe {
        SetProcessDefaultCpuSetMasks(
            handle.as_raw(),
            group_affinities.as_ptr(),
            group_affinities.len() as u16,
        )
    };
    if ok == 0 {
        // SAFETY: Reads last Win32 error set by `SetProcessDefaultCpuSetMasks`.
        let err = unsafe { GetLastError() };
        return Err(map_win32_error(
            err,
            Some(key),
            "SetProcessDefaultCpuSetMasks",
        ));
    }

    Ok(())
}

/// Queries the active logical processor count for each Windows processor group `0..group_count`.
fn query_processor_group_counts() -> Vec<u32> {
    // SAFETY: `GetActiveProcessorGroupCount` takes no pointers and is always safe to call.
    let group_count = unsafe { GetActiveProcessorGroupCount() };
    let mut counts = Vec::with_capacity(usize::from(group_count));
    for g in 0..group_count {
        // SAFETY: `g` is within `0..group_count`.
        let count = unsafe { GetActiveProcessorCount(g) };
        counts.push(count);
    }
    counts
}

/// Queries the Windows processor groups currently associated with `handle` via
/// `GetProcessGroupAffinity`.
fn query_process_group_affinity(handle: HANDLE, key: ProcessKey) -> Result<Vec<u16>, PalError> {
    let mut groups = vec![0u16; 16];
    for _ in 0..3 {
        let mut count = groups.len() as u16;
        // SAFETY: `handle` has `PROCESS_QUERY_LIMITED_INFORMATION` and `groups` has length `count`.
        let ok = unsafe { GetProcessGroupAffinity(handle, &mut count, groups.as_mut_ptr()) };
        if ok != 0 {
            groups.truncate(usize::from(count));
            return Ok(groups);
        }
        // SAFETY: Reads last Win32 error set by `GetProcessGroupAffinity`.
        let err = unsafe { GetLastError() };
        if err == ERROR_INSUFFICIENT_BUFFER && count > 0 {
            groups.resize(usize::from(count), 0);
            continue;
        }
        return Err(map_win32_error(err, Some(key), "GetProcessGroupAffinity"));
    }
    Err(PalError::OsError {
        code: ERROR_INSUFFICIENT_BUFFER as i32,
        message: "GetProcessGroupAffinity exceeded retry budget".into(),
    })
}

/// Queries custom per-group CPU set masks assigned to `handle` via
/// `GetProcessDefaultCpuSetMasks`, returning an empty `Vec` if no explicit set is configured.
fn query_process_default_cpu_set_masks(handle: HANDLE) -> Vec<GROUP_AFFINITY> {
    let mut required: u16 = 0;
    // SAFETY: Size discovery call with null buffer.
    unsafe {
        GetProcessDefaultCpuSetMasks(handle, ptr::null_mut(), 0, &mut required);
    }
    if required == 0 {
        return Vec::new();
    }
    let mut masks = vec![
        GROUP_AFFINITY {
            Mask: 0,
            Group: 0,
            Reserved: [0; 3],
        };
        usize::from(required)
    ];
    let mut actual: u16 = 0;
    // SAFETY: `masks` is sized to `required` entries.
    let ok =
        unsafe { GetProcessDefaultCpuSetMasks(handle, masks.as_mut_ptr(), required, &mut actual) };
    if ok == 0 {
        return Vec::new();
    }
    masks.truncate(usize::from(actual).min(masks.len()));
    masks
}

/// Maps Windows processor-group affinity results into a flat [`CpuAffinityMask`].
pub(super) fn build_affinity_mask_from_groups(
    logical_cpus: u32,
    group_counts: &[u32],
    active_groups: &[u16],
    single_group_proc_mask: Option<u64>,
    cpu_set_masks: &[GROUP_AFFINITY],
) -> CpuAffinityMask {
    let mut mask = CpuAffinityMask::empty(logical_cpus);

    // Precompute flat starting CPU index for each Windows processor group.
    let mut group_bases = Vec::with_capacity(group_counts.len());
    let mut base: u32 = 0;
    for &count in group_counts {
        group_bases.push(base);
        base = base.saturating_add(count);
    }

    // Prefer explicit CPU set masks when present on multi-group processes.
    if !cpu_set_masks.is_empty() {
        for ga in cpu_set_masks {
            let g_idx = usize::from(ga.Group);
            let Some(&g_base) = group_bases.get(g_idx) else {
                continue;
            };
            let g_count = group_counts[g_idx].min(usize::BITS);
            for bit in 0..g_count {
                if (ga.Mask & (1usize << bit)) != 0 {
                    mask.set(g_base + bit, true);
                }
            }
        }
        if !mask.is_empty() {
            return mask;
        }
    }

    if active_groups.len() == 1
        && let Some(proc_mask) = single_group_proc_mask
    {
        let g_idx = usize::from(active_groups[0]);
        if let Some(&g_base) = group_bases.get(g_idx) {
            let g_count = group_counts[g_idx].min(64);
            for bit in 0..g_count {
                if (proc_mask & (1u64 << bit)) != 0 {
                    mask.set(g_base + bit, true);
                }
            }
            if !mask.is_empty() {
                return mask;
            }
        }
    }

    for &group in active_groups {
        let g_idx = usize::from(group);
        let Some(&g_base) = group_bases.get(g_idx) else {
            continue;
        };
        let g_count = group_counts[g_idx];
        for bit in 0..g_count {
            mask.set(g_base + bit, true);
        }
    }

    if mask.is_empty() {
        CpuAffinityMask::all(logical_cpus)
    } else {
        mask
    }
}

/// Converts a flat [`CpuAffinityMask`] into non-empty Windows [`GROUP_AFFINITY`] entries
/// according to `group_counts`.
pub(super) fn affinity_mask_to_group_affinities(
    mask: &CpuAffinityMask,
    group_counts: &[u32],
) -> Vec<GROUP_AFFINITY> {
    let mut out = Vec::with_capacity(group_counts.len());
    let mut base: u32 = 0;

    for (g_idx, &count) in group_counts.iter().enumerate() {
        let clamped_count = count.min(usize::BITS);
        let mut group_mask: usize = 0;
        for bit in 0..clamped_count {
            if mask.is_enabled(base + bit) {
                group_mask |= 1usize << bit;
            }
        }
        if group_mask != 0
            && let Ok(group) = u16::try_from(g_idx)
        {
            out.push(GROUP_AFFINITY {
                Mask: group_mask,
                Group: group,
                Reserved: [0; 3],
            });
        }
        base = base.saturating_add(count);
    }

    out
}
