//! Per-process kernel & token metadata queries for [`super::WindowsSlowProber`].
//!
//! Uses stack fast-path buffers to minimize heap allocations per probed process and
//! strictly reads command lines via `NtQueryInformationProcess(ProcessCommandLineInformation)`
//! (never cross-process `ReadProcessMemory` on target PEB).

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr;
use std::sync::Arc;

use loom_types::{ProcessArch, UacVirtualization};
use parking_lot::RwLock;
use windows_sys::Wdk::System::Threading::{
    NtQueryInformationProcess, ProcessCommandLineInformation,
};
use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, GetLastError, HANDLE, STATUS_BUFFER_TOO_SMALL,
    STATUS_INFO_LENGTH_MISMATCH, STATUS_SUCCESS, UNICODE_STRING,
};
use windows_sys::Win32::Security::{
    GetLengthSid, GetTokenInformation, LookupAccountSidW, SID_NAME_USE, TOKEN_ELEVATION,
    TOKEN_QUERY, TOKEN_USER, TokenElevation, TokenUser, TokenVirtualizationAllowed,
    TokenVirtualizationEnabled,
};
use windows_sys::Win32::Storage::Packaging::Appx::GetPackageFamilyName;
use windows_sys::Win32::System::SystemInformation::{
    IMAGE_FILE_MACHINE, IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM64,
    IMAGE_FILE_MACHINE_I386, IMAGE_FILE_MACHINE_UNKNOWN,
};
use windows_sys::Win32::System::Threading::{
    GetProcessInformation, IsWow64Process2, OpenProcessToken,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, QueryFullProcessImageNameW,
};

use crate::handle::{OwnedHandle, null_wide_ptr};

/// Reads the full Win32 executable path via `QueryFullProcessImageNameW`.
///
/// Uses a 512-UTF-16-unit stack fast path before falling back to heap allocation for long paths.
pub(super) fn query_process_image_path(handle: HANDLE) -> Option<String> {
    let mut stack_buf = [0u16; 512];
    let mut len = stack_buf.len() as u32;
    // SAFETY: `handle` has `PROCESS_QUERY_LIMITED_INFORMATION` and `stack_buf` is writable for `len`.
    if unsafe { QueryFullProcessImageNameW(handle, 0, stack_buf.as_mut_ptr(), &mut len) } != 0 {
        return Some(String::from_utf16_lossy(
            &stack_buf[..(len as usize).min(stack_buf.len())],
        ));
    }
    if unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
        return None;
    }

    let mut cap = 1024usize;
    for _ in 0..4 {
        let mut buf = vec![0u16; cap];
        let mut heap_len = cap as u32;
        // SAFETY: `handle` has `PROCESS_QUERY_LIMITED_INFORMATION` and `buf` is writable for `heap_len`.
        let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut heap_len) };
        if ok != 0 {
            return Some(String::from_utf16_lossy(
                &buf[..(heap_len as usize).min(buf.len())],
            ));
        }
        // SAFETY: `GetLastError` reads thread-local error from `QueryFullProcessImageNameW`.
        if unsafe { GetLastError() } == ERROR_INSUFFICIENT_BUFFER && cap < 32_768 {
            cap = (cap * 2).min(32_768);
            continue;
        }
        return None;
    }
    None
}

/// Decodes a `UNICODE_STRING` returned inside `buf` by `NtQueryInformationProcess`.
fn decode_command_line_buf(buf: &[u64]) -> Option<String> {
    let buf_bytes = std::mem::size_of_val(buf);
    if buf_bytes < size_of::<UNICODE_STRING>() {
        return None;
    }
    // SAFETY: Buffer is 8-byte aligned and holds at least `UNICODE_STRING`.
    let us = unsafe { ptr::read(buf.as_ptr().cast::<UNICODE_STRING>()) };
    if us.Length == 0 || us.Buffer.is_null() {
        return Some(String::new());
    }
    let base = buf.as_ptr() as usize;
    let end = base.saturating_add(buf_bytes);
    let ptr_addr = us.Buffer as usize;
    let byte_len = usize::from(us.Length);
    if ptr_addr < base || ptr_addr.saturating_add(byte_len) > end {
        return None;
    }
    let code_units = byte_len / size_of::<u16>();
    // SAFETY: Validated `us.Buffer` slice lies strictly within `buf`.
    let wide = unsafe { std::slice::from_raw_parts(us.Buffer, code_units) };
    Some(String::from_utf16_lossy(wide))
}

/// Reads the target process command line purely from kernel memory via
/// `ntdll!NtQueryInformationProcess(ProcessCommandLineInformation)`.
///
/// Strictly avoids cross-process `ReadProcessMemory` on the target PEB, and uses an 8-byte
/// aligned 1 KiB stack fast path before falling back to heap allocation for long command lines.
pub(super) fn query_process_command_line(handle: HANDLE) -> Option<String> {
    let mut stack_buf = [0u64; 128]; // 1024 bytes, 8-byte aligned
    let mut return_len: u32 = 0;

    // SAFETY: `stack_buf` is 8-byte aligned and writable for 1024 bytes.
    let status = unsafe {
        NtQueryInformationProcess(
            handle,
            ProcessCommandLineInformation,
            stack_buf.as_mut_ptr().cast::<c_void>(),
            std::mem::size_of_val(&stack_buf) as u32,
            &mut return_len,
        )
    };
    if status == STATUS_SUCCESS {
        return decode_command_line_buf(&stack_buf);
    }

    if (status != STATUS_INFO_LENGTH_MISMATCH && status != STATUS_BUFFER_TOO_SMALL)
        || return_len == 0
        || (return_len as usize) > 1024 * 1024
    {
        return None;
    }

    let mut needed_u64s = (return_len as usize)
        .saturating_add(64)
        .div_ceil(size_of::<u64>());
    let mut buf = vec![0u64; needed_u64s];

    for _ in 0..2 {
        let buf_bytes = buf.len() * size_of::<u64>();
        let mut ret_len: u32 = 0;
        // SAFETY: `buf` is 8-byte aligned and writable for `buf_bytes`.
        let status = unsafe {
            NtQueryInformationProcess(
                handle,
                ProcessCommandLineInformation,
                buf.as_mut_ptr().cast::<c_void>(),
                buf_bytes as u32,
                &mut ret_len,
            )
        };
        if status == STATUS_SUCCESS {
            return decode_command_line_buf(&buf);
        }
        if (status == STATUS_INFO_LENGTH_MISMATCH || status == STATUS_BUFFER_TOO_SMALL)
            && ret_len > 0
            && (ret_len as usize) <= 1024 * 1024
        {
            needed_u64s = (ret_len as usize)
                .saturating_add(64)
                .div_ceil(size_of::<u64>());
            buf.resize(needed_u64s, 0);
            continue;
        }
        return None;
    }

    None
}

/// Determines target process CPU architecture via `IsWow64Process2`.
pub(super) fn query_process_arch(handle: HANDLE) -> ProcessArch {
    let mut process_machine: IMAGE_FILE_MACHINE = IMAGE_FILE_MACHINE_UNKNOWN;
    let mut native_machine: IMAGE_FILE_MACHINE = IMAGE_FILE_MACHINE_UNKNOWN;

    // SAFETY: `handle` has `PROCESS_QUERY_LIMITED_INFORMATION` and out pointers are stack locals.
    let ok = unsafe { IsWow64Process2(handle, &mut process_machine, &mut native_machine) };
    if ok == 0 {
        return ProcessArch::Unknown;
    }

    let effective = if process_machine == IMAGE_FILE_MACHINE_UNKNOWN {
        native_machine
    } else {
        process_machine
    };

    match effective {
        IMAGE_FILE_MACHINE_I386 => ProcessArch::X86,
        IMAGE_FILE_MACHINE_AMD64 => ProcessArch::X64,
        IMAGE_FILE_MACHINE_ARM64 => ProcessArch::Arm64,
        _ => ProcessArch::Unknown,
    }
}

/// Queries whether EcoQoS / Efficiency Mode is active on `handle` via
/// `GetProcessInformation(ProcessPowerThrottling)`.
pub(super) fn query_efficiency_mode(handle: HANDLE) -> bool {
    let mut state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: 0,
        StateMask: 0,
    };
    // SAFETY: `handle` has `PROCESS_QUERY_LIMITED_INFORMATION` and `state` matches `ProcessPowerThrottling`.
    let ok = unsafe {
        GetProcessInformation(
            handle,
            ProcessPowerThrottling,
            (&mut state as *mut PROCESS_POWER_THROTTLING_STATE).cast::<c_void>(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    };
    ok != 0 && (state.ControlMask & state.StateMask & PROCESS_POWER_THROTTLING_EXECUTION_SPEED) != 0
}

/// Queries the MSIX/UWP Package Family Name for packaged Windows apps via `GetPackageFamilyName`.
///
/// Returns `None` for non-packaged Win32 desktop processes (`APPMODEL_ERROR_NO_PACKAGE`).
pub(super) fn query_process_package_family_name(handle: HANDLE) -> Option<String> {
    let mut stack_buf = [0u16; 128];
    let mut len = stack_buf.len() as u32;

    // SAFETY: `handle` has `PROCESS_QUERY_LIMITED_INFORMATION`; `stack_buf` is writable for `len` u16s.
    let rc = unsafe { GetPackageFamilyName(handle, &mut len, stack_buf.as_mut_ptr()) };
    if rc == ERROR_SUCCESS {
        let char_len = (len as usize).saturating_sub(1).min(stack_buf.len());
        let name = String::from_utf16_lossy(&stack_buf[..char_len]);
        let trimmed = name.trim();
        return (!trimmed.is_empty()).then(|| trimmed.to_owned());
    }

    if rc == ERROR_INSUFFICIENT_BUFFER && len > 1 && len <= 1024 {
        let mut heap_buf = vec![0u16; len as usize];
        // SAFETY: `heap_buf` is sized to the exact length returned by `GetPackageFamilyName`.
        let rc2 = unsafe { GetPackageFamilyName(handle, &mut len, heap_buf.as_mut_ptr()) };
        if rc2 == ERROR_SUCCESS {
            let char_len = (len as usize).saturating_sub(1).min(heap_buf.len());
            let name = String::from_utf16_lossy(&heap_buf[..char_len]);
            let trimmed = name.trim();
            return (!trimmed.is_empty()).then(|| trimmed.to_owned());
        }
    }

    None
}

/// Queries process token owner account name, UAC virtualization state, and elevation status.
pub(super) fn query_token_metadata(
    proc_handle: HANDLE,
    sid_cache: &RwLock<HashMap<Vec<u8>, Arc<str>>>,
) -> (Option<Arc<str>>, UacVirtualization, bool) {
    let mut raw_token: HANDLE = ptr::null_mut();
    // SAFETY: `OpenProcessToken` writes a new token handle into `raw_token` when successful.
    let ok = unsafe { OpenProcessToken(proc_handle, TOKEN_QUERY, &mut raw_token) };
    if ok == 0 {
        return (None, UacVirtualization::Unknown, false);
    }
    let Some(token) = OwnedHandle::from_raw(raw_token) else {
        return (None, UacVirtualization::Unknown, false);
    };

    let uac = query_uac_virtualization(token.as_raw());
    let user = query_token_user(token.as_raw(), sid_cache);
    let is_elevated = query_token_elevation(token.as_raw());
    (user, uac, is_elevated)
}

/// Queries `TokenElevation` from an open `TOKEN_QUERY` handle.
fn query_token_elevation(token: HANDLE) -> bool {
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut ret_len: u32 = 0;
    // SAFETY: `elevation` matches `TokenElevation` output size.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast::<c_void>(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        )
    };
    ok != 0 && elevation.TokenIsElevated != 0
}

/// Queries `TokenVirtualizationAllowed` and `TokenVirtualizationEnabled` from a process token.
fn query_uac_virtualization(token: HANDLE) -> UacVirtualization {
    let mut allowed: u32 = 0;
    let mut ret_len: u32 = 0;
    // SAFETY: Output buffer holds one `u32` as required by `TokenVirtualizationAllowed`.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenVirtualizationAllowed,
            (&mut allowed as *mut u32).cast::<c_void>(),
            size_of::<u32>() as u32,
            &mut ret_len,
        )
    };
    if ok == 0 {
        return UacVirtualization::Unknown;
    }
    if allowed == 0 {
        return UacVirtualization::NotAllowed;
    }

    let mut enabled: u32 = 0;
    // SAFETY: Output buffer holds one `u32` as required by `TokenVirtualizationEnabled`.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenVirtualizationEnabled,
            (&mut enabled as *mut u32).cast::<c_void>(),
            size_of::<u32>() as u32,
            &mut ret_len,
        )
    };
    if ok == 0 {
        UacVirtualization::Unknown
    } else if enabled != 0 {
        UacVirtualization::Enabled
    } else {
        UacVirtualization::Disabled
    }
}

/// Queries `TokenUser` from a process token using a 128-byte stack fast path and resolves its SID.
fn query_token_user(
    token: HANDLE,
    sid_cache: &RwLock<HashMap<Vec<u8>, Arc<str>>>,
) -> Option<Arc<str>> {
    let mut stack_buf = [0u64; 16]; // 128 bytes, 8-byte aligned (fits standard TOKEN_USER + SID)
    let mut needed: u32 = 0;
    // SAFETY: `stack_buf` is 8-byte aligned and writable for 128 bytes.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            stack_buf.as_mut_ptr().cast::<c_void>(),
            std::mem::size_of_val(&stack_buf) as u32,
            &mut needed,
        )
    };
    if ok != 0 {
        // SAFETY: `TOKEN_USER` is written at offset 0 of `stack_buf`.
        let token_user = unsafe { ptr::read(stack_buf.as_ptr().cast::<TOKEN_USER>()) };
        return resolve_sid(token_user.User.Sid, sid_cache);
    }

    if needed == 0 || (needed as usize) < size_of::<TOKEN_USER>() || needed > 4096 {
        return None;
    }

    let mut buf: Vec<u64> = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
    let buf_bytes = (buf.len() * size_of::<u64>()) as u32;

    // SAFETY: `buf` is 8-byte aligned and sized to at least `needed` bytes.
    let ok2 = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast::<c_void>(),
            buf_bytes,
            &mut needed,
        )
    };
    if ok2 == 0 {
        return None;
    }

    // SAFETY: `TOKEN_USER` is written at offset 0 of `buf`.
    let token_user = unsafe { ptr::read(buf.as_ptr().cast::<TOKEN_USER>()) };
    resolve_sid(token_user.User.Sid, sid_cache)
}

/// Resolves a binary Windows SID to a cached `"DOMAIN\\User"` (or `"User"`) [`Arc<str>`].
fn resolve_sid(
    sid_ptr: *mut c_void,
    sid_cache: &RwLock<HashMap<Vec<u8>, Arc<str>>>,
) -> Option<Arc<str>> {
    if sid_ptr.is_null() {
        return None;
    }

    // SAFETY: `sid_ptr` points to a valid Windows SID returned by `GetTokenInformation`.
    let sid_len = unsafe { GetLengthSid(sid_ptr) } as usize;
    if sid_len == 0 || sid_len > 256 {
        return None;
    }

    // SAFETY: `sid_ptr` is readable for `sid_len` bytes.
    let sid_bytes = unsafe { std::slice::from_raw_parts(sid_ptr.cast::<u8>(), sid_len) };

    if let Some(cached) = sid_cache.read().get(sid_bytes) {
        return Some(Arc::clone(cached));
    }

    let mut name_len: u32 = 0;
    let mut domain_len: u32 = 0;
    let mut sid_use: SID_NAME_USE = 0;

    // First call determines required buffer lengths.
    // SAFETY: Null output buffers are documented for size discovery in `LookupAccountSidW`.
    unsafe {
        LookupAccountSidW(
            null_wide_ptr(),
            sid_ptr,
            ptr::null_mut(),
            &mut name_len,
            ptr::null_mut(),
            &mut domain_len,
            &mut sid_use,
        );
    }

    if name_len == 0 {
        return None;
    }

    let mut name_buf = vec![0u16; name_len as usize];
    let mut domain_buf = vec![0u16; domain_len.max(1) as usize];

    // SAFETY: Both buffers are sized according to the preceding `LookupAccountSidW` query.
    let ok = unsafe {
        LookupAccountSidW(
            null_wide_ptr(),
            sid_ptr,
            name_buf.as_mut_ptr(),
            &mut name_len,
            domain_buf.as_mut_ptr(),
            &mut domain_len,
            &mut sid_use,
        )
    };
    if ok == 0 {
        return None;
    }

    let name = String::from_utf16_lossy(&name_buf[..(name_len as usize).min(name_buf.len())]);
    let domain =
        String::from_utf16_lossy(&domain_buf[..(domain_len as usize).min(domain_buf.len())]);

    let formatted: Arc<str> = if domain.is_empty() {
        Arc::from(name)
    } else {
        Arc::from(format!("{domain}\\{name}"))
    };

    sid_cache
        .write()
        .insert(sid_bytes.to_vec(), Arc::clone(&formatted));
    Some(formatted)
}
