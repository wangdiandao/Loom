//! Desktop shell integration, de-elevated task launch, Explorer restart, UAC relaunch,
//! JIT debugger attachment (`AeDebug`), and file/search dialogs.

use std::ffi::c_void;
use std::mem::{size_of, transmute, zeroed};
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::Duration;

use loom_pal_api::PalError;
use loom_types::{ProcessKey, RunTaskRequest};
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, FALSE, GetLastError, HANDLE};
use windows_sys::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, SecurityImpersonation, TOKEN_ADJUST_DEFAULT,
    TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_QUERY,
    TokenElevation, TokenPrimary,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_EXPAND_SZ, REG_SZ, RegOpenKeyExW, RegQueryValueExW,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetCurrentProcess, GetProcessId, OpenProcess, OpenProcessToken,
    PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows_sys::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::Shell::{
    SEE_MASK_INVOKEIDLIST, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetShellWindow, GetWindowThreadProcessId, SW_SHOW, SW_SHOWNORMAL,
};
use windows_sys::core::BOOL;

use crate::handle::{
    OwnedHandle, OwnedModule, OwnedRegKey, decode_wide_nul, map_win32_error, null_wide_ptr,
    open_verified_process, os_str_to_wide_null, to_wide_null,
};
use crate::slow_prober::ensure_com_initialized;

/// Double-null-terminated Win32 `OPENFILENAMEW` filter for executable file selection.
pub(super) const BROWSE_EXECUTABLE_FILTER: &str = "Executables (*.exe;*.bat;*.cmd;*.msc;*.com)\0*.exe;*.bat;*.cmd;*.msc;*.com\0All Files (*.*)\0*.*\0\0";

/// `LOGON_WITH_PROFILE` flag for `advapi32!CreateProcessWithTokenW`.
const LOGON_WITH_PROFILE: u32 = 0x0000_0001;

const AEDEBUG_PATHS: [&str; 2] = [
    r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\AeDebug",
    r"SOFTWARE\WOW6432Node\Microsoft\Windows NT\CurrentVersion\AeDebug",
];

type CreateProcessWithTokenWFn = unsafe extern "system" fn(
    h_token: HANDLE,
    dw_logon_flags: u32,
    lp_application_name: *const u16,
    lp_command_line: *mut u16,
    dw_creation_flags: u32,
    lp_environment: *const c_void,
    lp_current_directory: *const u16,
    lp_startup_info: *const STARTUPINFOW,
    lp_process_information: *mut PROCESS_INFORMATION,
) -> BOOL;

pub(super) fn run_new_task(
    caller_elevated: bool,
    req: &RunTaskRequest,
) -> Result<Option<u32>, PalError> {
    let trimmed = req.command.trim();
    if trimmed.is_empty() {
        return Err(PalError::InvalidArgument("command cannot be empty".into()));
    }

    // When Loom itself is running elevated and the user did NOT request an elevated task,
    // attempt to launch de-elevated via the desktop Shell primary token before falling back
    // to `ShellExecuteExW` (e.g., for URLs, documents, or shell verbs).
    if caller_elevated
        && !req.run_as_admin
        && let Some(token) = duplicate_shell_primary_token()
        && let Some(pid) = spawn_with_token(&token, trimmed)
    {
        return Ok(Some(pid));
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

pub(super) fn attach_debugger(key: ProcessKey) -> Result<(), PalError> {
    if key.pid == 0 || key.pid == 4 {
        return Err(PalError::Unsupported(
            "attaching a debugger is not supported for Windows kernel system processes",
        ));
    }

    // Verify target process identity before reading AeDebug or launching any debugger.
    let _verified = open_verified_process(key, PROCESS_QUERY_LIMITED_INFORMATION)?;

    let template = read_aedebug_command()?.ok_or(PalError::Unsupported(
        "No JIT debugger configured in Windows AeDebug registry",
    ))?;

    let command_line = expand_env_vars(&format_aedebug_command(&template, key.pid));
    let mut wide_cmd = to_wide_null(&command_line);

    // SAFETY: `STARTUPINFOW` and `PROCESS_INFORMATION` are plain C POD structs.
    let mut si: STARTUPINFOW = unsafe { zeroed() };
    si.cb = size_of::<STARTUPINFOW>() as u32;
    let mut pi: PROCESS_INFORMATION = unsafe { zeroed() };

    // SAFETY: `wide_cmd` is a mutable null-terminated UTF-16 command line as required by `CreateProcessW`.
    let ok = unsafe {
        CreateProcessW(
            null_wide_ptr(),
            wide_cmd.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            FALSE,
            0,
            ptr::null(),
            null_wide_ptr(),
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        // SAFETY: Reads thread-local Win32 error set by `CreateProcessW`.
        let err = unsafe { GetLastError() };
        return Err(map_win32_error(
            err,
            Some(key),
            "CreateProcessW(attach_debugger)",
        ));
    }

    // Close spawned debugger process and primary thread handles immediately via RAII.
    let _proc = OwnedHandle::from_raw(pi.hProcess);
    let _thread = OwnedHandle::from_raw(pi.hThread);
    Ok(())
}

pub(super) fn restart_explorer(caller_elevated: bool) -> Result<(), PalError> {
    // Capture the non-elevated desktop shell token BEFORE terminating `explorer.exe`
    // so an elevated Loom instance can relaunch Explorer de-elevated cleanly.
    let shell_token = if caller_elevated {
        duplicate_shell_primary_token()
    } else {
        None
    };

    let my_session = current_process_session_id();
    let explorer_pids = find_session_explorer_pids(my_session);

    for pid in explorer_pids {
        // SAFETY: Opens existing user-session `explorer.exe` for termination and wait.
        let raw = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, FALSE, pid) };
        let Some(handle) = OwnedHandle::from_raw(raw) else {
            continue;
        };
        // SAFETY: `handle` has `PROCESS_TERMINATE` and `PROCESS_SYNCHRONIZE`.
        let ok = unsafe { TerminateProcess(handle.as_raw(), 1) };
        if ok != 0 {
            // SAFETY: Bounded 2 s wait for Explorer teardown.
            unsafe {
                WaitForSingleObject(handle.as_raw(), 2_000);
            }
        }
    }

    // Give Windows `AutoRestartShell` a brief window before spawning a replacement manually.
    std::thread::sleep(Duration::from_millis(150));
    if !find_session_explorer_pids(my_session).is_empty() {
        return Ok(());
    }

    let explorer_cmd = explorer_exe_path();
    if let Some(ref token) = shell_token
        && spawn_with_token(token, &explorer_cmd).is_some()
    {
        return Ok(());
    }

    let mut wide_cmd = to_wide_null(&explorer_cmd);
    // SAFETY: `STARTUPINFOW` and `PROCESS_INFORMATION` are plain C POD structs.
    let mut si: STARTUPINFOW = unsafe { zeroed() };
    si.cb = size_of::<STARTUPINFOW>() as u32;
    let mut pi: PROCESS_INFORMATION = unsafe { zeroed() };
    // SAFETY: `wide_cmd` is a mutable null-terminated UTF-16 command line.
    let ok = unsafe {
        CreateProcessW(
            null_wide_ptr(),
            wide_cmd.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            FALSE,
            0,
            ptr::null(),
            null_wide_ptr(),
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        // SAFETY: Reads thread-local Win32 error set by `CreateProcessW`.
        let err = unsafe { GetLastError() };
        return Err(map_win32_error(err, None, "CreateProcessW(explorer.exe)"));
    }
    let _proc = OwnedHandle::from_raw(pi.hProcess);
    let _thread = OwnedHandle::from_raw(pi.hThread);
    Ok(())
}

pub(super) fn relaunch_self_elevated(restore_route: &str) -> Result<(), PalError> {
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

pub(super) fn open_file_location(path: &Path) -> Result<(), PalError> {
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

pub(super) fn show_file_properties(path: &Path) -> Result<(), PalError> {
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

pub(super) fn search_online(query: &str) -> Result<(), PalError> {
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

pub(super) fn browse_executable() -> Result<Option<PathBuf>, PalError> {
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

    let decoded = decode_wide_nul(&file_buf);
    if decoded.is_empty() {
        return Ok(None);
    }

    Ok(Some(PathBuf::from(decoded)))
}

pub(super) fn is_elevated() -> bool {
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

/// Duplicates the non-elevated desktop shell (`explorer.exe`) primary token via `GetShellWindow`.
fn duplicate_shell_primary_token() -> Option<OwnedHandle> {
    // SAFETY: `GetShellWindow` takes no arguments and returns null if no shell window exists.
    let shell_hwnd = unsafe { GetShellWindow() };
    if shell_hwnd.is_null() {
        return None;
    }
    let mut shell_pid: u32 = 0;
    // SAFETY: `shell_hwnd` is non-null and `shell_pid` is a valid stack local.
    unsafe {
        GetWindowThreadProcessId(shell_hwnd, &mut shell_pid);
    }
    if shell_pid == 0 {
        return None;
    }

    // SAFETY: Opens the shell process with minimal read-only query rights.
    let raw_proc = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, shell_pid) };
    let shell_proc = OwnedHandle::from_raw(raw_proc)?;

    let mut raw_token: HANDLE = ptr::null_mut();
    // SAFETY: Requests `TOKEN_DUPLICATE` on the shell process token.
    let ok = unsafe { OpenProcessToken(shell_proc.as_raw(), TOKEN_DUPLICATE, &mut raw_token) };
    if ok == 0 {
        return None;
    }
    let shell_token = OwnedHandle::from_raw(raw_token)?;

    let desired_access = TOKEN_ASSIGN_PRIMARY
        | TOKEN_DUPLICATE
        | TOKEN_QUERY
        | TOKEN_ADJUST_DEFAULT
        | TOKEN_ADJUST_SESSIONID;
    let mut raw_primary: HANDLE = ptr::null_mut();
    // SAFETY: Duplicates `shell_token` as a primary token suitable for `CreateProcessWithTokenW`.
    let dup_ok = unsafe {
        DuplicateTokenEx(
            shell_token.as_raw(),
            desired_access,
            ptr::null(),
            SecurityImpersonation,
            TokenPrimary,
            &mut raw_primary,
        )
    };
    if dup_ok == 0 {
        return None;
    }
    OwnedHandle::from_raw(raw_primary)
}

/// Spawns `command_line` under a duplicated primary user token via dynamically loaded
/// `advapi32!CreateProcessWithTokenW`, returning the spawned PID on success.
fn spawn_with_token(primary_token: &OwnedHandle, command_line: &str) -> Option<u32> {
    let advapi32 = OwnedModule::load("advapi32.dll").ok()?;
    let addr = advapi32.proc_addr(b"CreateProcessWithTokenW\0")?;
    // SAFETY: Signature matches `CreateProcessWithTokenW` in `winbase.h`.
    let create_proc_with_token: CreateProcessWithTokenWFn = unsafe { transmute(addr) };

    let mut wide_cmd = to_wide_null(command_line);
    // SAFETY: `STARTUPINFOW` and `PROCESS_INFORMATION` are plain C POD structs.
    let mut si: STARTUPINFOW = unsafe { zeroed() };
    si.cb = size_of::<STARTUPINFOW>() as u32;
    let mut pi: PROCESS_INFORMATION = unsafe { zeroed() };

    // SAFETY: `primary_token` is a valid primary token and `wide_cmd` is mutable null-terminated UTF-16.
    let ok = unsafe {
        create_proc_with_token(
            primary_token.as_raw(),
            LOGON_WITH_PROFILE,
            null_wide_ptr(),
            wide_cmd.as_mut_ptr(),
            0,
            ptr::null(),
            null_wide_ptr(),
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        return None;
    }

    let pid = pi.dwProcessId;
    let _proc = OwnedHandle::from_raw(pi.hProcess);
    let _thread = OwnedHandle::from_raw(pi.hThread);
    (pid != 0).then_some(pid)
}

fn current_process_session_id() -> Option<u32> {
    let mut session_id: u32 = 0;
    // SAFETY: Queries Windows session ID of the current process.
    let ok = unsafe { ProcessIdToSessionId(std::process::id(), &mut session_id) };
    (ok != 0).then_some(session_id)
}

fn find_session_explorer_pids(target_session: Option<u32>) -> Vec<u32> {
    // SAFETY: `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)` takes no pointers.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    let Some(snapshot) = OwnedHandle::from_raw(raw) else {
        return Vec::new();
    };

    // SAFETY: `PROCESSENTRY32W` is C POD with `dwSize` initialized before calling `Process32FirstW`.
    let mut entry: PROCESSENTRY32W = unsafe { zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;

    let mut pids = Vec::new();
    // SAFETY: `snapshot` is a valid process snapshot handle.
    let mut has_next = unsafe { Process32FirstW(snapshot.as_raw(), &mut entry) };
    while has_next != 0 {
        let name = decode_wide_nul(&entry.szExeFile);
        if name.eq_ignore_ascii_case("explorer.exe") {
            let pid = entry.th32ProcessID;
            let same_session = match target_session {
                Some(expected) => {
                    let mut sid: u32 = 0;
                    // SAFETY: Queries session ID for enumerated `explorer.exe` PID.
                    unsafe { ProcessIdToSessionId(pid, &mut sid) != 0 && sid == expected }
                }
                None => true,
            };
            if same_session {
                pids.push(pid);
            }
        }
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        // SAFETY: Continues process snapshot iteration.
        has_next = unsafe { Process32NextW(snapshot.as_raw(), &mut entry) };
    }

    pids
}

fn explorer_exe_path() -> String {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".to_owned());
    PathBuf::from(windir)
        .join("explorer.exe")
        .to_string_lossy()
        .into_owned()
}

/// Splits a "Run new task" command line into `(executable_or_target, arguments)`.
pub(super) fn split_command_and_args(cmd: &str) -> (&str, &str) {
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

/// RFC 3986 percent-encoding for search query URL parameters.
pub(super) fn percent_encode_query(input: &str) -> String {
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

/// Reads `HKLM\...\AeDebug\Debugger` (falling back to `WOW6432Node`), returning `Ok(None)`
/// if no JIT debugger is configured on this machine.
fn read_aedebug_command() -> Result<Option<String>, PalError> {
    let value_name = to_wide_null("Debugger");

    for subkey_path in AEDEBUG_PATHS {
        let wide_subkey = to_wide_null(subkey_path);
        let mut raw_hkey: HKEY = ptr::null_mut();
        // SAFETY: `wide_subkey` is a valid null-terminated UTF-16 registry subkey path.
        let open_status = unsafe {
            RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                wide_subkey.as_ptr(),
                0,
                KEY_READ,
                &mut raw_hkey,
            )
        };
        let Some(hkey) = (open_status == ERROR_SUCCESS)
            .then(|| OwnedRegKey::from_raw(raw_hkey))
            .flatten()
        else {
            continue;
        };

        let mut reg_type: u32 = 0;
        let mut byte_len: u32 = 0;
        // SAFETY: Size-discovery call with null data buffer on valid `hkey`.
        let size_status = unsafe {
            RegQueryValueExW(
                hkey.as_raw(),
                value_name.as_ptr(),
                ptr::null(),
                &mut reg_type,
                ptr::null_mut(),
                &mut byte_len,
            )
        };
        if size_status != ERROR_SUCCESS
            || byte_len == 0
            || (reg_type != REG_SZ && reg_type != REG_EXPAND_SZ)
        {
            continue;
        }

        let mut buf = vec![0u16; (byte_len as usize).div_ceil(2)];
        let mut actual_bytes = (buf.len() * 2) as u32;
        // SAFETY: `buf` is sized to hold `byte_len` bytes of UTF-16 registry string data.
        let read_status = unsafe {
            RegQueryValueExW(
                hkey.as_raw(),
                value_name.as_ptr(),
                ptr::null(),
                &mut reg_type,
                buf.as_mut_ptr().cast::<u8>(),
                &mut actual_bytes,
            )
        };
        if read_status != ERROR_SUCCESS {
            continue;
        }

        let u16_len = ((actual_bytes as usize) / 2).min(buf.len());
        let raw = decode_wide_nul(&buf[..u16_len]);
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Ok(Some(trimmed.to_owned()));
        }
    }

    Ok(None)
}

/// Formats a Windows `AeDebug` debugger command template (substituting `%ld`/`%lu`/`%d`/`%u`
/// placeholders: first placeholder -> `pid`, subsequent placeholders -> `0`).
pub(super) fn format_aedebug_command(template: &str, pid: u32) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    let mut arg_index = 0usize;

    while let Some(pct_idx) = rest.find('%') {
        out.push_str(&rest[..pct_idx]);
        let after = &rest[pct_idx + 1..];

        if let Some(rem) = after.strip_prefix('%') {
            out.push('%');
            rest = rem;
            continue;
        }

        let matched = after
            .strip_prefix("ld")
            .or_else(|| after.strip_prefix("lu"))
            .or_else(|| after.strip_prefix('d'))
            .or_else(|| after.strip_prefix('u'));

        if let Some(rem) = matched {
            if arg_index == 0 {
                out.push_str(&pid.to_string());
            } else {
                out.push('0');
            }
            arg_index += 1;
            rest = rem;
        } else {
            out.push('%');
            rest = after;
        }
    }

    out.push_str(rest);
    out
}

/// Expands Windows `%ENV_VAR%` tokens using process environment variables (leaving unknown tokens intact).
fn expand_env_vars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(end) = after.find('%') {
            let var_name = &after[..end];
            if !var_name.is_empty()
                && !var_name.chars().any(char::is_whitespace)
                && let Ok(val) = std::env::var(var_name)
            {
                out.push_str(&val);
                rest = &after[end + 1..];
                continue;
            }
        }
        out.push('%');
        rest = after;
    }
    out.push_str(rest);
    out
}
