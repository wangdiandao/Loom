//! Windows process control, shell integration, elevation, and diagnostics operator.
//!
//! Every process-mutating method verifies [`ProcessKey::create_time_100ns`] before
//! acting, closes target handles immediately via an RAII handle guard, and maps Win32
//! `ERROR_ACCESS_DENIED` to [`PalError::ElevationRequired`].

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use loom_pal_api::{PalError, SystemOperator};
use loom_types::{
    CpuAffinityMask, ProcessKey, ProcessPriority, RunTaskRequest, TreeKillItem, TreeKillReport,
    WaitChainNode,
};
use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED, GetLastError, HANDLE,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::SystemInformation::GROUP_AFFINITY;
use windows_sys::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, GetActiveProcessorCount,
    GetActiveProcessorGroupCount, GetCurrentProcess, GetProcessAffinityMask,
    GetProcessDefaultCpuSetMasks, GetProcessGroupAffinity, GetProcessId, HIGH_PRIORITY_CLASS,
    IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, OpenProcessToken,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_STATE, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
    PROCESS_TERMINATE, ProcessPowerThrottling, QueryFullProcessImageNameW, REALTIME_PRIORITY_CLASS,
    SetPriorityClass, SetProcessAffinityMask, SetProcessDefaultCpuSetMasks, SetProcessInformation,
    TerminateProcess,
};
use windows_sys::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::Shell::{
    SEE_MASK_INVOKEIDLIST, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{SW_SHOW, SW_SHOWNORMAL};

use crate::fast_sampler::query_logical_cpu_count;
use crate::handle::{
    OwnedHandle, map_win32_error, null_wide_ptr, open_verified_process, os_str_to_wide_null,
    to_wide_null,
};
use crate::slow_prober::ensure_com_initialized;

/// Windows implementation of [`SystemOperator`].
#[derive(Debug, Default)]
pub struct WindowsSystemOperator;

impl WindowsSystemOperator {
    /// Creates a new Windows system operator.
    pub fn new() -> Self {
        Self
    }
}

impl SystemOperator for WindowsSystemOperator {
    fn run_new_task(&self, req: &RunTaskRequest) -> Result<Option<u32>, PalError> {
        let trimmed = req.command.trim();
        if trimmed.is_empty() {
            return Err(PalError::InvalidArgument("command cannot be empty".into()));
        }

        ensure_com_initialized();

        let (file, params) = split_command_and_args(trimmed);
        let wide_file = to_wide_null(file);
        let wide_params = if params.is_empty() {
            None
        } else {
            Some(to_wide_null(params))
        };
        let wide_verb = if req.run_as_admin {
            Some(to_wide_null("runas"))
        } else {
            None
        };

        // SAFETY: `SHELLEXECUTEINFOW` is POD; all pointed buffers outlive the call.
        let mut sei: SHELLEXECUTEINFOW = unsafe { zeroed() };
        sei.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        sei.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
        sei.lpVerb = wide_verb
            .as_ref()
            .map_or_else(null_wide_ptr, |v| v.as_ptr());
        sei.lpFile = wide_file.as_ptr();
        sei.lpParameters = wide_params
            .as_ref()
            .map_or_else(null_wide_ptr, |p| p.as_ptr());
        sei.nShow = SW_SHOWNORMAL;

        // SAFETY: `sei` fields are valid null-terminated UTF-16 pointers or null.
        let ok = unsafe { ShellExecuteExW(&mut sei) };
        if ok == 0 {
            // SAFETY: Reads last Win32 error set by `ShellExecuteExW`.
            let err = unsafe { GetLastError() };
            return Err(map_win32_error(err, None, "ShellExecuteExW(run_new_task)"));
        }

        let pid = OwnedHandle::from_raw(sei.hProcess).and_then(|proc_handle| {
            // SAFETY: `proc_handle` is a valid process handle returned by `ShellExecuteExW`.
            let raw_pid = unsafe { GetProcessId(proc_handle.as_raw()) };
            if raw_pid != 0 { Some(raw_pid) } else { None }
        });

        Ok(pid)
    }

    fn terminate_process(&self, key: ProcessKey) -> Result<(), PalError> {
        let handle = open_verified_process(key, PROCESS_TERMINATE)?;

        // SAFETY: `handle` is validated against `key` and opened with `PROCESS_TERMINATE`.
        let ok = unsafe { TerminateProcess(handle.as_raw(), 1) };
        if ok == 0 {
            // SAFETY: Reads last Win32 error set by `TerminateProcess`.
            let err = unsafe { GetLastError() };
            return Err(map_win32_error(err, Some(key), "TerminateProcess"));
        }

        Ok(())
    }

    fn terminate_process_tree(&self, targets: &[ProcessKey]) -> Result<TreeKillReport, PalError> {
        let Some(&root) = targets.last() else {
            return Err(PalError::InvalidArgument(
                "terminate_process_tree requires at least one target".into(),
            ));
        };

        let mut items = Vec::with_capacity(targets.len());

        for &key in targets {
            let name = best_effort_process_short_name(key);
            match self.terminate_process(key) {
                Ok(()) | Err(PalError::ProcessNotFound(_)) => {
                    items.push(TreeKillItem {
                        key,
                        name,
                        terminated: true,
                        error: None,
                    });
                }
                Err(err) => {
                    items.push(TreeKillItem {
                        key,
                        name,
                        terminated: false,
                        error: Some(err.to_string()),
                    });
                }
            }
        }

        Ok(TreeKillReport { root, items })
    }

    fn set_efficiency_mode(&self, key: ProcessKey, enabled: bool) -> Result<(), PalError> {
        // System Idle (PID 0) and System (PID 4) are kernel pseudo-processes that must never
        // be throttled even when Loom runs elevated.
        if key.pid == 0 || key.pid == 4 {
            return Err(PalError::Unsupported(
                "efficiency mode is not supported for Windows kernel system processes",
            ));
        }

        let handle = open_verified_process(key, PROCESS_SET_INFORMATION)?;

        let throttling = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: if enabled {
                PROCESS_POWER_THROTTLING_EXECUTION_SPEED
            } else {
                0
            },
        };

        // SAFETY: `handle` has `PROCESS_SET_INFORMATION` and `throttling` matches `ProcessPowerThrottling`.
        let ok = unsafe {
            SetProcessInformation(
                handle.as_raw(),
                ProcessPowerThrottling,
                (&throttling as *const PROCESS_POWER_THROTTLING_STATE).cast::<c_void>(),
                size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
        };
        if ok == 0 {
            // SAFETY: Reads last Win32 error set by `SetProcessInformation`.
            let err = unsafe { GetLastError() };
            if err == ERROR_NOT_SUPPORTED || err == ERROR_INVALID_FUNCTION {
                return Err(PalError::Unsupported(
                    "ProcessPowerThrottling is not supported for this process",
                ));
            }
            return Err(map_win32_error(
                err,
                Some(key),
                "SetProcessInformation(ProcessPowerThrottling)",
            ));
        }

        let target_class = if enabled {
            IDLE_PRIORITY_CLASS
        } else {
            NORMAL_PRIORITY_CLASS
        };
        // SAFETY: `handle` has `PROCESS_SET_INFORMATION`.
        let prio_ok = unsafe { SetPriorityClass(handle.as_raw(), target_class) };
        if prio_ok == 0 {
            // SAFETY: Reads last Win32 error set by `SetPriorityClass`.
            let err = unsafe { GetLastError() };
            return Err(map_win32_error(err, Some(key), "SetPriorityClass"));
        }

        Ok(())
    }

    fn set_priority(&self, key: ProcessKey, priority: ProcessPriority) -> Result<(), PalError> {
        let handle = open_verified_process(key, PROCESS_SET_INFORMATION)?;
        let class = match priority {
            ProcessPriority::Idle => IDLE_PRIORITY_CLASS,
            ProcessPriority::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
            ProcessPriority::Normal => NORMAL_PRIORITY_CLASS,
            ProcessPriority::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
            ProcessPriority::High => HIGH_PRIORITY_CLASS,
            ProcessPriority::Realtime => REALTIME_PRIORITY_CLASS,
        };

        // SAFETY: `handle` is validated and opened with `PROCESS_SET_INFORMATION`.
        let ok = unsafe { SetPriorityClass(handle.as_raw(), class) };
        if ok == 0 {
            // SAFETY: Reads last Win32 error set by `SetPriorityClass`.
            let err = unsafe { GetLastError() };
            return Err(map_win32_error(err, Some(key), "SetPriorityClass"));
        }

        Ok(())
    }

    fn get_affinity(&self, key: ProcessKey) -> Result<CpuAffinityMask, PalError> {
        let handle = open_verified_process(key, PROCESS_QUERY_LIMITED_INFORMATION)?;
        let logical_cpus = self.logical_cpu_count();

        if logical_cpus <= 64 {
            let mut proc_mask: usize = 0;
            let mut sys_mask: usize = 0;
            // SAFETY: `handle` has `PROCESS_QUERY_LIMITED_INFORMATION` and out pointers are stack locals.
            let ok =
                unsafe { GetProcessAffinityMask(handle.as_raw(), &mut proc_mask, &mut sys_mask) };
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
            let ok =
                unsafe { GetProcessAffinityMask(handle.as_raw(), &mut proc_mask, &mut sys_mask) };
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

    fn set_affinity(&self, key: ProcessKey, mask: &CpuAffinityMask) -> Result<(), PalError> {
        if mask.is_empty() {
            return Err(PalError::InvalidArgument(
                "CPU affinity mask must enable at least one processor".into(),
            ));
        }

        let logical_cpus = self.logical_cpu_count();
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

    fn set_uac_virtualization(&self, _key: ProcessKey, _enabled: bool) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "set_uac_virtualization is scheduled for v0.0.5",
        ))
    }

    fn analyze_wait_chain(&self, _key: ProcessKey) -> Result<Vec<WaitChainNode>, PalError> {
        Err(PalError::Unsupported(
            "analyze_wait_chain is scheduled for v0.0.5",
        ))
    }

    fn write_minidump(
        &self,
        _key: ProcessKey,
        _dest: &Path,
        _cancel: &AtomicBool,
    ) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "write_minidump is scheduled for v0.0.5",
        ))
    }

    fn relaunch_self_elevated(&self, restore_route: &str) -> Result<(), PalError> {
        ensure_com_initialized();

        let current_exe = std::env::current_exe()?;
        let wide_exe = os_str_to_wide_null(current_exe.as_os_str());
        let wide_verb = to_wide_null("runas");

        let arg_str = if restore_route.contains(char::is_whitespace) {
            format!("--restore-route=\"{restore_route}\"")
        } else {
            format!("--restore-route={restore_route}")
        };
        let wide_params = to_wide_null(&arg_str);

        // SAFETY: `SHELLEXECUTEINFOW` is POD; wide string buffers remain alive across the call.
        let mut sei: SHELLEXECUTEINFOW = unsafe { zeroed() };
        sei.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        sei.fMask = SEE_MASK_NOASYNC;
        sei.lpVerb = wide_verb.as_ptr();
        sei.lpFile = wide_exe.as_ptr();
        sei.lpParameters = wide_params.as_ptr();
        sei.nShow = SW_SHOWNORMAL;

        // SAFETY: All pointers in `sei` are valid null-terminated UTF-16 buffers.
        let ok = unsafe { ShellExecuteExW(&mut sei) };
        if ok == 0 {
            // SAFETY: Reads last Win32 error set by `ShellExecuteExW`.
            let err = unsafe { GetLastError() };
            return Err(map_win32_error(err, None, "relaunch_self_elevated"));
        }

        Ok(())
    }

    fn open_file_location(&self, path: &Path) -> Result<(), PalError> {
        ensure_com_initialized();

        let wide_verb = to_wide_null("open");
        let wide_file = to_wide_null("explorer.exe");
        let params = format!("/select,\"{}\"", path.display());
        let wide_params = to_wide_null(&params);

        // SAFETY: `SHELLEXECUTEINFOW` is POD; wide strings remain valid during the call.
        let mut sei: SHELLEXECUTEINFOW = unsafe { zeroed() };
        sei.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        sei.fMask = SEE_MASK_NOASYNC;
        sei.lpVerb = wide_verb.as_ptr();
        sei.lpFile = wide_file.as_ptr();
        sei.lpParameters = wide_params.as_ptr();
        sei.nShow = SW_SHOWNORMAL;

        // SAFETY: All pointers in `sei` are valid null-terminated UTF-16 buffers.
        let ok = unsafe { ShellExecuteExW(&mut sei) };
        if ok == 0 {
            // SAFETY: Reads last Win32 error set by `ShellExecuteExW`.
            let err = unsafe { GetLastError() };
            return Err(map_win32_error(err, None, "open_file_location"));
        }

        Ok(())
    }

    fn show_file_properties(&self, path: &Path) -> Result<(), PalError> {
        ensure_com_initialized();

        let wide_verb = to_wide_null("properties");
        let wide_path = os_str_to_wide_null(path.as_os_str());

        // SAFETY: `SHELLEXECUTEINFOW` is POD.
        let mut sei: SHELLEXECUTEINFOW = unsafe { zeroed() };
        sei.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        sei.fMask = SEE_MASK_INVOKEIDLIST;
        sei.lpVerb = wide_verb.as_ptr();
        sei.lpFile = wide_path.as_ptr();
        sei.nShow = SW_SHOW;

        // SAFETY: All pointers in `sei` are valid null-terminated UTF-16 buffers.
        let ok = unsafe { ShellExecuteExW(&mut sei) };
        if ok == 0 {
            // SAFETY: Reads last Win32 error set by `ShellExecuteExW`.
            let err = unsafe { GetLastError() };
            return Err(map_win32_error(err, None, "show_file_properties"));
        }

        Ok(())
    }

    fn search_online(&self, query: &str) -> Result<(), PalError> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Err(PalError::InvalidArgument(
                "search query cannot be empty".into(),
            ));
        }

        ensure_com_initialized();

        let encoded = percent_encode_query(trimmed);
        let url = format!("https://www.bing.com/search?q={encoded}");
        let wide_verb = to_wide_null("open");
        let wide_url = to_wide_null(&url);

        // SAFETY: `SHELLEXECUTEINFOW` is POD.
        let mut sei: SHELLEXECUTEINFOW = unsafe { zeroed() };
        sei.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        sei.fMask = SEE_MASK_NOASYNC;
        sei.lpVerb = wide_verb.as_ptr();
        sei.lpFile = wide_url.as_ptr();
        sei.nShow = SW_SHOWNORMAL;

        // SAFETY: All pointers in `sei` are valid null-terminated UTF-16 buffers.
        let ok = unsafe { ShellExecuteExW(&mut sei) };
        if ok == 0 {
            // SAFETY: Reads last Win32 error set by `ShellExecuteExW`.
            let err = unsafe { GetLastError() };
            return Err(map_win32_error(err, None, "search_online"));
        }

        Ok(())
    }

    fn browse_executable(&self) -> Result<Option<PathBuf>, PalError> {
        ensure_com_initialized();

        let filter: Vec<u16> = BROWSE_EXECUTABLE_FILTER.encode_utf16().collect();
        let mut file_buf = vec![0u16; 32_768];

        // SAFETY: `OPENFILENAMEW` is POD.
        let mut ofn: OPENFILENAMEW = unsafe { zeroed() };
        ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
        ofn.lpstrFilter = filter.as_ptr();
        ofn.nFilterIndex = 1;
        ofn.lpstrFile = file_buf.as_mut_ptr();
        ofn.nMaxFile = file_buf.len() as u32;
        ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;

        // SAFETY: `ofn` points to valid UTF-16 buffers for the synchronous dialog call.
        let ok = unsafe { GetOpenFileNameW(&mut ofn) };
        if ok == 0 {
            return Ok(None);
        }

        let len = file_buf
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(file_buf.len());
        if len == 0 {
            return Ok(None);
        }

        Ok(Some(PathBuf::from(String::from_utf16_lossy(
            &file_buf[..len],
        ))))
    }

    fn is_elevated(&self) -> bool {
        let mut raw_token: HANDLE = ptr::null_mut();
        // SAFETY: `GetCurrentProcess` returns a pseudo-handle; `OpenProcessToken` opens a token handle.
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) };
        if ok == 0 {
            return false;
        }
        let Some(token) = OwnedHandle::from_raw(raw_token) else {
            return false;
        };

        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut ret_len: u32 = 0;
        // SAFETY: `elevation` matches `TokenElevation` output size.
        let info_ok = unsafe {
            GetTokenInformation(
                token.as_raw(),
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast::<c_void>(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut ret_len,
            )
        };
        info_ok != 0 && elevation.TokenIsElevated != 0
    }

    fn logical_cpu_count(&self) -> u32 {
        query_logical_cpu_count()
    }
}

/// Double-null-terminated Win32 `OPENFILENAMEW` filter for executable file selection.
const BROWSE_EXECUTABLE_FILTER: &str = "Executables (*.exe;*.bat;*.cmd;*.msc;*.com)\0*.exe;*.bat;*.cmd;*.msc;*.com\0All Files (*.*)\0*.*\0\0";

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
fn build_affinity_mask_from_groups(
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
fn affinity_mask_to_group_affinities(
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

/// Splits a "Run new task" command line into `(executable_or_target, arguments)`.
fn split_command_and_args(cmd: &str) -> (&str, &str) {
    let trimmed = cmd.trim();
    if let Some(after_open) = trimmed.strip_prefix('"') {
        if let Some(close_idx) = after_open.find('"') {
            let file = &after_open[..close_idx];
            let args = after_open[close_idx + 1..].trim();
            return (file, args);
        }
        return (after_open, "");
    }

    if Path::new(trimmed).exists() {
        return (trimmed, "");
    }

    match trimmed.find(char::is_whitespace) {
        Some(idx) => (&trimmed[..idx], trimmed[idx..].trim()),
        None => (trimmed, ""),
    }
}

/// Best-effort short image filename lookup used for enriching [`TreeKillItem`].
fn best_effort_process_short_name(key: ProcessKey) -> Arc<str> {
    let Ok(handle) = open_verified_process(key, PROCESS_QUERY_LIMITED_INFORMATION) else {
        return Arc::from("");
    };
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    // SAFETY: `handle` has `PROCESS_QUERY_LIMITED_INFORMATION` and `buf` is writable for `len`.
    let ok = unsafe { QueryFullProcessImageNameW(handle.as_raw(), 0, buf.as_mut_ptr(), &mut len) };
    if ok == 0 || len == 0 {
        return Arc::from("");
    }
    let full = String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]);
    let short = Path::new(&full)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&full);
    Arc::from(short)
}

/// RFC 3986 percent-encoding for search query URL parameters.
fn percent_encode_query(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 2);
    for &b in input.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else if b == b' ' {
            out.push('+');
        } else {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0F) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_command_and_args_handles_quoted_and_unquoted_inputs() {
        assert_eq!(
            split_command_and_args(r#""C:\Program Files\Demo\app.exe" --flag 1"#),
            (r"C:\Program Files\Demo\app.exe", "--flag 1")
        );
        assert_eq!(
            split_command_and_args("notepad.exe C:\\test.txt"),
            ("notepad.exe", "C:\\test.txt")
        );
        assert_eq!(split_command_and_args("  cmd.exe  "), ("cmd.exe", ""));
    }

    #[test]
    fn percent_encode_query_encodes_spaces_and_unicode() {
        assert_eq!(percent_encode_query("svchost.exe"), "svchost.exe");
        assert_eq!(
            percent_encode_query("svchost.exe high cpu"),
            "svchost.exe+high+cpu"
        );
        assert_eq!(percent_encode_query("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn multi_group_affinity_helpers_map_96_and_128_cpu_topologies() {
        // 96 logical CPUs split as two 48-CPU processor groups ([48, 48]).
        let groups_96 = [48u32, 48u32];
        let mut mask = CpuAffinityMask::empty(96);
        mask.set(0, true);
        mask.set(47, true);
        mask.set(48, true); // Group 1, bit 0
        mask.set(95, true); // Group 1, bit 47

        let ga = affinity_mask_to_group_affinities(&mask, &groups_96);
        assert_eq!(ga.len(), 2);
        assert_eq!(ga[0].Group, 0);
        assert_eq!(ga[0].Mask, (1usize << 0) | (1usize << 47));
        assert_eq!(ga[1].Group, 1);
        assert_eq!(ga[1].Mask, (1usize << 0) | (1usize << 47));

        let roundtrip = build_affinity_mask_from_groups(96, &groups_96, &[0, 1], None, &ga);
        assert_eq!(roundtrip, mask);

        // Single-group process pinned to group 1 on a 128-CPU ([64, 64]) system.
        let groups_128 = [64u32, 64u32];
        let single_g1 = build_affinity_mask_from_groups(128, &groups_128, &[1], Some(0b1011), &[]);
        assert_eq!(single_g1.enabled_count(), 3);
        assert!(!single_g1.is_enabled(0));
        assert!(single_g1.is_enabled(64));
        assert!(single_g1.is_enabled(65));
        assert!(!single_g1.is_enabled(66));
        assert!(single_g1.is_enabled(67));

        // Multi-group default process with no custom CPU set masks enables all CPUs in its groups.
        let all_groups = build_affinity_mask_from_groups(128, &groups_128, &[0, 1], None, &[]);
        assert_eq!(all_groups, CpuAffinityMask::all(128));
    }

    #[test]
    fn browse_executable_filter_includes_expected_extensions_and_double_null() {
        assert!(BROWSE_EXECUTABLE_FILTER.ends_with("\0\0"));
        assert!(BROWSE_EXECUTABLE_FILTER.contains("*.exe;*.bat;*.cmd;*.msc;*.com"));
        assert!(BROWSE_EXECUTABLE_FILTER.contains("All Files (*.*)\0*.*\0\0"));
    }
}
