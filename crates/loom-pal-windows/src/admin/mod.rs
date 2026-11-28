//! Persistent UAC-free administrator startup backed by Windows Task Scheduler 2.0.
//!
//! Architecture:
//! - Enabling registers an on-demand scheduled task (`Loom.Admin.<SID>`) with
//!   `InteractiveToken`, `HighestAvailable`, `Priority=4`, and an owner-only `FRFX` SDDL
//!   (no password stored; only SYSTEM/Administrators may modify the task), alongside a
//!   grant record in `%ProgramData%\Loom\admin\<SID>\grant.toml`.
//! - Cold launches (including IFEO `--task-manager` -> `--loom-taskmgr-entry` launches) call
//!   [`route`] before single-instance locking and GPUI boot. When enabled and running
//!   non-elevated, [`route`] prepares an elevated broker via `IRegisteredTask::RunEx`, commits
//!   the three-event handshake, and exits so the elevated UI takes over seamlessly with zero
//!   UAC prompt.
//! - Toggling the option ON or OFF in Settings uses [`prepare_privilege_switch`] +
//!   [`PreparedSwitch::commit`] to seamlessly relaunch Loom elevated or de-elevated (via
//!   `standard_spawn` / Explorer parent on split-token accounts) and restore `Route::Settings`.

pub mod task;
pub mod ticket;

use std::fs::{self, OpenOptions};
use std::mem::{size_of, zeroed};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use loom_pal_api::{PalError, SystemOperator};
use loom_types::AdminStartupStatus;
use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{ERROR_CANCELLED, GetLastError, WAIT_OBJECT_0};
use windows_sys::Win32::Security::{
    TOKEN_QUERY, TokenElevationTypeFull, TokenElevationTypeLimited,
};
use windows_sys::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject,
};
use windows_sys::Win32::UI::Shell::{
    SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

use crate::handle::{OwnedHandle, map_win32_error, os_str_to_wide_null, to_wide_null};
use crate::install_guard;
use crate::operator::WindowsSystemOperator;
use crate::slow_prober::ensure_com_initialized;
use crate::taskmgr::{launch, state_directory as base_state_directory};

pub use ticket::{PreparedSwitch, acknowledge};

/// Internal CLI flag invoked by Task Scheduler (`--loom-admin-run $(Arg0)`).
pub(crate) const RUN_FLAG: &str = "--loom-admin-run";

/// Internal CLI flag for the standard-privilege de-elevation broker.
pub(crate) const STANDARD_BROKER_FLAG: &str = "--loom-admin-standard";

/// Internal CLI flag for the on-demand UAC maintenance helper (`enable` / `disable`).
const MANAGE_FLAG: &str = "--loom-admin-manage";

/// Internal CLI flag for uninstaller cleanup (removes all Loom admin tasks and grants).
const UNINSTALL_FLAG: &str = "--loom-admin-uninstall";

/// CLI flag passed to the UI child spawned by a broker so it can acknowledge `done`.
pub(crate) const STARTED_FLAG: &str = "--loom-admin-started";

/// CLI flag passed when an elevated launch failed and fell back to standard privileges.
pub const FALLBACK_FLAG: &str = "--loom-admin-fallback";

/// CLI flag instructing this launch to remain standard-user once without routing to admin.
pub const STANDARD_ONCE_FLAG: &str = "--loom-standard-once";

const GRANT_FILE: &str = "grant.toml";
const LOCK_FILE: &str = "admin.lock";
const ERROR_FILE: &str = "admin-error.txt";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrantRecord {
    pub exe_path: PathBuf,
}

pub(crate) fn canonical_exe(path: &Path) -> PathBuf {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let raw = resolved.to_string_lossy();
    PathBuf::from(raw.strip_prefix(r"\\?\").unwrap_or(&raw))
}

fn admin_root_dir() -> PathBuf {
    base_state_directory().join("admin")
}

fn account_dir(sid: &str) -> Result<PathBuf, String> {
    if !task::valid_user_sid(sid) {
        return Err("invalid user account SID".into());
    }
    Ok(admin_root_dir().join(sid))
}

pub(crate) fn load_grant(sid: &str) -> Result<Option<GrantRecord>, String> {
    let dir = account_dir(sid)?;
    let path = dir.join(GRANT_FILE);
    let data = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    if data.len() > 64 * 1024 {
        return Err("grant record exceeds maximum size".into());
    }
    let record: GrantRecord =
        toml::from_str(&data).map_err(|e| format!("corrupt grant record: {e}"))?;
    Ok(Some(record))
}

fn write_grant_atomic(dir: &Path, record: &GrantRecord) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    let text =
        toml::to_string_pretty(record).map_err(|e| format!("failed to serialize grant: {e}"))?;
    let tmp = dir.join(format!("{GRANT_FILE}.tmp.{}", std::process::id()));
    fs::write(&tmp, text.as_bytes())
        .map_err(|e| format!("failed to write {}: {e}", tmp.display()))?;
    let target = dir.join(GRANT_FILE);
    let wide_from = to_wide_null(&tmp.to_string_lossy());
    let wide_to = to_wide_null(&target.to_string_lossy());
    // SAFETY: Atomically replaces `grant.toml` with write-through durability.
    let ok = unsafe {
        MoveFileExW(
            wide_from.as_ptr(),
            wide_to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        let code = unsafe { GetLastError() };
        let _ = fs::remove_file(&tmp);
        return Err(format!("MoveFileExW(grant.toml) failed (os error {code})"));
    }
    Ok(())
}

fn acquire_lock(dir: &Path) -> Result<fs::File, String> {
    fs::create_dir_all(dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(dir.join(LOCK_FILE))
        .map_err(|e| format!("another administrator startup operation is in progress: {e}"))
}

pub(crate) fn is_elevated() -> bool {
    WindowsSystemOperator::new().is_elevated()
}

fn own_elevation_type() -> Result<i32, String> {
    let mut raw: windows_sys::Win32::Foundation::HANDLE = std::ptr::null_mut();
    // SAFETY: Opens current process token for `TOKEN_QUERY`.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
        let code = unsafe { GetLastError() };
        return Err(format!("OpenProcessToken failed (os error {code})"));
    }
    let token =
        OwnedHandle::from_raw(raw).ok_or_else(|| "OpenProcessToken returned null".to_owned())?;
    launch::token_elevation_type(token.as_raw())
}

/// Returns `true` if the current account is an administrator (already elevated or split-token).
pub fn is_eligible() -> bool {
    if is_elevated() {
        return true;
    }
    own_elevation_type().is_ok_and(|kind| kind == TokenElevationTypeLimited)
}

/// Returns `true` if the current process holds an elevated split token (`TokenElevationTypeFull`)
/// and can cleanly de-elevate back to standard privileges via Explorer parent inheritance.
pub fn can_drop_elevation() -> bool {
    own_elevation_type().is_ok_and(|kind| kind == TokenElevationTypeFull)
}

/// Fast read-only status query for persistent administrator startup.
///
/// Skips connecting to Task Scheduler completely when this account has never created a grant
/// folder, ensuring zero COM/IPC overhead on normal launches.
pub fn status() -> Result<AdminStartupStatus, PalError> {
    let elevated = is_elevated();
    let eligible = is_eligible();
    let can_drop = can_drop_elevation();
    let unprotected_path = std::env::current_exe()
        .map(|p| install_guard::verify_protected_executable(&p).is_err())
        .unwrap_or(true);
    let mut result = AdminStartupStatus {
        supported: true,
        enabled: false,
        ready: false,
        elevated,
        eligible,
        can_drop_elevation: can_drop,
        error: None,
        unprotected_path,
    };

    let check = (|| -> Result<(), String> {
        let sid = launch::current_user_sid()?;
        let dir = account_dir(&sid)?;
        if !dir.exists() {
            return Ok(());
        }
        let grant_exists = dir.join(GRANT_FILE).is_file();
        let scheduler = match task::Scheduler::connect() {
            Ok(s) => s,
            Err(_) if !grant_exists => return Ok(()),
            Err(err) => return Err(err),
        };
        let task_exists = match scheduler.exists(&sid) {
            Ok(exists) => exists,
            Err(_) if !grant_exists => return Ok(()),
            Err(err) => return Err(err),
        };
        result.enabled = task_exists;
        if !task_exists {
            return Ok(());
        }

        let Some(grant) = load_grant(&sid)? else {
            return Err("scheduled task exists without a matching grant record".into());
        };
        let current_exe = canonical_exe(&std::env::current_exe().map_err(|e| e.to_string())?);
        if !grant
            .exe_path
            .to_string_lossy()
            .eq_ignore_ascii_case(&current_exe.to_string_lossy())
        {
            return Err(
                "registered administrator task points to a different executable path".into(),
            );
        }
        scheduler.validate(&sid, &current_exe)?;
        result.ready = true;
        Ok(())
    })();

    if let Err(err) = check {
        result.error = Some(err);
    }
    Ok(result)
}

/// Enables or disables persistent administrator startup for the current user account.
///
/// When called from a standard-user session, automatically invokes a short-lived elevated
/// maintenance helper via `ShellExecuteExW("runas")`, returning [`PalError::Cancelled`] if
/// the user dismisses the UAC prompt.
pub fn set_enabled(enabled: bool) -> Result<AdminStartupStatus, PalError> {
    if enabled && !is_eligible() {
        return Err(PalError::OsError {
            code: 5,
            message: "current Windows account is not a member of the Administrators group".into(),
        });
    }

    let sid = launch::current_user_sid().map_err(|msg| PalError::OsError {
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

    if is_elevated() {
        apply_elevated(&sid, &exe, enabled).map_err(|msg| PalError::OsError {
            code: -1,
            message: msg,
        })?;
        return status();
    }

    let action = if enabled { "enable" } else { "disable" };
    run_elevated_helper(&exe, &[MANAGE_FLAG.to_owned(), action.to_owned(), sid])?;
    status()
}

fn apply_elevated(sid: &str, exe: &Path, enabled: bool) -> Result<(), String> {
    if !is_elevated() {
        return Err("administrator privileges are required".into());
    }
    if !task::valid_user_sid(sid) {
        return Err("invalid user account SID".into());
    }
    // Establish Task Scheduler COM security before filesystem/shell work.
    let scheduler = task::Scheduler::connect()?;
    let dir = account_dir(sid)?;

    if !enabled {
        let _lock = if dir.exists() {
            acquire_lock(&dir).ok()
        } else {
            None
        };
        scheduler.remove(sid)?;
        let _ = fs::remove_file(dir.join(GRANT_FILE));
        let _ = fs::remove_file(dir.join(ERROR_FILE));
        return Ok(());
    }

    if enabled {
        install_guard::verify_protected_executable(exe).map_err(|r| r.to_string())?;
    }
    let _lock = acquire_lock(&dir)?;
    let record = GrantRecord {
        exe_path: exe.to_path_buf(),
    };
    write_grant_atomic(&dir, &record)?;
    if let Err(err) = scheduler.register(sid, exe) {
        let _ = fs::remove_file(dir.join(GRANT_FILE));
        return Err(err);
    }
    scheduler.validate(sid, exe)?;
    let _ = fs::remove_file(dir.join(ERROR_FILE));
    Ok(())
}

fn run_elevated_helper(exe: &Path, args: &[String]) -> Result<(), PalError> {
    ensure_com_initialized();

    let dir = base_state_directory();
    let _ = fs::create_dir_all(&dir);
    let err_path = dir.join(ERROR_FILE);
    let _ = fs::remove_file(&err_path);

    let params = args
        .iter()
        .map(|a| launch::quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ");

    let wide_exe = os_str_to_wide_null(exe.as_os_str());
    let wide_verb = to_wide_null("runas");
    let wide_params = to_wide_null(&params);

    // SAFETY: `SHELLEXECUTEINFOW` is POD; all wide buffers stay alive across the call.
    let mut sei: SHELLEXECUTEINFOW = unsafe { zeroed() };
    sei.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    sei.lpVerb = wide_verb.as_ptr();
    sei.lpFile = wide_exe.as_ptr();
    sei.lpParameters = wide_params.as_ptr();
    sei.nShow = SW_HIDE;

    // SAFETY: Prompts UAC once to run the short-lived elevated maintenance helper.
    let ok = unsafe { ShellExecuteExW(&mut sei) };
    if ok == 0 {
        let code = unsafe { GetLastError() };
        if code == ERROR_CANCELLED {
            return Err(PalError::Cancelled);
        }
        return Err(map_win32_error(code, None, "ShellExecuteExW(admin-manage)"));
    }

    let child = OwnedHandle::from_raw(sei.hProcess).ok_or_else(|| PalError::OsError {
        code: -1,
        message: "elevated maintenance helper did not return a process handle".into(),
    })?;

    // Wait up to 60 s for the maintenance helper to register/remove the task.
    if unsafe { WaitForSingleObject(child.as_raw(), 60_000) } != WAIT_OBJECT_0 {
        return Err(PalError::TimedOut(std::time::Duration::from_secs(60)));
    }

    let mut exit_code: u32 = 1;
    unsafe {
        let _ = GetExitCodeProcess(child.as_raw(), &mut exit_code);
    }
    if exit_code != 0 {
        let detail = fs::read_to_string(&err_path)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                format!("administrator startup helper failed with exit code {exit_code}")
            });
        let _ = fs::remove_file(&err_path);
        return Err(PalError::OsError {
            code: exit_code as i32,
            message: detail,
        });
    }

    let _ = fs::remove_file(&err_path);
    Ok(())
}

/// Early cold-start routing called BEFORE single-instance locking and GPUI initialization.
///
/// If persistent administrator startup is enabled for the current account and this process is
/// running non-elevated (without `--loom-admin-fallback` or `--loom-standard-once`), prepares
/// the scheduled-task elevated broker, commits the three-event ticket, and returns `true` so
/// the caller exits immediately and lets the elevated UI take over.
///
/// If anything fails (e.g., task deleted, binary moved, scheduler unavailable), returns `false`
/// so Loom falls back seamlessly to starting with standard privileges.
pub fn route(raw_args: &[String]) -> bool {
    if is_elevated()
        || raw_args
            .iter()
            .any(|a| a == FALLBACK_FLAG || a == STANDARD_ONCE_FLAG)
    {
        return false;
    }

    let Ok(sid) = launch::current_user_sid() else {
        return false;
    };
    let Ok(Some(grant)) = load_grant(&sid) else {
        return false;
    };
    let Ok(current_exe) = std::env::current_exe().map(|p| canonical_exe(&p)) else {
        return false;
    };
    if !grant
        .exe_path
        .to_string_lossy()
        .eq_ignore_ascii_case(&current_exe.to_string_lossy())
    {
        return false;
    }

    match ticket::prepare(raw_args, true).and_then(PreparedSwitch::commit) {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(%err, "persistent administrator startup route failed; continuing as standard user");
            false
        }
    }
}

/// Prepares an interactive privilege transition (`elevated = true` to elevate via Task Scheduler,
/// `elevated = false` to drop elevation via `standard_spawn`) restoring `restore_route`.
pub fn prepare_privilege_switch(
    restore_route: &str,
    elevated: bool,
) -> Result<PreparedSwitch, PalError> {
    let args = vec![format!("--restore-route={restore_route}")];
    ticket::prepare(&args, elevated).map_err(|msg| PalError::OsError {
        code: -1,
        message: msg,
    })
}

/// Handles early CLI modes for persistent administrator startup BEFORE GPUI/engine boot.
///
/// Returns `Some(exit_code)` if a special mode ran and the process must exit immediately.
pub fn try_handle_cli_mode(args: &[String]) -> Option<i32> {
    match args.first().map(String::as_str) {
        Some(RUN_FLAG) => Some(match args.get(1) {
            Some(token) => match ticket::run_broker(token, true) {
                Ok(()) => 0,
                Err(_) => 1,
            },
            None => 87,
        }),
        Some(STANDARD_BROKER_FLAG) => Some(match args.get(1) {
            Some(token) => match ticket::run_broker(token, false) {
                Ok(()) => 0,
                Err(_) => 1,
            },
            None => 87,
        }),
        Some(MANAGE_FLAG) => Some(run_manage_helper(args)),
        Some(UNINSTALL_FLAG) => Some(run_uninstall_cleanup()),
        _ => None,
    }
}

fn run_manage_helper(args: &[String]) -> i32 {
    let result = (|| -> Result<(), String> {
        let [_, action, sid] = args else {
            return Err("invalid administrator maintenance helper arguments".into());
        };
        let enabled = match action.as_str() {
            "enable" => true,
            "disable" => false,
            _ => return Err(format!("unknown administrator action: {action}")),
        };
        let exe = canonical_exe(&std::env::current_exe().map_err(|e| e.to_string())?);
        apply_elevated(sid, &exe, enabled)
    })();

    match result {
        Ok(()) => 0,
        Err(err) => {
            let dir = base_state_directory();
            let _ = fs::create_dir_all(&dir);
            let _ = fs::write(dir.join(ERROR_FILE), err.as_bytes());
            1
        }
    }
}

fn run_uninstall_cleanup() -> i32 {
    if !is_elevated() {
        return 5;
    }
    let result = (|| -> Result<(), String> {
        let scheduler = task::Scheduler::connect()?;
        scheduler.remove_all()?;
        let root = admin_root_dir();
        if root.exists() {
            let _ = fs::remove_dir_all(&root);
        }
        Ok(())
    })();
    match result {
        Ok(()) => 0,
        Err(_) => 1,
    }
}
