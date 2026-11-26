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
use windows_sys::Win32::Foundation::{GetLastError, HANDLE};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess,
    GetProcessAffinityMask, GetProcessId, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS,
    NORMAL_PRIORITY_CLASS, OpenProcessToken, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, PROCESS_TERMINATE,
    ProcessPowerThrottling, QueryFullProcessImageNameW, REALTIME_PRIORITY_CLASS, SetPriorityClass,
    SetProcessAffinityMask, SetProcessInformation, TerminateProcess,
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
        if proc_mask == 0 && logical_cpus > 64 {
            // Multi-group process on >64 CPU hardware: default to reporting all active CPUs.
            return Ok(CpuAffinityMask::all(logical_cpus));
        }
        if let Some(first) = mask.groups.first_mut() {
            *first = proc_mask as u64;
        }
        Ok(mask)
    }

    fn set_affinity(&self, key: ProcessKey, mask: &CpuAffinityMask) -> Result<(), PalError> {
        if mask.is_empty() {
            return Err(PalError::InvalidArgument(
                "CPU affinity mask must enable at least one processor".into(),
            ));
        }

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
        let filter: Vec<u16> =
            "Programs (*.exe;*.bat;*.cmd;*.msc)\0*.exe;*.bat;*.cmd;*.msc\0All Files (*.*)\0*.*\0\0"
                .encode_utf16()
                .collect();
        let mut file_buf = vec![0u16; 32_768];

        // SAFETY: `OPENFILENAMEW` is POD.
        let mut ofn: OPENFILENAMEW = unsafe { zeroed() };
        ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
        ofn.lpstrFilter = filter.as_ptr();
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
}
