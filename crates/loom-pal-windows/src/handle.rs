//! RAII Win32 handle wrapper and verified process-opening helpers.
//!
//! Ensures Loom never holds target process handles across sampling ticks and
//! validates [`ProcessKey::create_time_100ns`] before every read or control
//! operation to eliminate Windows PID-reuse races.

use std::ffi::{OsStr, c_void};
use std::mem::{size_of, transmute};
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::sync::OnceLock;

use loom_pal_api::PalError;
use loom_types::ProcessKey;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_CANCELLED, ERROR_INVALID_PARAMETER, FALSE, FILETIME,
    GetLastError, HANDLE, INVALID_HANDLE_VALUE, STATUS_SUCCESS,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// NT `PROCESSINFOCLASS::ProcessSequenceNumber` (`92`, Windows 10 20H1+).
const PROCESS_SEQUENCE_NUMBER_CLASS: i32 = 92;

type FarProc = unsafe extern "system" fn() -> isize;
type FnNtQueryInformationProcess = unsafe extern "system" fn(
    process_handle: HANDLE,
    process_information_class: i32,
    process_information: *mut c_void,
    process_information_length: u32,
    return_length: *mut u32,
) -> i32;

/// RAII owner of a Win32 kernel `HANDLE` that invokes `CloseHandle` on drop.
#[derive(Debug)]
pub(crate) struct OwnedHandle(HANDLE);

// Win32 kernel handles are thread-safe to transfer across threads.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl OwnedHandle {
    /// Wraps a raw Win32 `HANDLE`, returning `None` for null or `INVALID_HANDLE_VALUE`.
    #[inline]
    pub(crate) fn from_raw(raw: HANDLE) -> Option<Self> {
        if raw.is_null() || raw == INVALID_HANDLE_VALUE {
            None
        } else {
            Some(Self(raw))
        }
    }

    /// Returns the underlying raw `HANDLE` for immediate FFI calls.
    #[inline]
    pub(crate) fn as_raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: `self.0` is an exclusively owned valid kernel handle.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// Converts a Win32 `FILETIME` (100-nanosecond intervals since 1601-01-01 UTC) into `u64`.
#[inline]
pub(crate) fn filetime_to_u64(ft: FILETIME) -> u64 {
    (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
}

/// Maps a Win32 `GetLastError()` code into a structured [`PalError`].
pub(crate) fn map_win32_error(err: u32, key: Option<ProcessKey>, context: &str) -> PalError {
    match err {
        ERROR_ACCESS_DENIED => PalError::ElevationRequired,
        ERROR_INVALID_PARAMETER => match key {
            Some(k) => PalError::ProcessNotFound(k),
            None => PalError::InvalidArgument(format!("{context}: invalid parameter")),
        },
        ERROR_CANCELLED => PalError::OsError {
            code: err as i32,
            message: format!("{context}: cancelled by user"),
        },
        _ => PalError::OsError {
            code: err as i32,
            message: format!("{context}: Win32 error {err}"),
        },
    }
}

/// Opens a Win32 process handle with minimal requested rights and verifies its
/// kernel creation timestamp matches [`ProcessKey::create_time_100ns`].
///
/// The returned [`OwnedHandle`] closes immediately when dropped, ensuring target
/// process handles are never retained across ticks.
pub(crate) fn open_verified_process(
    key: ProcessKey,
    desired_access: u32,
) -> Result<OwnedHandle, PalError> {
    if key.pid == 0 {
        return Err(PalError::ElevationRequired);
    }

    let access = desired_access | PROCESS_QUERY_LIMITED_INFORMATION;
    // SAFETY: `OpenProcess` takes plain integer arguments and returns null on error.
    let raw = unsafe { OpenProcess(access, FALSE, key.pid) };
    let Some(handle) = OwnedHandle::from_raw(raw) else {
        // SAFETY: `GetLastError` reads thread-local last error set by `OpenProcess`.
        let err = unsafe { GetLastError() };
        return Err(map_win32_error(err, Some(key), "OpenProcess"));
    };

    let mut create = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();

    // SAFETY: `handle` is valid with `PROCESS_QUERY_LIMITED_INFORMATION` and all output
    // pointers reference initialized stack structs.
    let ok = unsafe {
        GetProcessTimes(
            handle.as_raw(),
            &mut create,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        // SAFETY: `GetLastError` reads thread-local last error set by `GetProcessTimes`.
        let err = unsafe { GetLastError() };
        return Err(map_win32_error(err, Some(key), "GetProcessTimes"));
    }

    let actual_create_time = filetime_to_u64(create);
    if actual_create_time != key.create_time_100ns {
        return Err(PalError::ProcessIdentityMismatch(key));
    }

    Ok(handle)
}

/// Encodes a UTF-8 `&str` into a null-terminated UTF-16 `Vec<u16>` for Win32 W-APIs.
pub(crate) fn to_wide_null(s: &str) -> Vec<u16> {
    let mut buf: Vec<u16> = Vec::with_capacity(s.len().saturating_add(1));
    buf.extend(s.encode_utf16());
    buf.push(0);
    buf
}

/// Encodes an [`OsStr`] into a null-terminated UTF-16 `Vec<u16>` for Win32 W-APIs.
pub(crate) fn os_str_to_wide_null(s: &OsStr) -> Vec<u16> {
    let mut buf: Vec<u16> = s.encode_wide().collect();
    buf.push(0);
    buf
}

/// Resolves `ntdll.dll!NtQueryInformationProcess` once for additive kernel queries.
fn nt_query_information_process() -> Option<FnNtQueryInformationProcess> {
    static FN_PTR: OnceLock<Option<FnNtQueryInformationProcess>> = OnceLock::new();
    *FN_PTR.get_or_init(|| {
        let ntdll_name = to_wide_null("ntdll.dll");
        // SAFETY: `ntdll.dll` is permanently loaded in every Windows user-mode process.
        let module = unsafe { GetModuleHandleW(ntdll_name.as_ptr()) };
        if module.is_null() {
            return None;
        }
        // SAFETY: Resolving documented `NtQueryInformationProcess` export from `ntdll.dll`.
        let proc_ptr =
            unsafe { GetProcAddress(module, c"NtQueryInformationProcess".as_ptr().cast()) }?;
        // SAFETY: `FnNtQueryInformationProcess` matches the exact NT syscall prototype.
        Some(unsafe { transmute::<FarProc, FnNtQueryInformationProcess>(proc_ptr) })
    })
}

/// Queries the kernel-assigned monotonic process sequence number via
/// `NtQueryInformationProcess(ProcessSequenceNumber = 92)` as an additive PID-reuse guard.
///
/// Returns `None` gracefully on invalid handles or older Windows builds (prior to Windows 10
/// 20H1) that do not implement information class `92`.
pub(crate) fn query_process_sequence_number(handle: HANDLE) -> Option<u64> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let query_fn = nt_query_information_process()?;
    let mut seq: u64 = 0;
    let mut return_len: u32 = 0;

    // SAFETY: `seq` is an initialized stack `u64` matching `ProcessSequenceNumber` (class 92).
    let status = unsafe {
        query_fn(
            handle,
            PROCESS_SEQUENCE_NUMBER_CLASS,
            (&mut seq as *mut u64).cast::<c_void>(),
            size_of::<u64>() as u32,
            &mut return_len,
        )
    };
    if status == STATUS_SUCCESS && (return_len == 0 || return_len as usize >= size_of::<u64>()) {
        Some(seq)
    } else {
        None
    }
}

/// Helper returning a null raw pointer with explicit type inference for optional Win32 strings.
#[inline]
pub(crate) fn null_wide_ptr() -> *const u16 {
    ptr::null()
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    use super::*;

    #[test]
    fn query_process_sequence_number_is_stable_for_current_process_and_rejects_invalid_handles() {
        assert_eq!(query_process_sequence_number(ptr::null_mut()), None);
        assert_eq!(query_process_sequence_number(INVALID_HANDLE_VALUE), None);

        // SAFETY: `GetCurrentProcess` returns the pseudo-handle for this process.
        let current = unsafe { GetCurrentProcess() };
        let first = query_process_sequence_number(current);
        let second = query_process_sequence_number(current);
        assert_eq!(
            first, second,
            "ProcessSequenceNumber must be stable across repeated queries of the same process"
        );
        if let Some(seq) = first {
            assert!(
                seq > 0,
                "ProcessSequenceNumber for a live process should be non-zero"
            );
        }
    }
}
