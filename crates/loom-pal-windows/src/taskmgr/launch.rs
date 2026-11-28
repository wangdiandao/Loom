//! Process launch primitives for Task Manager replacement:
//! - Safe Windows command-line quoting
//! - Explorer-parent de-elevated spawn (`PROC_THREAD_ATTRIBUTE_PARENT_PROCESS` + `CreateEnvironmentBlock`)
//! - IFEO-bypassing native `taskmgr.exe` launch (`DEBUG_ONLY_THIS_PROCESS | CREATE_SUSPENDED` -> detach -> resume)
//! - `taskmgr.exe` `autoElevate` relaunch handoff via `Local\Loom.Taskmgr.Native.<PID>`

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::{Duration, Instant};

use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, FALSE, FILETIME, GetLastError, HANDLE, LocalFree, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ACCESS_MASK, TOKEN_DUPLICATE, TOKEN_ELEVATION_TYPE, TOKEN_QUERY,
    TOKEN_USER, TokenElevationType, TokenElevationTypeFull, TokenElevationTypeLimited, TokenUser,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    DebugActiveProcessStop, DebugSetProcessKillOnExit,
};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateEventW, CreateProcessW,
    DEBUG_ONLY_THIS_PROCESS, DeleteProcThreadAttributeList, EVENT_MODIFY_STATE,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess, GetProcessTimes,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenEventW, OpenProcess,
    OpenProcessToken, PROCESS_CREATE_PROCESS, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, ResumeThread, STARTUPINFOEXW,
    STARTUPINFOW, SetEvent, TerminateProcess, UpdateProcThreadAttribute, WaitForMultipleObjects,
    WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};

use crate::handle::{OwnedHandle, to_wide_null};

/// `PROC_THREAD_ATTRIBUTE_PARENT_PROCESS` (`0x0002_0000`).
const PROC_THREAD_ATTRIBUTE_PARENT_PROCESS: usize = 0x0002_0000;

/// Maximum time to wait for a limited-token `taskmgr.exe` to request its `autoElevate` copy.
const RELAUNCH_WAIT: Duration = Duration::from_secs(60);
/// Grace period after the initial `taskmgr.exe` exits for the elevated IFEO launcher to answer.
const ANSWER_WAIT: Duration = Duration::from_secs(15);

/// Quotes a single Windows command-line argument per `CommandLineToArgvW` rules.
pub fn quote_arg(value: &str) -> String {
    let mut result = String::with_capacity(value.len() + 2);
    result.push('"');
    let mut slashes = 0usize;
    for ch in value.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        if ch == '"' {
            for _ in 0..(slashes * 2 + 1) {
                result.push('\\');
            }
            result.push('"');
        } else {
            for _ in 0..slashes {
                result.push('\\');
            }
            result.push(ch);
        }
        slashes = 0;
    }
    for _ in 0..(slashes * 2) {
        result.push('\\');
    }
    result.push('"');
    result
}

/// Returns `%SystemRoot%\System32` via `GetSystemDirectoryW`.
pub fn system_directory() -> Result<PathBuf, String> {
    let mut buf = [0u16; 1024];
    // SAFETY: `buf` is a writable UTF-16 buffer of length `buf.len()`.
    let len = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if len == 0 || len >= buf.len() {
        return Err("GetSystemDirectoryW failed".into());
    }
    Ok(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

/// Returns the current process creation timestamp (`FILETIME` 100ns ticks).
pub(crate) fn process_create_time(process: HANDLE) -> Result<u64, String> {
    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    // SAFETY: `process` is a valid handle with at least `PROCESS_QUERY_LIMITED_INFORMATION`.
    let ok = unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) };
    if ok == 0 {
        let code = unsafe { GetLastError() };
        return Err(format!("GetProcessTimes failed (os error {code})"));
    }
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

/// Returns the string SID (e.g., `"S-1-5-21-..."`) of the specified process token.
pub(crate) fn token_user_sid(token: HANDLE) -> Result<String, String> {
    let mut size: u32 = 0;
    // SAFETY: Queries required buffer size for `TokenUser`.
    unsafe {
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut size);
    }
    if size == 0 || size > 64 * 1024 {
        let code = unsafe { GetLastError() };
        return Err(format!(
            "GetTokenInformation(TokenUser) failed (os error {code})"
        ));
    }
    let mut buf = vec![0u64; (size as usize).div_ceil(8)];
    // SAFETY: `buf` is 8-byte aligned and at least `size` bytes long.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast::<c_void>(),
            size,
            &mut size,
        )
    };
    if ok == 0 {
        let code = unsafe { GetLastError() };
        return Err(format!(
            "GetTokenInformation(TokenUser) failed (os error {code})"
        ));
    }
    // SAFETY: `buf` holds a valid `TOKEN_USER` structure populated by `GetTokenInformation`.
    let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
    let mut sid_str: *mut u16 = ptr::null_mut();
    // SAFETY: `user.User.Sid` is valid for the lifetime of `buf`.
    let ok = unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid_str) };
    if ok == 0 || sid_str.is_null() {
        let code = unsafe { GetLastError() };
        return Err(format!("ConvertSidToStringSidW failed (os error {code})"));
    }
    let sid = unsafe {
        let mut len = 0usize;
        while *sid_str.add(len) != 0 {
            len += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(sid_str, len));
        LocalFree(sid_str.cast::<c_void>());
        s
    };
    Ok(sid)
}

/// Returns the string SID of the current process's user account.
pub fn current_user_sid() -> Result<String, String> {
    let token = own_token(TOKEN_QUERY)?;
    token_user_sid(token.as_raw())
}

/// Queries `TokenElevationType` (`Default`, `Full`, or `Limited`) for `token`.
pub(crate) fn token_elevation_type(token: HANDLE) -> Result<TOKEN_ELEVATION_TYPE, String> {
    let mut kind: TOKEN_ELEVATION_TYPE = 0;
    let mut size: u32 = 0;
    // SAFETY: `kind` matches the exact size of `TOKEN_ELEVATION_TYPE`.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevationType,
            (&mut kind as *mut TOKEN_ELEVATION_TYPE).cast::<c_void>(),
            size_of::<TOKEN_ELEVATION_TYPE>() as u32,
            &mut size,
        )
    };
    if ok == 0 {
        let code = unsafe { GetLastError() };
        return Err(format!(
            "GetTokenInformation(TokenElevationType) failed (os error {code})"
        ));
    }
    Ok(kind)
}

fn process_token(process: HANDLE, access: TOKEN_ACCESS_MASK) -> Result<OwnedHandle, String> {
    let mut raw: HANDLE = ptr::null_mut();
    // SAFETY: `process` is a valid process handle.
    let ok = unsafe { OpenProcessToken(process, access, &mut raw) };
    if ok == 0 {
        let code = unsafe { GetLastError() };
        return Err(format!("OpenProcessToken failed (os error {code})"));
    }
    OwnedHandle::from_raw(raw).ok_or_else(|| "OpenProcessToken returned null".into())
}

fn own_token(access: TOKEN_ACCESS_MASK) -> Result<OwnedHandle, String> {
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle.
    process_token(unsafe { GetCurrentProcess() }, access)
}

/// Locates the desktop shell (`explorer.exe`) belonging to the same user account with a
/// non-elevated token so an elevated launcher can spawn a standard-privilege UI child.
fn find_standard_shell_parent() -> Result<OwnedHandle, String> {
    // SAFETY: Returns the desktop shell window HWND (owned by `explorer.exe`).
    let hwnd = unsafe { GetShellWindow() };
    if hwnd.is_null() {
        return Err("desktop shell window is unavailable".into());
    }
    let mut shell_pid: u32 = 0;
    // SAFETY: `shell_pid` is a valid out-pointer.
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut shell_pid);
    }
    if shell_pid == 0 {
        return Err("failed to resolve desktop shell PID".into());
    }
    // SAFETY: Opens `explorer.exe` to serve as parent for `PROC_THREAD_ATTRIBUTE_PARENT_PROCESS`.
    let raw = unsafe {
        OpenProcess(
            PROCESS_CREATE_PROCESS | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            FALSE,
            shell_pid,
        )
    };
    let shell_proc = OwnedHandle::from_raw(raw).ok_or_else(|| {
        let code = unsafe { GetLastError() };
        format!("OpenProcess(shell) failed (os error {code})")
    })?;

    let shell_tok = process_token(shell_proc.as_raw(), TOKEN_QUERY)?;
    let own_tok = own_token(TOKEN_QUERY)?;
    if token_user_sid(shell_tok.as_raw())? != token_user_sid(own_tok.as_raw())? {
        return Err("desktop shell belongs to a different user account".into());
    }
    if token_elevation_type(shell_tok.as_raw())? == TokenElevationTypeFull {
        return Err("desktop shell is also elevated; no standard token available".into());
    }
    Ok(shell_proc)
}

/// RAII wrapper around `PROC_THREAD_ATTRIBUTE_LIST` setting `PROC_THREAD_ATTRIBUTE_PARENT_PROCESS`.
struct ParentAttribute {
    storage: Vec<u64>,
    parent: Box<HANDLE>,
}

impl ParentAttribute {
    fn new(parent: HANDLE) -> Result<Self, String> {
        let mut size: usize = 0;
        // SAFETY: First call returns required byte size in `size`.
        unsafe {
            InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size);
        }
        if size == 0 {
            let code = unsafe { GetLastError() };
            return Err(format!(
                "InitializeProcThreadAttributeList(size) failed (os error {code})"
            ));
        }
        let mut storage = vec![0u64; size.div_ceil(8)];
        // SAFETY: `storage` is 8-byte aligned and has length >= `size`.
        let ok = unsafe {
            InitializeProcThreadAttributeList(
                storage.as_mut_ptr().cast::<c_void>() as LPPROC_THREAD_ATTRIBUTE_LIST,
                1,
                0,
                &mut size,
            )
        };
        if ok == 0 {
            let code = unsafe { GetLastError() };
            return Err(format!(
                "InitializeProcThreadAttributeList failed (os error {code})"
            ));
        }
        let mut attr = Self {
            storage,
            parent: Box::new(parent),
        };
        let value_ptr = (&*attr.parent as *const HANDLE).cast::<c_void>();
        // SAFETY: `attr.parent` is boxed and outlives `attr.storage`.
        let ok = unsafe {
            UpdateProcThreadAttribute(
                attr.as_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_PARENT_PROCESS,
                value_ptr,
                size_of::<HANDLE>(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if ok == 0 {
            let code = unsafe { GetLastError() };
            return Err(format!(
                "UpdateProcThreadAttribute failed (os error {code})"
            ));
        }
        Ok(attr)
    }

    fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_mut_ptr().cast::<c_void>() as LPPROC_THREAD_ATTRIBUTE_LIST
    }
}

impl Drop for ParentAttribute {
    fn drop(&mut self) {
        // SAFETY: `self.storage` was initialized by `InitializeProcThreadAttributeList`.
        unsafe {
            DeleteProcThreadAttributeList(self.as_ptr());
        }
    }
}

/// RAII wrapper around `CreateEnvironmentBlock` / `DestroyEnvironmentBlock`.
struct UserEnvironment(*mut c_void);

impl UserEnvironment {
    fn for_current_user() -> Result<Self, String> {
        let token = own_token(TOKEN_QUERY | TOKEN_DUPLICATE)?;
        let mut block: *mut c_void = ptr::null_mut();
        // SAFETY: Rebuilds a fresh environment block for this account without inheriting caller vars.
        let ok = unsafe { CreateEnvironmentBlock(&mut block, token.as_raw(), FALSE) };
        if ok == 0 || block.is_null() {
            let code = unsafe { GetLastError() };
            return Err(format!("CreateEnvironmentBlock failed (os error {code})"));
        }
        Ok(Self(block))
    }
}

impl Drop for UserEnvironment {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` was returned by `CreateEnvironmentBlock`.
            unsafe {
                let _ = DestroyEnvironmentBlock(self.0);
            }
        }
    }
}

/// Spawns `exe` with standard user privileges: if the current process holds an elevated split
/// token (`TokenElevationTypeFull`), spawns as a child of `explorer.exe` to inherit its standard
/// token, rebuilding the account's environment block.
pub(crate) fn standard_spawn(exe: &Path, args: &[String]) -> Result<OwnedHandle, String> {
    let own = own_token(TOKEN_QUERY)?;
    let shell_parent = if token_elevation_type(own.as_raw())? == TokenElevationTypeFull {
        Some(find_standard_shell_parent()?)
    } else {
        None
    };
    let (proc, ()) = start_process(
        exe,
        args,
        false,
        shell_parent.as_ref().map(OwnedHandle::as_raw),
        true,
        |_| Ok(()),
    )?;
    Ok(proc)
}

/// Spawns `exe` directly, optionally bypassing IFEO via `DEBUG_ONLY_THIS_PROCESS | CREATE_SUSPENDED`.
pub(crate) fn spawn(exe: &Path, args: &[String], bypass_ifeo: bool) -> Result<OwnedHandle, String> {
    let (proc, ()) = start_process(exe, args, bypass_ifeo, None, false, |_| Ok(()))?;
    Ok(proc)
}

fn start_process<T>(
    exe: &Path,
    args: &[String],
    bypass_ifeo: bool,
    parent: Option<HANDLE>,
    rebuild_env: bool,
    before_resume: impl FnOnce(u32) -> Result<T, String>,
) -> Result<(OwnedHandle, T), String> {
    let wide_exe = to_wide_null(&exe.to_string_lossy());
    let cmd_str = std::iter::once(exe.to_string_lossy().into_owned())
        .chain(args.iter().cloned())
        .map(|a| quote_arg(&a))
        .collect::<Vec<_>>()
        .join(" ");
    let mut wide_cmd = to_wide_null(&cmd_str);
    let wide_dir = to_wide_null(&system_directory()?.to_string_lossy());

    // SAFETY: `STARTUPINFOEXW` and `PROCESS_INFORMATION` are plain C POD structs.
    let mut si: STARTUPINFOEXW = unsafe { zeroed() };
    si.StartupInfo.cb = size_of::<STARTUPINFOW>() as u32;

    let mut flags = CREATE_NO_WINDOW;
    if bypass_ifeo {
        flags |= DEBUG_ONLY_THIS_PROCESS | CREATE_SUSPENDED;
    }

    let mut parent_attr = parent.map(ParentAttribute::new).transpose()?;
    if let Some(ref mut attr) = parent_attr {
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = attr.as_ptr();
        flags |= EXTENDED_STARTUPINFO_PRESENT;
    }

    let env_block = if rebuild_env {
        Some(UserEnvironment::for_current_user()?)
    } else {
        None
    };
    if env_block.is_some() {
        flags |= CREATE_UNICODE_ENVIRONMENT;
    }

    let mut pi: PROCESS_INFORMATION = unsafe { zeroed() };
    // SAFETY: All pointers remain valid for the duration of `CreateProcessW`.
    let ok = unsafe {
        CreateProcessW(
            wide_exe.as_ptr(),
            wide_cmd.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            FALSE,
            flags,
            env_block.as_ref().map_or(ptr::null(), |e| e.0.cast_const()),
            wide_dir.as_ptr(),
            &si.StartupInfo,
            &mut pi,
        )
    };
    if ok == 0 {
        let code = unsafe { GetLastError() };
        return Err(format!("CreateProcessW failed (os error {code})"));
    }

    let proc_handle = OwnedHandle::from_raw(pi.hProcess)
        .ok_or_else(|| "CreateProcessW returned null hProcess".to_owned())?;
    let thread_handle = OwnedHandle::from_raw(pi.hThread)
        .ok_or_else(|| "CreateProcessW returned null hThread".to_owned())?;

    let terminate_new = || unsafe {
        let _ = TerminateProcess(proc_handle.as_raw(), 1);
        let _ = WaitForSingleObject(proc_handle.as_raw(), 1000);
    };

    let value = match before_resume(pi.dwProcessId) {
        Ok(v) => v,
        Err(err) => {
            terminate_new();
            return Err(err);
        }
    };

    if bypass_ifeo {
        // Detach the debug session on the same thread BEFORE resuming the suspended main thread.
        let detach_res: Result<(), String> = (|| {
            // SAFETY: This thread created `pi.dwProcessId` with `DEBUG_ONLY_THIS_PROCESS`.
            if unsafe { DebugSetProcessKillOnExit(FALSE) } == 0 {
                let code = unsafe { GetLastError() };
                return Err(format!(
                    "DebugSetProcessKillOnExit failed (os error {code})"
                ));
            }
            if unsafe { DebugActiveProcessStop(pi.dwProcessId) } == 0 {
                let code = unsafe { GetLastError() };
                return Err(format!("DebugActiveProcessStop failed (os error {code})"));
            }
            if unsafe { ResumeThread(thread_handle.as_raw()) } == u32::MAX {
                let code = unsafe { GetLastError() };
                return Err(format!("ResumeThread failed (os error {code})"));
            }
            Ok(())
        })();
        if let Err(err) = detach_res {
            terminate_new();
            return Err(err);
        }
    }

    Ok((proc_handle, value))
}

/// Waits up to `timeout` for `process` to exit and returns its Win32 exit code.
pub(crate) fn wait_exit_code(process: &OwnedHandle, timeout: Duration) -> Result<u32, String> {
    let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
    // SAFETY: `process` is a valid process handle.
    if unsafe { WaitForSingleObject(process.as_raw(), ms) } != WAIT_OBJECT_0 {
        unsafe {
            let _ = TerminateProcess(process.as_raw(), 1);
            let _ = WaitForSingleObject(process.as_raw(), 1000);
        }
        return Err("probe child process timed out".into());
    }
    let mut exit_code: u32 = 0;
    // SAFETY: Queries exit code of the exited process.
    if unsafe { GetExitCodeProcess(process.as_raw(), &mut exit_code) } == 0 {
        let code = unsafe { GetLastError() };
        return Err(format!("GetExitCodeProcess failed (os error {code})"));
    }
    Ok(exit_code)
}

/// Handle to a native Windows `taskmgr.exe` launched past IFEO, together with its optional
/// `autoElevate` handoff marker event (`Local\Loom.Taskmgr.Native.<PID>`).
pub struct NativeTaskManager {
    process: OwnedHandle,
    marker: Option<OwnedHandle>,
}

impl NativeTaskManager {
    /// Holds the process handle and marker event until the elevated IFEO launcher signals it,
    /// `ANSWER_WAIT` elapses after `taskmgr.exe` exits, or `RELAUNCH_WAIT` elapses.
    pub fn wait(self) {
        let Some(marker) = self.marker else {
            return;
        };
        let handles = [marker.as_raw(), self.process.as_raw()];
        let wait_ms = RELAUNCH_WAIT.as_millis() as u32;
        // SAFETY: Both handles are valid synchronization handles.
        let res = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), FALSE, wait_ms) };
        // If index 1 (`self.process`) exited first (typical `autoElevate` exit ~0.2s), keep
        // holding `self.process` and `marker` for `ANSWER_WAIT` so PID is not reused before
        // the elevated IFEO launcher answers `marker`.
        if res == WAIT_OBJECT_0 + 1 {
            unsafe {
                let _ = WaitForSingleObject(marker.as_raw(), ANSWER_WAIT.as_millis() as u32);
            }
        }
    }
}

/// Launches the fixed `%SystemRoot%\System32\taskmgr.exe` directly past IFEO.
pub fn launch_native_task_manager() -> Result<NativeTaskManager, String> {
    let is_limited_split = own_token(TOKEN_QUERY)
        .and_then(|tok| token_elevation_type(tok.as_raw()))
        .is_ok_and(|kind| kind == TokenElevationTypeLimited);

    let exe = system_directory()?.join("taskmgr.exe");
    let (process, marker) = start_process(&exe, &[], true, None, false, |pid| {
        Ok(if is_limited_split {
            create_native_marker(pid)
        } else {
            None
        })
    })?;
    Ok(NativeTaskManager { process, marker })
}

fn native_marker_name(pid: u32) -> Vec<u16> {
    to_wide_null(&format!("Local\\Loom.Taskmgr.Native.{pid}"))
}

fn create_native_marker(pid: u32) -> Option<OwnedHandle> {
    let wide = native_marker_name(pid);
    // SAFETY: Creates a manual-reset, initially unsignaled session-local event.
    let raw = unsafe { CreateEventW(ptr::null(), 1, 0, wide.as_ptr()) };
    let handle = OwnedHandle::from_raw(raw)?;
    // Refuse if another process already owned that event name.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return None;
    }
    Some(handle)
}

/// Returns `true` when this IFEO-invoked process is the elevated `autoElevate` relaunch
/// requested by a native `taskmgr.exe` that Loom itself just opened.
///
/// Signals the starter's `Local\Loom.Taskmgr.Native.<parent_pid>` marker event so the starter
/// can release its parent handle.
pub fn is_relaunched_native_task_manager() -> bool {
    let Some(parent_pid) = current_parent_pid() else {
        return false;
    };
    let wide = native_marker_name(parent_pid);
    // SAFETY: Opens session-local event if the native starter created it for `parent_pid`.
    let raw_event = unsafe { OpenEventW(EVENT_MODIFY_STATE, FALSE, wide.as_ptr()) };
    let Some(marker) = OwnedHandle::from_raw(raw_event) else {
        return false;
    };
    // Guard against PID reuse: verify `parent_pid` started before or at the same time as us.
    let raw_parent = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, parent_pid) };
    let Some(parent_proc) = OwnedHandle::from_raw(raw_parent) else {
        return false;
    };
    let parent_created = process_create_time(parent_proc.as_raw());
    let own_created = process_create_time(unsafe { GetCurrentProcess() });
    if !matches!((parent_created, own_created), (Ok(p), Ok(o)) if p <= o) {
        return false;
    }
    // SAFETY: `marker` was opened with `EVENT_MODIFY_STATE`.
    unsafe {
        let _ = SetEvent(marker.as_raw());
    }
    true
}

#[repr(C)]
#[derive(Default)]
struct ProcessBasicInfo {
    exit_status: i32,
    peb_base_address: usize,
    affinity_mask: usize,
    base_priority: i32,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
}

fn current_parent_pid() -> Option<u32> {
    let mut info = ProcessBasicInfo::default();
    let mut ret_len: u32 = 0;
    // SAFETY: Queries `ProcessBasicInformation` for the current process.
    let status = unsafe {
        NtQueryInformationProcess(
            GetCurrentProcess(),
            ProcessBasicInformation,
            (&mut info as *mut ProcessBasicInfo).cast::<c_void>(),
            size_of::<ProcessBasicInfo>() as u32,
            &mut ret_len,
        )
    };
    if status >= 0 && info.inherited_from_unique_process_id != 0 {
        Some(info.inherited_from_unique_process_id as u32)
    } else {
        None
    }
}

/// Waits until `event` is signaled, returning `false` early if `parent` exits or `timeout` expires.
pub(crate) fn wait_event_while_alive(event: HANDLE, parent: HANDLE, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if unsafe { WaitForSingleObject(parent, 0) } == WAIT_OBJECT_0 {
            return false;
        }
        if unsafe { WaitForSingleObject(event, 20) } == WAIT_OBJECT_0 {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_arguments_are_quoted_without_shell_interpretation() {
        assert_eq!(quote_arg(r"C:\with space\"), r#""C:\with space\\""#);
        assert_eq!(quote_arg(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote_arg("--task-manager"), r#""--task-manager""#);
    }

    #[test]
    #[ignore = "child fixture invoked by detached_debug_start_runs_to_completion"]
    fn detached_child_fixture() {
        assert_eq!(2 + 2, 4);
    }

    #[test]
    fn detached_debug_start_runs_to_completion() {
        let proc = spawn(
            &std::env::current_exe().unwrap(),
            &[
                "--exact".into(),
                "taskmgr::launch::tests::detached_child_fixture".into(),
                "--ignored".into(),
            ],
            true,
        )
        .expect("debug-bypass spawn should succeed");
        let code = wait_exit_code(&proc, Duration::from_secs(10)).expect("wait exit");
        assert_eq!(code, 0);
    }

    #[test]
    #[ignore = "child fixture invoked by auto_elevate_relaunch_marker_is_recognized_by_child"]
    fn relaunch_child_fixture() {
        assert!(is_relaunched_native_task_manager());
    }

    #[test]
    #[ignore = "parent fixture invoked by auto_elevate_relaunch_marker_is_recognized_by_child"]
    fn marked_parent_fixture() {
        let child = spawn(
            &std::env::current_exe().unwrap(),
            &[
                "--exact".into(),
                "taskmgr::launch::tests::relaunch_child_fixture".into(),
                "--ignored".into(),
            ],
            false,
        )
        .unwrap();
        assert_eq!(wait_exit_code(&child, Duration::from_secs(15)).unwrap(), 0);
    }

    #[test]
    fn auto_elevate_relaunch_marker_is_recognized_by_child() {
        let (parent, marker) = start_process(
            &std::env::current_exe().unwrap(),
            &[
                "--exact".into(),
                "taskmgr::launch::tests::marked_parent_fixture".into(),
                "--ignored".into(),
            ],
            true,
            None,
            false,
            |pid| Ok(create_native_marker(pid)),
        )
        .expect("start marked parent");
        let marker = marker.expect("create marker event");
        assert_eq!(wait_exit_code(&parent, Duration::from_secs(20)).unwrap(), 0);
        assert_eq!(
            unsafe { WaitForSingleObject(marker.as_raw(), 0) },
            WAIT_OBJECT_0
        );
        // Existing marker name must not be double-claimed while held.
        assert!(
            create_native_marker(unsafe {
                windows_sys::Win32::System::Threading::GetProcessId(parent.as_raw())
            })
            .is_none()
        );
        // Unmarked parent returns false.
        assert!(!is_relaunched_native_task_manager());
    }
}
