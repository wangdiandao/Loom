//! Windows file lock detection combining Restart Manager (`rstrtmgr.dll`) with an
//! elevated-only `NtQuerySystemInformation(SystemExtendedHandleInformation)` kernel
//! handle scanner.
//!
//! - Always runs Restart Manager (`rstrtmgr.dll`) with RAII session teardown and recursive
//!   binary-split batch registration for rich application/service metadata.
//! - When running elevated, supplements Restart Manager by enumerating system `File`
//!   handles (`SystemExtendedHandleInformation = 64`) to catch open directory handles,
//!   console CWD locks, background services, and non-RM processes.
//! - Strictly hang-proof: skips known blocking named-pipe `GrantedAccess` masks, filters
//!   to the dynamically discovered `"File"` `ObjectTypeIndex`, and runs handle-path probes
//!   on an isolated worker thread with per-handle timeouts (`CancelSynchronousIo`) and a
//!   bounded total scan budget.

mod handle_scan;
mod restart_manager;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use loom_pal_api::{PalError, SystemOperator};
use loom_types::FileLockHolder;

use crate::operator::WindowsSystemOperator;

use handle_scan::scan_file_locks_via_handles;
use restart_manager::find_locks_via_restart_manager;

/// Queries processes holding open file handles on `path` (single file or directory).
///
/// Always queries Restart Manager (`rstrtmgr.dll`). When Loom is running elevated, also
/// supplements Restart Manager with a hang-protected `SystemExtendedHandleInformation` kernel
/// handle scan so directory handles, CWD locks, services, and non-RM processes are covered.
pub fn find_file_locks(path: &Path) -> Result<Vec<FileLockHolder>, PalError> {
    if path.as_os_str().is_empty() {
        return Err(PalError::InvalidArgument(
            "file lock query path cannot be empty".into(),
        ));
    }

    let meta = fs::metadata(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            PalError::InvalidArgument(format!("path does not exist: {}", path.display()))
        } else {
            PalError::Io(err)
        }
    })?;

    let is_dir = meta.is_dir();
    let mut holders = find_locks_via_restart_manager(path, is_dir)?;

    if WindowsSystemOperator::new().is_elevated()
        && let Ok(handle_holders) = scan_file_locks_via_handles(path, is_dir)
    {
        merge_lock_holders(&mut holders, handle_holders);
    }

    holders.sort_by(|a, b| (a.pid, a.app_name.as_ref()).cmp(&(b.pid, b.app_name.as_ref())));
    Ok(holders)
}

/// Merges supplementary kernel-handle lock holders into `primary`, preserving Restart Manager's
/// richer metadata whenever a PID was already discovered by Restart Manager.
fn merge_lock_holders(primary: &mut Vec<FileLockHolder>, extra: Vec<FileLockHolder>) {
    let mut existing_pids: HashSet<u32> = primary.iter().map(|h| h.pid).collect();
    for holder in extra {
        if existing_pids.insert(holder.pid) {
            primary.push(holder);
        }
    }
}

/// Normalizes a Win32 final path (stripping `\\?\` or `\\?\UNC\` prefixes and trailing backslashes)
/// for case-insensitive comparison.
fn normalize_final_path(raw: &str) -> String {
    let stripped = if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        raw.to_owned()
    };
    let trimmed = stripped.trim_end_matches('\\');
    if trimmed.len() == 2 && trimmed.as_bytes()[1] == b':' {
        // Keep drive root trailing backslash (e.g., `C:\`).
        format!("{trimmed}\\")
    } else {
        trimmed.to_owned()
    }
}

/// Returns `true` if `candidate` (already normalized) locks `target` (already normalized).
///
/// For files, requires an exact case-insensitive match. For directories, matches the directory
/// itself or any descendant path inside `target`.
fn path_matches_target(candidate: &str, target: &str, target_is_dir: bool) -> bool {
    if candidate.eq_ignore_ascii_case(target) {
        return true;
    }
    if !target_is_dir || candidate.len() <= target.len() {
        return false;
    }
    let Some(prefix) = candidate.get(..target.len()) else {
        return false;
    };
    if !prefix.eq_ignore_ascii_case(target) {
        return false;
    }
    target.ends_with('\\') || candidate.as_bytes().get(target.len()) == Some(&b'\\')
}
