//! Windows Service Control Manager (SCM) enumeration and lifecycle control.
//!
//! Strictly avoids WMI/PDH and maps Win32 `ERROR_ACCESS_DENIED` to
//! [`PalError::ElevationRequired`] so the UI can trigger on-demand UAC elevation.

use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use std::ptr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use loom_pal_api::{PalError, ServiceManager};
use loom_types::{ServiceRecord, ServiceState};
use parking_lot::RwLock;
use windows_sys::Win32::Foundation::{
    ERROR_MORE_DATA, ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_NOT_ACTIVE, ERROR_SUCCESS,
    GetLastError, S_OK,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_EXPAND_SZ, REG_SZ, RegCloseKey, RegOpenKeyExW,
    RegQueryValueExW,
};
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, ControlService, ENUM_SERVICE_STATUS_PROCESSW, EnumServicesStatusExW,
    OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, SC_ENUM_PROCESS_INFO, SC_HANDLE,
    SC_MANAGER_CONNECT, SC_MANAGER_ENUMERATE_SERVICE, SC_STATUS_PROCESS_INFO,
    SERVICE_CONTINUE_PENDING, SERVICE_CONTROL_STOP, SERVICE_PAUSE_PENDING, SERVICE_PAUSED,
    SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START, SERVICE_START_PENDING, SERVICE_STATE_ALL,
    SERVICE_STATUS, SERVICE_STATUS_PROCESS, SERVICE_STOP, SERVICE_STOP_PENDING, SERVICE_STOPPED,
    SERVICE_WIN32, StartServiceW,
};
use windows_sys::Win32::UI::Shell::{
    SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, SHLoadIndirectString, ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::handle::{map_win32_error, null_wide_ptr, to_wide_null};
use crate::slow_prober::{ensure_com_initialized, is_unc_path};

/// Maximum time spent waiting for a service to reach `SERVICE_STOPPED` during restart.
const RESTART_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Poll interval while waiting for a stopping service during restart.
const RESTART_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Soft wall-clock budget for `SHLoadIndirectString` resolution during a single
/// `list_services` call so both cold and warm enumerations stay well below `50 ms`.
const INDIRECT_STRING_CALL_BUDGET: Duration = Duration::from_millis(10);

/// Maximum number of indirect resource strings resolved per `list_services` call.
const MAX_INDIRECT_RESOLVES_PER_CALL: usize = 16;

/// RAII owner of a Win32 `SC_HANDLE` closing via `CloseServiceHandle` on drop.
struct ScHandle(SC_HANDLE);

impl ScHandle {
    #[inline]
    fn from_raw(raw: SC_HANDLE) -> Option<Self> {
        if raw.is_null() { None } else { Some(Self(raw)) }
    }

    #[inline]
    fn as_raw(&self) -> SC_HANDLE {
        self.0
    }
}

impl Drop for ScHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is an exclusively owned valid SCM or service handle.
            unsafe {
                CloseServiceHandle(self.0);
            }
        }
    }
}

/// RAII owner of a Win32 registry `HKEY` closing via `RegCloseKey` on drop.
struct RegKeyGuard(HKEY);

impl Drop for RegKeyGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is an exclusively owned valid registry key handle.
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }
}

/// Cached registry-derived service metadata that rarely changes at runtime.
#[derive(Debug, Clone)]
struct CachedServiceMeta {
    group: Option<Arc<str>>,
    description: Option<Arc<str>>,
    /// Raw `@...` indirect string awaiting progressive resolution on a subsequent poll.
    pending_indirect: Option<String>,
}

/// Windows SCM implementation of [`ServiceManager`].
#[derive(Debug, Default)]
pub struct WindowsServiceManager {
    meta_cache: RwLock<HashMap<String, CachedServiceMeta>>,
}

impl WindowsServiceManager {
    /// Creates a new Windows SCM service manager.
    pub fn new() -> Self {
        Self::default()
    }

    fn lookup_or_probe_meta(
        &self,
        services_root: Option<HKEY>,
        service_name: &str,
        indirect_deadline: Instant,
        indirect_remaining: &mut usize,
    ) -> (Option<Arc<str>>, Option<Arc<str>>) {
        let pending_to_resolve = {
            let cache = self.meta_cache.read();
            if let Some(cached) = cache.get(service_name) {
                match cached.pending_indirect.as_ref() {
                    None => return (cached.group.clone(), cached.description.clone()),
                    Some(raw) => {
                        if *indirect_remaining == 0 || Instant::now() >= indirect_deadline {
                            return (cached.group.clone(), cached.description.clone());
                        }
                        Some((cached.group.clone(), raw.clone()))
                    }
                }
            } else {
                None
            }
        };

        if let Some((group, raw_indirect)) = pending_to_resolve {
            *indirect_remaining = indirect_remaining.saturating_sub(1);
            let resolved = resolve_indirect_string(&raw_indirect).map(Arc::<str>::from);
            self.meta_cache.write().insert(
                service_name.to_owned(),
                CachedServiceMeta {
                    group: group.clone(),
                    description: resolved.clone(),
                    pending_indirect: None,
                },
            );
            return (group, resolved);
        }

        let Some(root) = services_root else {
            return (None, None);
        };

        let meta =
            read_service_registry_meta(root, service_name, indirect_deadline, indirect_remaining);
        let result = (meta.group.clone(), meta.description.clone());
        self.meta_cache
            .write()
            .insert(service_name.to_owned(), meta);
        result
    }
}

impl ServiceManager for WindowsServiceManager {
    fn list_services(&self) -> Result<Vec<ServiceRecord>, PalError> {
        let scm = open_scm(SC_MANAGER_ENUMERATE_SERVICE)?;
        let raw_buf = enumerate_win32_services(&scm)?;

        if raw_buf.records_count == 0 {
            return Ok(Vec::new());
        }

        // Open `HKLM\SYSTEM\CurrentControlSet\Services` once for fast relative subkey reads.
        let services_root = open_services_registry_root();
        let indirect_deadline = Instant::now() + INDIRECT_STRING_CALL_BUDGET;
        let mut indirect_remaining = MAX_INDIRECT_RESOLVES_PER_CALL;

        let mut records = Vec::with_capacity(raw_buf.records_count);
        let ptr = raw_buf
            .bytes
            .as_ptr()
            .cast::<ENUM_SERVICE_STATUS_PROCESSW>();

        for idx in 0..raw_buf.records_count {
            // SAFETY: `EnumServicesStatusExW` populated `records_count` contiguous structs at offset 0.
            let entry = unsafe { ptr::read_unaligned(ptr.add(idx)) };
            // SAFETY: `lpServiceName` points to a null-terminated UTF-16 string inside `raw_buf.bytes`.
            let name_str = unsafe { read_wide_ptr(entry.lpServiceName) };
            if name_str.is_empty() {
                continue;
            }
            // SAFETY: `lpDisplayName` points to a null-terminated UTF-16 string inside `raw_buf.bytes`.
            let display_str = unsafe { read_wide_ptr(entry.lpDisplayName) };

            let (group, description) = self.lookup_or_probe_meta(
                services_root.as_ref().map(|k| k.0),
                &name_str,
                indirect_deadline,
                &mut indirect_remaining,
            );

            let pid = if entry.ServiceStatusProcess.dwProcessId != 0 {
                Some(entry.ServiceStatusProcess.dwProcessId)
            } else {
                None
            };
            let state = map_service_state(entry.ServiceStatusProcess.dwCurrentState);
            let name: Arc<str> = Arc::from(name_str);
            let display_name: Arc<str> = if display_str.is_empty() {
                Arc::clone(&name)
            } else {
                Arc::from(display_str)
            };

            records.push(ServiceRecord {
                name,
                display_name,
                description,
                pid,
                state,
                group,
            });
        }

        records.sort_by(|a, b| {
            a.name
                .as_bytes()
                .iter()
                .map(u8::to_ascii_lowercase)
                .cmp(b.name.as_bytes().iter().map(u8::to_ascii_lowercase))
        });

        Ok(records)
    }

    fn start_service(&self, name: &str) -> Result<(), PalError> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(PalError::InvalidArgument(
                "service name cannot be empty".into(),
            ));
        }

        let scm = open_scm(SC_MANAGER_CONNECT)?;
        let svc = open_service(&scm, trimmed, SERVICE_START | SERVICE_QUERY_STATUS)?;

        // SAFETY: `svc` is opened with `SERVICE_START`; zero arguments passed.
        let ok = unsafe { StartServiceW(svc.as_raw(), 0, ptr::null()) };
        if ok == 0 {
            // SAFETY: Reads thread-local error set by `StartServiceW`.
            let err = unsafe { GetLastError() };
            if err == ERROR_SERVICE_ALREADY_RUNNING {
                return Ok(());
            }
            return Err(map_win32_error(err, None, "StartServiceW"));
        }

        Ok(())
    }

    fn stop_service(&self, name: &str) -> Result<(), PalError> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(PalError::InvalidArgument(
                "service name cannot be empty".into(),
            ));
        }

        let scm = open_scm(SC_MANAGER_CONNECT)?;
        let svc = open_service(&scm, trimmed, SERVICE_STOP | SERVICE_QUERY_STATUS)?;

        let mut status: SERVICE_STATUS = unsafe { zeroed() };
        // SAFETY: `svc` is opened with `SERVICE_STOP` and `status` is valid writable POD.
        let ok = unsafe { ControlService(svc.as_raw(), SERVICE_CONTROL_STOP, &mut status) };
        if ok == 0 {
            // SAFETY: Reads thread-local error set by `ControlService`.
            let err = unsafe { GetLastError() };
            if err == ERROR_SERVICE_NOT_ACTIVE {
                return Ok(());
            }
            return Err(map_win32_error(err, None, "ControlService(STOP)"));
        }

        Ok(())
    }

    fn restart_service(&self, name: &str) -> Result<(), PalError> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(PalError::InvalidArgument(
                "service name cannot be empty".into(),
            ));
        }

        let scm = open_scm(SC_MANAGER_CONNECT)?;
        // Request both START and STOP up front so insufficient privilege fails immediately
        // with `PalError::ElevationRequired` before mutating service state.
        let svc = open_service(
            &scm,
            trimmed,
            SERVICE_START | SERVICE_STOP | SERVICE_QUERY_STATUS,
        )?;

        let current = query_service_status_process(&svc)?;
        if current.dwCurrentState != SERVICE_STOPPED {
            let mut status: SERVICE_STATUS = unsafe { zeroed() };
            // SAFETY: `svc` is opened with `SERVICE_STOP`.
            let ok = unsafe { ControlService(svc.as_raw(), SERVICE_CONTROL_STOP, &mut status) };
            if ok == 0 {
                // SAFETY: Reads thread-local error set by `ControlService`.
                let err = unsafe { GetLastError() };
                if err != ERROR_SERVICE_NOT_ACTIVE {
                    return Err(map_win32_error(err, None, "ControlService(STOP)"));
                }
            }

            let deadline = Instant::now() + RESTART_STOP_TIMEOUT;
            loop {
                let polled = query_service_status_process(&svc)?;
                if polled.dwCurrentState == SERVICE_STOPPED {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(PalError::TimedOut(RESTART_STOP_TIMEOUT));
                }
                std::thread::sleep(RESTART_POLL_INTERVAL);
            }
        }

        // SAFETY: `svc` is opened with `SERVICE_START`.
        let ok = unsafe { StartServiceW(svc.as_raw(), 0, ptr::null()) };
        if ok == 0 {
            // SAFETY: Reads thread-local error set by `StartServiceW`.
            let err = unsafe { GetLastError() };
            if err == ERROR_SERVICE_ALREADY_RUNNING {
                return Ok(());
            }
            return Err(map_win32_error(err, None, "StartServiceW(restart)"));
        }

        Ok(())
    }

    fn open_services_console(&self) -> Result<(), PalError> {
        ensure_com_initialized();

        let wide_verb = to_wide_null("open");
        let wide_file = to_wide_null("services.msc");

        // SAFETY: `SHELLEXECUTEINFOW` is POD; wide string buffers outlive the call.
        let mut sei: SHELLEXECUTEINFOW = unsafe { zeroed() };
        sei.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        sei.fMask = SEE_MASK_NOASYNC;
        sei.lpVerb = wide_verb.as_ptr();
        sei.lpFile = wide_file.as_ptr();
        sei.nShow = SW_SHOWNORMAL;

        // SAFETY: All pointers in `sei` are valid null-terminated UTF-16 buffers.
        let ok = unsafe { ShellExecuteExW(&mut sei) };
        if ok == 0 {
            // SAFETY: Reads thread-local error set by `ShellExecuteExW`.
            let err = unsafe { GetLastError() };
            return Err(map_win32_error(err, None, "ShellExecuteExW(services.msc)"));
        }

        Ok(())
    }
}

struct EnumeratedServicesBuf {
    bytes: Vec<u8>,
    records_count: usize,
}

fn open_scm(desired_access: u32) -> Result<ScHandle, PalError> {
    // SAFETY: Null machine/database names open the local active SCM database.
    let raw = unsafe { OpenSCManagerW(null_wide_ptr(), null_wide_ptr(), desired_access) };
    ScHandle::from_raw(raw).ok_or_else(|| {
        // SAFETY: Reads thread-local error set by `OpenSCManagerW`.
        let err = unsafe { GetLastError() };
        map_win32_error(err, None, "OpenSCManagerW")
    })
}

fn open_service(scm: &ScHandle, name: &str, desired_access: u32) -> Result<ScHandle, PalError> {
    let wide_name = to_wide_null(name);
    // SAFETY: `scm` is valid and `wide_name` is null-terminated UTF-16.
    let raw = unsafe { OpenServiceW(scm.as_raw(), wide_name.as_ptr(), desired_access) };
    ScHandle::from_raw(raw).ok_or_else(|| {
        // SAFETY: Reads thread-local error set by `OpenServiceW`.
        let err = unsafe { GetLastError() };
        map_win32_error(err, None, "OpenServiceW")
    })
}

fn query_service_status_process(svc: &ScHandle) -> Result<SERVICE_STATUS_PROCESS, PalError> {
    let mut ssp: SERVICE_STATUS_PROCESS = unsafe { zeroed() };
    let mut bytes_needed: u32 = 0;
    // SAFETY: `svc` is opened with `SERVICE_QUERY_STATUS` and `ssp` matches `SC_STATUS_PROCESS_INFO`.
    let ok = unsafe {
        QueryServiceStatusEx(
            svc.as_raw(),
            SC_STATUS_PROCESS_INFO,
            (&mut ssp as *mut SERVICE_STATUS_PROCESS).cast::<u8>(),
            size_of::<SERVICE_STATUS_PROCESS>() as u32,
            &mut bytes_needed,
        )
    };
    if ok == 0 {
        // SAFETY: Reads thread-local error set by `QueryServiceStatusEx`.
        let err = unsafe { GetLastError() };
        return Err(map_win32_error(err, None, "QueryServiceStatusEx"));
    }
    Ok(ssp)
}

fn enumerate_win32_services(scm: &ScHandle) -> Result<EnumeratedServicesBuf, PalError> {
    let mut buf_size: usize = 64 * 1024;
    for _ in 0..5 {
        let mut bytes = vec![0u8; buf_size];
        let mut bytes_needed: u32 = 0;
        let mut services_returned: u32 = 0;
        let mut resume_handle: u32 = 0;

        // SAFETY: `bytes` is writable for `buf_size` bytes.
        let ok = unsafe {
            EnumServicesStatusExW(
                scm.as_raw(),
                SC_ENUM_PROCESS_INFO,
                SERVICE_WIN32,
                SERVICE_STATE_ALL,
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                &mut bytes_needed,
                &mut services_returned,
                &mut resume_handle,
                null_wide_ptr(),
            )
        };
        if ok != 0 {
            return Ok(EnumeratedServicesBuf {
                bytes,
                records_count: services_returned as usize,
            });
        }

        // SAFETY: Reads thread-local error set by `EnumServicesStatusExW`.
        let err = unsafe { GetLastError() };
        if err == ERROR_MORE_DATA {
            buf_size = (bytes.len() + bytes_needed as usize).next_power_of_two();
            continue;
        }
        return Err(map_win32_error(err, None, "EnumServicesStatusExW"));
    }

    Err(PalError::OsError {
        code: ERROR_MORE_DATA as i32,
        message: "EnumServicesStatusExW exceeded buffer retry limit".into(),
    })
}

fn open_services_registry_root() -> Option<RegKeyGuard> {
    let subkey = to_wide_null(r"SYSTEM\CurrentControlSet\Services");
    let mut hkey: HKEY = ptr::null_mut();
    // SAFETY: `subkey` is null-terminated UTF-16; `hkey` receives the opened registry key.
    let status =
        unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, subkey.as_ptr(), 0, KEY_READ, &mut hkey) };
    if status == ERROR_SUCCESS && !hkey.is_null() {
        Some(RegKeyGuard(hkey))
    } else {
        None
    }
}

fn read_service_registry_meta(
    services_root: HKEY,
    service_name: &str,
    indirect_deadline: Instant,
    indirect_remaining: &mut usize,
) -> CachedServiceMeta {
    let wide_sub = to_wide_null(service_name);
    let mut hkey: HKEY = ptr::null_mut();
    // SAFETY: `services_root` is an open registry key and `wide_sub` is null-terminated UTF-16.
    let status = unsafe { RegOpenKeyExW(services_root, wide_sub.as_ptr(), 0, KEY_READ, &mut hkey) };
    if status != ERROR_SUCCESS || hkey.is_null() {
        return CachedServiceMeta {
            group: None,
            description: None,
            pending_indirect: None,
        };
    }
    let key = RegKeyGuard(hkey);

    // Prefer `svchost.exe -k <Group>` (matches Windows Task Manager Services tab),
    // falling back to registry `Group` value.
    let group = read_reg_string(key.0, "ImagePath")
        .as_deref()
        .and_then(parse_svchost_group)
        .or_else(|| {
            read_reg_string(key.0, "Group").and_then(|s| {
                let t = s.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_owned())
                }
            })
        })
        .map(Arc::<str>::from);

    let mut pending_indirect = None;
    let description = match read_reg_string(key.0, "Description") {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else if trimmed.starts_with('@') {
                if *indirect_remaining > 0 && Instant::now() < indirect_deadline {
                    *indirect_remaining = indirect_remaining.saturating_sub(1);
                    resolve_indirect_string(trimmed).map(Arc::<str>::from)
                } else {
                    pending_indirect = Some(trimmed.to_owned());
                    None
                }
            } else {
                Some(Arc::from(trimmed))
            }
        }
        None => None,
    };

    CachedServiceMeta {
        group,
        description,
        pending_indirect,
    }
}

fn read_reg_string(hkey: HKEY, value_name: &str) -> Option<String> {
    let wide_name = to_wide_null(value_name);
    let mut reg_type: u32 = 0;
    let mut byte_len: u32 = 0;
    // SAFETY: Size-query call with null data buffer.
    let status = unsafe {
        RegQueryValueExW(
            hkey,
            wide_name.as_ptr(),
            ptr::null_mut(),
            &mut reg_type,
            ptr::null_mut(),
            &mut byte_len,
        )
    };
    if status != ERROR_SUCCESS
        || !(2..=32 * 1024).contains(&byte_len)
        || (reg_type != REG_SZ && reg_type != REG_EXPAND_SZ)
    {
        return None;
    }

    let u16_len = (byte_len as usize).div_ceil(2);
    let mut buf = vec![0u16; u16_len];
    let mut actual_bytes = (buf.len() * 2) as u32;
    // SAFETY: `buf` is writable for `actual_bytes`.
    let status = unsafe {
        RegQueryValueExW(
            hkey,
            wide_name.as_ptr(),
            ptr::null_mut(),
            &mut reg_type,
            buf.as_mut_ptr().cast::<u8>(),
            &mut actual_bytes,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }

    let valid_u16 = (actual_bytes as usize / 2).min(buf.len());
    let slice = &buf[..valid_u16];
    let nul_pos = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    Some(String::from_utf16_lossy(&slice[..nul_pos]))
}

/// Resolves an indirect Windows resource string (e.g. `@%SystemRoot%\system32\foo.dll,-100`)
/// using `SHLoadIndirectString`, bypassing UNC paths to prevent network hangs.
fn resolve_indirect_string(indirect: &str) -> Option<String> {
    let inner = indirect.strip_prefix('@').unwrap_or(indirect);
    if is_unc_path(inner) {
        return None;
    }

    let wide_src = to_wide_null(indirect);
    let mut out_buf = [0u16; 512];
    // SAFETY: `wide_src` is null-terminated UTF-16 and `out_buf` holds 512 UTF-16 code units.
    let hr = unsafe {
        SHLoadIndirectString(
            wide_src.as_ptr(),
            out_buf.as_mut_ptr(),
            out_buf.len() as u32,
            ptr::null_mut(),
        )
    };
    if hr != S_OK {
        return None;
    }
    let nul_pos = out_buf
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(out_buf.len());
    let decoded = String::from_utf16_lossy(&out_buf[..nul_pos]);
    let trimmed = decoded.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// Reads a null-terminated UTF-16 string from a Win32 `PWSTR` pointer.
///
/// # Safety
/// `ptr` must be either null or point to a valid null-terminated UTF-16 string.
unsafe fn read_wide_ptr(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    // Bounded scan prevents runaway reads on malformed memory.
    while len < 4096 {
        // SAFETY: Caller guarantees `ptr` is valid up to and including its NUL terminator.
        let ch = unsafe { *ptr.add(len) };
        if ch == 0 {
            break;
        }
        len += 1;
    }
    // SAFETY: `len` code units before NUL are valid readable UTF-16.
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    String::from_utf16_lossy(slice)
}

/// Maps a Win32 `SERVICE_STATUS_PROCESS::dwCurrentState` value into [`ServiceState`].
#[inline]
pub(crate) fn map_service_state(raw: u32) -> ServiceState {
    match raw {
        SERVICE_STOPPED => ServiceState::Stopped,
        SERVICE_START_PENDING => ServiceState::StartPending,
        SERVICE_STOP_PENDING => ServiceState::StopPending,
        SERVICE_RUNNING => ServiceState::Running,
        SERVICE_CONTINUE_PENDING => ServiceState::ContinuePending,
        SERVICE_PAUSE_PENDING => ServiceState::PausePending,
        SERVICE_PAUSED => ServiceState::Paused,
        _ => ServiceState::Unknown,
    }
}

/// Extracts the `svchost.exe -k <Group>` service host group name from a Windows service
/// registry `ImagePath` command line.
pub(crate) fn parse_svchost_group(image_path: &str) -> Option<String> {
    let lower = image_path.to_ascii_lowercase();
    if !lower.contains("svchost.exe") && !lower.contains("svchost ") {
        return None;
    }

    // Tokenize respecting simple double-quoted arguments.
    let mut rest = image_path.trim();
    let mut expect_group = false;

    while !rest.is_empty() {
        let (token, remaining) = next_cmd_token(rest);
        rest = remaining.trim_start();
        if expect_group {
            let trimmed = token.trim();
            return if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            };
        }
        if token.eq_ignore_ascii_case("-k") || token.eq_ignore_ascii_case("/k") {
            expect_group = true;
        }
    }

    None
}

fn next_cmd_token(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    if let Some(after_quote) = s.strip_prefix('"') {
        if let Some(end) = after_quote.find('"') {
            return (&after_quote[..end], &after_quote[end + 1..]);
        }
        return (after_quote, "");
    }
    match s.find(char::is_whitespace) {
        Some(idx) => (&s[..idx], &s[idx..]),
        None => (s, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_svchost_group_extracts_standard_and_quoted_groups() {
        assert_eq!(
            parse_svchost_group(r"%SystemRoot%\system32\svchost.exe -k netsvcs -p").as_deref(),
            Some("netsvcs")
        );
        assert_eq!(
            parse_svchost_group(
                r#""C:\Windows\System32\svchost.exe" -k LocalServiceNetworkRestricted -p"#
            )
            .as_deref(),
            Some("LocalServiceNetworkRestricted")
        );
        assert_eq!(
            parse_svchost_group(r#"C:\Windows\System32\SVCHOST.EXE /K "DcomLaunch""#).as_deref(),
            Some("DcomLaunch")
        );
        assert_eq!(
            parse_svchost_group(r"C:\Windows\System32\spoolsv.exe"),
            None
        );
        assert_eq!(
            parse_svchost_group(r"C:\Windows\System32\svchost.exe -p"),
            None
        );
    }

    #[test]
    fn map_service_state_covers_all_win32_states() {
        assert_eq!(map_service_state(SERVICE_STOPPED), ServiceState::Stopped);
        assert_eq!(
            map_service_state(SERVICE_START_PENDING),
            ServiceState::StartPending
        );
        assert_eq!(
            map_service_state(SERVICE_STOP_PENDING),
            ServiceState::StopPending
        );
        assert_eq!(map_service_state(SERVICE_RUNNING), ServiceState::Running);
        assert_eq!(
            map_service_state(SERVICE_CONTINUE_PENDING),
            ServiceState::ContinuePending
        );
        assert_eq!(
            map_service_state(SERVICE_PAUSE_PENDING),
            ServiceState::PausePending
        );
        assert_eq!(map_service_state(SERVICE_PAUSED), ServiceState::Paused);
        assert_eq!(map_service_state(999), ServiceState::Unknown);
    }
}
