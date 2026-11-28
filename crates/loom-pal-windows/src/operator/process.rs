//! Process lifecycle and policy operations: termination, tree kill, EcoQoS efficiency mode,
//! priority class, UAC virtualization, and suspend/resume with critical-process protection.

use std::ffi::c_void;
use std::mem::{size_of, transmute};
use std::path::Path;
use std::ptr;
use std::sync::Arc;

use loom_pal_api::PalError;
use loom_types::{ProcessKey, ProcessPriority, TreeKillItem, TreeKillReport};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED,
    GetLastError, HANDLE, STATUS_ACCESS_DENIED, STATUS_PROCESS_IS_TERMINATING,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_QUERY,
    TokenVirtualizationAllowed, TokenVirtualizationEnabled,
};
use windows_sys::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS,
    IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, OpenProcessToken,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_STATE, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
    PROCESS_SUSPEND_RESUME, PROCESS_TERMINATE, ProcessPowerThrottling, QueryFullProcessImageNameW,
    REALTIME_PRIORITY_CLASS, SetPriorityClass, SetProcessInformation, TerminateProcess,
};
use windows_sys::core::BOOL;

use crate::handle::{OwnedHandle, OwnedModule, map_win32_error, open_verified_process};

type NtProcessControlFn = unsafe extern "system" fn(HANDLE) -> i32;
type IsProcessCriticalFn = unsafe extern "system" fn(HANDLE, *mut BOOL) -> BOOL;

/// Image names that must never be suspended/resumed to protect desktop and kernel stability.
const PROTECTED_SUSPEND_NAMES: [&str; 6] = [
    "dwm.exe",
    "csrss.exe",
    "smss.exe",
    "wininit.exe",
    "winlogon.exe",
    "lsass.exe",
];

pub(super) fn is_protected_suspend_image(image_name: &str) -> bool {
    PROTECTED_SUSPEND_NAMES
        .iter()
        .any(|blocked| image_name.eq_ignore_ascii_case(blocked))
}

pub(super) fn terminate_process(key: ProcessKey) -> Result<(), PalError> {
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

pub(super) fn terminate_process_tree(targets: &[ProcessKey]) -> Result<TreeKillReport, PalError> {
    let Some(&root) = targets.last() else {
        return Err(PalError::InvalidArgument(
            "terminate_process_tree requires at least one target".into(),
        ));
    };

    let mut items = Vec::with_capacity(targets.len());

    for &key in targets {
        let name = best_effort_process_short_name(key);
        match terminate_process(key) {
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

pub(super) fn set_efficiency_mode(key: ProcessKey, enabled: bool) -> Result<(), PalError> {
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

pub(super) fn set_priority(key: ProcessKey, priority: ProcessPriority) -> Result<(), PalError> {
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

pub(super) fn set_uac_virtualization(key: ProcessKey, enabled: bool) -> Result<(), PalError> {
    if key.pid == 0 || key.pid == 4 {
        return Err(PalError::Unsupported(
            "UAC virtualization is not supported for Windows kernel system processes",
        ));
    }

    let proc_handle = open_verified_process(key, PROCESS_QUERY_LIMITED_INFORMATION)?;

    let mut raw_token: HANDLE = ptr::null_mut();
    // SAFETY: `proc_handle` is verified and open with `PROCESS_QUERY_LIMITED_INFORMATION`.
    let open_ok = unsafe {
        OpenProcessToken(
            proc_handle.as_raw(),
            TOKEN_ADJUST_DEFAULT | TOKEN_QUERY,
            &mut raw_token,
        )
    };
    if open_ok == 0 {
        // SAFETY: Reads thread-local Win32 error set by `OpenProcessToken`.
        let err = unsafe { GetLastError() };
        return Err(map_win32_error(
            err,
            Some(key),
            "OpenProcessToken(set_uac_virtualization)",
        ));
    }
    let Some(token) = OwnedHandle::from_raw(raw_token) else {
        return Err(PalError::ElevationRequired);
    };

    // Query `TokenVirtualizationAllowed` first so 64-bit/elevated processes yield a clear Unsupported error.
    let mut allowed: u32 = 0;
    let mut ret_len: u32 = 0;
    // SAFETY: `allowed` is a 4-byte `u32` matching `TokenVirtualizationAllowed`.
    let allowed_ok = unsafe {
        GetTokenInformation(
            token.as_raw(),
            TokenVirtualizationAllowed,
            (&mut allowed as *mut u32).cast::<c_void>(),
            size_of::<u32>() as u32,
            &mut ret_len,
        )
    };
    if allowed_ok != 0 && allowed == 0 {
        return Err(PalError::Unsupported(
            "UAC virtualization is not allowed for this process",
        ));
    }

    let value: u32 = u32::from(enabled);
    // SAFETY: `token` has `TOKEN_ADJUST_DEFAULT` and `value` is a 4-byte `DWORD`.
    let set_ok = unsafe {
        SetTokenInformation(
            token.as_raw(),
            TokenVirtualizationEnabled,
            (&value as *const u32).cast::<c_void>(),
            size_of::<u32>() as u32,
        )
    };
    if set_ok == 0 {
        // SAFETY: Reads thread-local Win32 error set by `SetTokenInformation`.
        let err = unsafe { GetLastError() };
        return match err {
            ERROR_ACCESS_DENIED => Err(PalError::ElevationRequired),
            ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED => Err(PalError::Unsupported(
                "UAC virtualization is not supported for this process",
            )),
            _ => Err(map_win32_error(
                err,
                Some(key),
                "SetTokenInformation(TokenVirtualizationEnabled)",
            )),
        };
    }

    Ok(())
}

/// Suspends or resumes all threads of `key` via dynamically loaded `ntdll!NtSuspendProcess` /
/// `ntdll!NtResumeProcess` after verifying identity, non-criticality, and protected-UI exclusions.
pub(super) fn suspend_or_resume_process(key: ProcessKey, suspend: bool) -> Result<(), PalError> {
    if key.pid == 0 || key.pid == 4 || key.pid == std::process::id() {
        return Err(PalError::Unsupported(
            "cannot suspend or resume kernel processes or Loom itself",
        ));
    }

    let handle = open_verified_process(key, PROCESS_SUSPEND_RESUME)?;

    // Refuse if `IsProcessCritical` reports true OR cannot be queried safely.
    if !verify_not_critical_process(handle.as_raw()) {
        return Err(PalError::Unsupported(
            "cannot suspend or resume a critical or unverified system process",
        ));
    }

    // Refuse core desktop/session processes such as `dwm.exe`.
    if let Some(short_name) = query_short_image_name(handle.as_raw())
        && is_protected_suspend_image(&short_name)
    {
        return Err(PalError::Unsupported(
            "cannot suspend or resume protected Windows desktop/session processes",
        ));
    }

    let ntdll = OwnedModule::load("ntdll.dll")?;
    let symbol: &[u8] = if suspend {
        b"NtSuspendProcess\0"
    } else {
        b"NtResumeProcess\0"
    };
    let fn_addr = ntdll.proc_addr(symbol).ok_or(PalError::Unsupported(
        "NtSuspendProcess/NtResumeProcess is unavailable in ntdll.dll",
    ))?;
    // SAFETY: Signature matches `NTSTATUS NTAPI NtSuspendProcess/NtResumeProcess(HANDLE)`.
    let nt_call: NtProcessControlFn = unsafe { transmute(fn_addr) };

    // SAFETY: `handle` is verified against `key` and opened with `PROCESS_SUSPEND_RESUME`.
    let status = unsafe { nt_call(handle.as_raw()) };
    if status < 0 {
        if status == STATUS_ACCESS_DENIED {
            return Err(PalError::ElevationRequired);
        }
        if status == STATUS_PROCESS_IS_TERMINATING {
            return Err(PalError::ProcessNotFound(key));
        }
        return Err(PalError::OsError {
            code: status,
            message: format!(
                "{} failed with NTSTATUS 0x{:08X}",
                if suspend {
                    "NtSuspendProcess"
                } else {
                    "NtResumeProcess"
                },
                status as u32
            ),
        });
    }

    Ok(())
}

/// Returns `true` ONLY when `kernel32!IsProcessCritical` succeeds and confirms `critical == 0`.
fn verify_not_critical_process(handle: HANDLE) -> bool {
    let Ok(kernel32) = OwnedModule::load("kernel32.dll") else {
        return false;
    };
    let Some(addr) = kernel32.proc_addr(b"IsProcessCritical\0") else {
        return false;
    };
    // SAFETY: Signature matches `BOOL WINAPI IsProcessCritical(HANDLE, PBOOL)`.
    let is_critical_fn: IsProcessCriticalFn = unsafe { transmute(addr) };
    let mut critical: BOOL = 1;
    // SAFETY: `handle` has `PROCESS_QUERY_LIMITED_INFORMATION` and `critical` is a valid out pointer.
    let ok = unsafe { is_critical_fn(handle, &mut critical) };
    ok != 0 && critical == 0
}

fn query_short_image_name(handle: HANDLE) -> Option<String> {
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    // SAFETY: `handle` has `PROCESS_QUERY_LIMITED_INFORMATION` and `buf` is writable for `len`.
    let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len) };
    if ok == 0 || len == 0 {
        return None;
    }
    let full = String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]);
    Path::new(&full)
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
}

/// Best-effort short image filename lookup used for enriching [`TreeKillItem`].
fn best_effort_process_short_name(key: ProcessKey) -> Arc<str> {
    let Ok(handle) = open_verified_process(key, PROCESS_QUERY_LIMITED_INFORMATION) else {
        return Arc::from("");
    };
    query_short_image_name(handle.as_raw())
        .map(Arc::<str>::from)
        .unwrap_or_else(|| Arc::from(""))
}
