//! Windows IFEO system Task Manager replacement (`taskmgr.exe`).
//!
//! Implements a safe, opt-in IFEO replacement chain:
//! - Read-only status queries with conflict detection (refuses existing `Debugger`, `UseFilter`,
//!   or any IFEO subkey such as `PerfOptions`).
//! - WAL recovery journal (`%ProgramData%\Loom\taskmgr-recovery.toml`) tracking
//!   `Prepared -> Active -> Restoring -> Restored` alongside human-readable recovery instructions.
//! - Pre-enable dual self-test probes (normal launch & `DEBUG_ONLY_THIS_PROCESS` detach launch).
//! - On-demand UAC maintenance helper (`--loom-taskmgr-manage`) so standard-user Settings can
//!   toggle replacement with a single UAC prompt without restarting the main window.
//! - `--task-manager` IFEO entry: checks `autoElevate` native relaunch marker, validates owner
//!   SID, spawns/wakes Loom with standard privileges + three-event readiness handshake +
//!   foreground pipe, and automatically falls back to native `taskmgr.exe` on any error.

pub mod launch;
pub mod ready;
pub mod registry;
pub mod state;

use std::fs::{self, OpenOptions};
use std::mem::{size_of, zeroed};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use loom_pal_api::{PalError, SystemOperator};
use loom_types::{TaskManagerReplacementState, TaskManagerReplacementStatus};
use windows_sys::Win32::Foundation::{ERROR_CANCELLED, GetLastError, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{
    SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

use crate::handle::{OwnedHandle, map_win32_error, os_str_to_wide_null, to_wide_null};
use crate::install_guard;
use crate::operator::WindowsSystemOperator;
use crate::slow_prober::ensure_com_initialized;

use self::registry::{JOURNAL_FILE, MachineStore, RECOVERY_TEXT_FILE, TASKMGR_IFEO_KEY};
use self::state::{Phase, RegValue};

/// CLI flag registered in IFEO `Debugger` (`"<loom-app.exe>" --task-manager`).
pub const IFEO_FLAG: &str = "--task-manager";

/// Internal CLI flag for the elevated maintenance helper (`enable` / `restore`).
const MANAGE_FLAG: &str = "--loom-taskmgr-manage";

/// Internal CLI flag for the pre-enable self-test probe child (exits 0 immediately).
const PROBE_FLAG: &str = "--loom-taskmgr-probe";

/// Internal CLI flag for uninstaller cleanup (restores IFEO if owned by Loom).
const UNINSTALL_FLAG: &str = "--loom-taskmgr-uninstall";

const LOCK_FILE: &str = "taskmgr.lock";
const ERROR_FILE: &str = "taskmgr-error.txt";

/// Returns the machine-wide state directory (`%ProgramData%\Loom`).
pub fn state_directory() -> PathBuf {
    if let Ok(program_data) = std::env::var("ProgramData")
        && !program_data.trim().is_empty()
    {
        return PathBuf::from(program_data).join("Loom");
    }
    PathBuf::from(r"C:\ProgramData\Loom")
}

/// Canonical Win32 executable path without `\\?\` prefix.
fn canonical_exe(path: &Path) -> PathBuf {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let raw = resolved.to_string_lossy();
    PathBuf::from(raw.strip_prefix(r"\\?\").unwrap_or(&raw))
}

/// Builds the exact `REG_SZ` `Debugger` command written by Loom for `exe`.
pub fn replacement_value(exe: &Path) -> RegValue {
    let clean = canonical_exe(exe);
    RegValue::reg_sz(&format!(
        "{} {IFEO_FLAG}",
        launch::quote_arg(&clean.to_string_lossy())
    ))
}

fn recovery_instructions(exe: &Path) -> String {
    let cmd = replacement_value(exe)
        .as_text()
        .unwrap_or_else(|| format!("\"{}\" {IFEO_FLAG}", exe.display()));
    format!(
        "Loom Task Manager Replacement - Emergency Recovery\r\n\
         ==================================================\r\n\r\n\
         Registry Key:\r\n\
           HKEY_LOCAL_MACHINE\\{TASKMGR_IFEO_KEY}\r\n\r\n\
         Expected Loom Debugger Value (REG_SZ):\r\n\
           {cmd}\r\n\r\n\
         To restore the built-in Windows Task Manager:\r\n\
         1. Open Loom Settings -> Privileges -> disable 'Replace Windows Task Manager', OR\r\n\
         2. Run from an elevated prompt:\r\n\
              \"{}\" {UNINSTALL_FLAG}\r\n\
         3. Manual fallback (only if Debugger matches Loom above):\r\n\
              Open regedit as Administrator, navigate to the key above, verify Debugger matches\r\n\
              Loom's command, and delete ONLY the 'Debugger' value.\r\n",
        exe.display()
    )
}

fn acquire_lock(dir: &Path) -> Result<fs::File, String> {
    fs::create_dir_all(dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(dir.join(LOCK_FILE))
        .map_err(|e| format!("another Task Manager replacement operation is in progress: {e}"))
}

/// Read-only status inspection of IFEO `taskmgr.exe` and Loom's recovery journal.
pub fn status() -> Result<TaskManagerReplacementStatus, PalError> {
    let dir = state_directory();
    let store = MachineStore::new(dir.clone());
    let current_exe = std::env::current_exe().map_err(PalError::Io)?;
    let unprotected_path = install_guard::verify_protected_executable(&current_exe).is_err();
    let ours = replacement_value(&current_exe);

    let live = registry::observe_hklm(TASKMGR_IFEO_KEY).map_err(|msg| PalError::OsError {
        code: -1,
        message: msg,
    })?;

    let journal = match store.load_journal() {
        Ok(j) => j,
        Err(err) => {
            tracing::warn!(%err, "corrupt taskmgr recovery journal");
            // Journal is damaged; still classify cleanly based on live registry value.
            let is_ours = !live.filtered && live.debugger.as_ref() == Some(&ours);
            let state = if is_ours {
                TaskManagerReplacementState::RecoveryNeeded
            } else if live.debugger.is_some() || live.filtered {
                TaskManagerReplacementState::Conflict
            } else {
                TaskManagerReplacementState::RecoveryNeeded
            };
            let conflict_detail = if state == TaskManagerReplacementState::Conflict {
                registry::describe_hklm(TASKMGR_IFEO_KEY)
                    .ok()
                    .and_then(|r| r.describe(&ours))
            } else {
                Some(err)
            };
            return Ok(TaskManagerReplacementStatus {
                state,
                can_enable: false,
                can_restore: state == TaskManagerReplacementState::RecoveryNeeded,
                conflict_detail,
                recovery_path: Some(dir.join(RECOVERY_TEXT_FILE).display().to_string()),
                unprotected_path,
            });
        }
    };

    let expected_val = journal.as_ref().map(|j| &j.replacement).unwrap_or(&ours);
    let state = state::classify_state(journal.as_ref(), &live);
    let conflict_detail = if state == TaskManagerReplacementState::Conflict {
        registry::describe_hklm(TASKMGR_IFEO_KEY)
            .ok()
            .and_then(|report| report.describe(expected_val))
    } else {
        None
    };

    let (can_enable, can_restore) = match state {
        TaskManagerReplacementState::Disabled => (!unprotected_path, false),
        TaskManagerReplacementState::Enabled => (false, true),
        TaskManagerReplacementState::Revoked => (!unprotected_path, true),
        TaskManagerReplacementState::RecoveryNeeded => (false, true),
        TaskManagerReplacementState::Conflict | TaskManagerReplacementState::Unsupported => {
            (false, false)
        }
    };

    let recovery_path =
        if dir.join(RECOVERY_TEXT_FILE).is_file() || dir.join(JOURNAL_FILE).is_file() {
            Some(dir.join(RECOVERY_TEXT_FILE).display().to_string())
        } else {
            None
        };

    Ok(TaskManagerReplacementStatus {
        state,
        can_enable,
        can_restore,
        conflict_detail,
        recovery_path,
        unprotected_path,
    })
}

/// Enables or restores Task Manager replacement, invoking an elevated maintenance helper via
/// UAC (`ShellExecuteExW("runas")`) automatically when called from a standard-user process.
pub fn set_replacement(enabled: bool) -> Result<TaskManagerReplacementStatus, PalError> {
    let owner_sid = launch::current_user_sid().map_err(|msg| PalError::OsError {
        code: -1,
        message: msg,
    })?;
    let exe = canonical_exe(&std::env::current_exe().map_err(PalError::Io)?);
    if enabled && let Err(reason) = install_guard::verify_protected_executable(&exe) {
        return Err(PalError::OsError {
            code: 5,
            message: reason.to_string(),
        });
    }

    if WindowsSystemOperator::new().is_elevated() {
        let res = if enabled {
            enable_elevated(&exe, &owner_sid)
        } else {
            restore_elevated(&exe)
        };
        res.map_err(|msg| PalError::OsError {
            code: -1,
            message: msg,
        })?;
        return status();
    }

    let action = if enabled { "enable" } else { "restore" };
    run_elevated_helper(
        &exe,
        &[MANAGE_FLAG.to_owned(), action.to_owned(), owner_sid],
    )?;
    status()
}

/// Launches the built-in Windows `%SystemRoot%\System32\taskmgr.exe` directly past IFEO.
///
/// Spawns a short-lived background waiter thread when running under a limited split token
/// so the `autoElevate` handoff marker remains alive while the UI returns immediately.
pub fn open_native() -> Result<(), PalError> {
    let native = launch::launch_native_task_manager().map_err(|msg| PalError::OsError {
        code: -1,
        message: msg,
    })?;
    let _ = std::thread::Builder::new()
        .name("loom-native-taskmgr-wait".into())
        .spawn(move || native.wait());
    Ok(())
}

/// Handles early CLI maintenance / IFEO launcher modes BEFORE GPUI or engine initialization.
///
/// Returns `Some(exit_code)` when a special CLI mode was executed and the process should exit
/// immediately; returns `None` for normal interactive desktop startup.
pub fn try_handle_cli_mode(args: &[String]) -> Option<i32> {
    match args.first().map(String::as_str) {
        Some(PROBE_FLAG) => Some(0),
        Some(IFEO_FLAG) => Some(run_ifeo_launcher()),
        Some(UNINSTALL_FLAG) => Some(run_uninstall_restore()),
        Some(MANAGE_FLAG) => Some(run_manage_helper(args)),
        _ => None,
    }
}

fn run_ifeo_launcher() -> i32 {
    // 1. Check whether this elevated IFEO invocation is the `autoElevate` relaunch requested
    //    by a native `taskmgr.exe` that Loom itself just launched.
    if launch::is_relaunched_native_task_manager() {
        return match launch::launch_native_task_manager() {
            Ok(native) => {
                native.wait();
                0
            }
            Err(_) => 1,
        };
    }

    // 2. Attempt to launch or wake Loom for the authorized user with readiness verification.
    let activated = (|| -> Result<(), String> {
        let store = MachineStore::new(state_directory());
        let journal = store
            .load_journal()?
            .ok_or_else(|| "Task Manager replacement journal is missing".to_owned())?;
        if journal.phase != Phase::Active {
            return Err("Task Manager replacement journal is not active".into());
        }
        let exe = canonical_exe(&std::env::current_exe().map_err(|e| e.to_string())?);
        ready::activate(&exe, &journal.owner_sid)
    })();

    match activated {
        Ok(()) => 0,
        Err(err) => {
            tracing::warn!(%err, "IFEO launcher could not activate Loom; falling back to native taskmgr.exe");
            match launch::launch_native_task_manager() {
                Ok(native) => {
                    native.wait();
                    0
                }
                Err(_) => 1,
            }
        }
    }
}

fn run_uninstall_restore() -> i32 {
    let Ok(exe) = std::env::current_exe() else {
        return 1;
    };
    let exe = canonical_exe(&exe);
    match restore_elevated(&exe) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

fn run_manage_helper(args: &[String]) -> i32 {
    let dir = state_directory();
    let _ = fs::remove_file(dir.join(ERROR_FILE));
    let result = (|| -> Result<(), String> {
        let exe = canonical_exe(&std::env::current_exe().map_err(|e| e.to_string())?);
        match args.get(1).map(String::as_str) {
            Some("enable") => {
                let owner_sid = args
                    .get(2)
                    .filter(|s| s.starts_with("S-1-"))
                    .ok_or_else(|| "invalid owner SID argument".to_owned())?;
                enable_elevated(&exe, owner_sid)
            }
            Some("restore") => restore_elevated(&exe),
            _ => Err("invalid maintenance action".into()),
        }
    })();

    match result {
        Ok(()) => 0,
        Err(err) => {
            let _ = registry::write_atomic_file(&dir, ERROR_FILE, err.as_bytes());
            1
        }
    }
}

fn run_self_test_probes(exe: &Path) -> Result<(), String> {
    // 1. Normal spawn of `--loom-taskmgr-probe` must exit 0 cleanly.
    let normal = launch::spawn(exe, &[PROBE_FLAG.to_owned()], false)
        .map_err(|e| format!("normal self-test probe failed to start: {e}"))?;
    let code = launch::wait_exit_code(&normal, Duration::from_secs(10))
        .map_err(|e| format!("normal self-test probe failed: {e}"))?;
    if code != 0 {
        return Err(format!("normal self-test probe exited with code {code}"));
    }

    // 2. Debug-bypass spawn (`DEBUG_ONLY_THIS_PROCESS | CREATE_SUSPENDED` -> detach -> resume)
    //    must also exit 0 cleanly so native fallback is verified before writing IFEO.
    let bypass = launch::spawn(exe, &[PROBE_FLAG.to_owned()], true)
        .map_err(|e| format!("debug-detach self-test probe failed to start: {e}"))?;
    let code = launch::wait_exit_code(&bypass, Duration::from_secs(10))
        .map_err(|e| format!("debug-detach self-test probe failed: {e}"))?;
    if code != 0 {
        return Err(format!(
            "debug-detach self-test probe exited with code {code}"
        ));
    }

    Ok(())
}

fn enable_elevated(exe: &Path, owner_sid: &str) -> Result<(), String> {
    if !WindowsSystemOperator::new().is_elevated() {
        return Err(
            "administrator privileges are required to enable Task Manager replacement".into(),
        );
    }
    install_guard::verify_protected_executable(exe).map_err(|r| r.to_string())?;
    let dir = state_directory();
    let _lock = acquire_lock(&dir)?;
    let mut store = MachineStore::new(dir.clone());

    // Settle any previous unfinished or revoked journal cleanly first.
    if let Ok(Some(mut prev)) = store.load_journal() {
        state::settle(&mut store, &mut prev)?;
    }

    // Verify no conflicting IFEO configuration exists before running probes.
    let live = registry::observe_hklm(TASKMGR_IFEO_KEY)?;
    if live.debugger.is_some() || live.filtered {
        let ours = replacement_value(exe);
        let detail = registry::describe_hklm(TASKMGR_IFEO_KEY)
            .ok()
            .and_then(|r| r.describe(&ours))
            .unwrap_or_else(|| "existing IFEO configuration".to_owned());
        return Err(format!(
            "cannot enable replacement while another configuration is present: {detail}"
        ));
    }

    // Self-test both normal and IFEO-bypass debug launches before touching HKLM.
    run_self_test_probes(exe)?;

    // Write human-readable emergency recovery instructions beside the journal.
    registry::write_atomic_file(
        &dir,
        RECOVERY_TEXT_FILE,
        recovery_instructions(exe).as_bytes(),
    )?;

    let replacement = replacement_value(exe);
    state::enable(
        &mut store,
        owner_sid.to_owned(),
        exe.to_string_lossy().into_owned(),
        replacement,
    )?;
    Ok(())
}

fn restore_elevated(exe: &Path) -> Result<(), String> {
    if !WindowsSystemOperator::new().is_elevated() {
        return Err("administrator privileges are required to restore Windows Task Manager".into());
    }
    let dir = state_directory();
    let _lock = acquire_lock(&dir)?;
    let mut store = MachineStore::new(dir.clone());

    match store.load_journal() {
        Ok(Some(mut journal)) => state::restore(&mut store, &mut journal),
        Ok(None) => {
            // No journal present: strictly remove ONLY if Debugger matches Loom's exact command.
            registry::remove_owned_hklm(TASKMGR_IFEO_KEY, &replacement_value(exe))
        }
        Err(_) => {
            // Journal corrupted: strictly remove ONLY if Debugger matches Loom's exact command,
            // then remove the corrupt journal file so status returns cleanly to Disabled.
            registry::remove_owned_hklm(TASKMGR_IFEO_KEY, &replacement_value(exe))?;
            let _ = fs::remove_file(dir.join(JOURNAL_FILE));
            Ok(())
        }
    }
}

fn run_elevated_helper(exe: &Path, args: &[String]) -> Result<(), PalError> {
    ensure_com_initialized();
    let started = SystemTime::now();
    let wide_exe = os_str_to_wide_null(exe.as_os_str());
    let wide_verb = to_wide_null("runas");
    let params = args
        .iter()
        .map(|a| launch::quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ");
    let wide_params = to_wide_null(&params);
    let sys_dir = launch::system_directory().map_err(|msg| PalError::OsError {
        code: -1,
        message: msg,
    })?;
    let wide_dir = os_str_to_wide_null(sys_dir.as_os_str());

    // SAFETY: `SHELLEXECUTEINFOW` is POD; all pointed UTF-16 buffers outlive the call.
    let mut sei: SHELLEXECUTEINFOW = unsafe { zeroed() };
    sei.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    sei.lpVerb = wide_verb.as_ptr();
    sei.lpFile = wide_exe.as_ptr();
    sei.lpParameters = wide_params.as_ptr();
    sei.lpDirectory = wide_dir.as_ptr();
    sei.nShow = SW_HIDE;

    // SAFETY: All pointers in `sei` are valid NUL-terminated UTF-16 strings.
    let ok = unsafe { ShellExecuteExW(&mut sei) };
    if ok == 0 {
        let code = unsafe { GetLastError() };
        if code == ERROR_CANCELLED {
            return Err(PalError::Cancelled);
        }
        return Err(map_win32_error(code, None, "ShellExecuteExW(taskmgr)"));
    }

    let proc_handle = OwnedHandle::from_raw(sei.hProcess).ok_or_else(|| PalError::OsError {
        code: -1,
        message: "elevated maintenance helper did not return a process handle".into(),
    })?;

    if unsafe { WaitForSingleObject(proc_handle.as_raw(), 120_000) } != WAIT_OBJECT_0 {
        return Err(PalError::TimedOut(Duration::from_secs(120)));
    }

    let mut exit_code: u32 = 1;
    if unsafe { GetExitCodeProcess(proc_handle.as_raw(), &mut exit_code) } == 0 {
        let code = unsafe { GetLastError() };
        return Err(map_win32_error(code, None, "GetExitCodeProcess(taskmgr)"));
    }

    if exit_code == 0 {
        return Ok(());
    }

    let detail = read_helper_error(started)
        .unwrap_or_else(|| format!("elevated maintenance helper failed (exit code {exit_code})"));
    Err(PalError::OsError {
        code: exit_code as i32,
        message: detail,
    })
}

fn read_helper_error(since: SystemTime) -> Option<String> {
    let path = state_directory().join(ERROR_FILE);
    let meta = fs::metadata(&path).ok()?;
    if meta.len() > 8 * 1024 {
        return None;
    }
    let modified = meta.modified().ok()?;
    if modified + Duration::from_secs(2) < since {
        return None;
    }
    let text = fs::read_to_string(&path).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}
