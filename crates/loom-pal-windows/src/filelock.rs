//! Windows file lock detection via dynamically loaded Restart Manager (`rstrtmgr.dll`).
//!
//! Supports both single files and directories (with bounded file expansion and recursive
//! binary-split batch registration so a single unreadable path never fails an entire query)
//! with RAII session teardown guaranteeing `RmEndSession` always runs.

use std::collections::HashSet;
use std::ffi::c_void;
use std::fs;
use std::mem::{transmute, zeroed};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;

use loom_pal_api::PalError;
use loom_types::FileLockHolder;
use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS, FILETIME};
use windows_sys::core::BOOL;

use crate::diagnostics::OwnedModule;
use crate::handle::{map_win32_error, os_str_to_wide_null};

/// Length of `RM_SESSION_KEY` in UTF-16 code units excluding the null terminator (`CCH_RM_SESSION_KEY`).
const CCH_RM_SESSION_KEY: usize = 32;
/// `CCH_RM_MAX_APP_NAME` (`RestartManager.h`).
const CCH_RM_MAX_APP_NAME: usize = 255;
/// `CCH_RM_MAX_SVC_NAME` (`RestartManager.h`).
const CCH_RM_MAX_SVC_NAME: usize = 63;

/// Maximum number of files collected when scanning a directory for locks.
const MAX_DIR_FILES: usize = 256;
/// Maximum directory recursion depth to avoid runaway traversals.
const MAX_DIR_DEPTH: usize = 8;
/// Initial chunk size passed to `RmRegisterResources`.
const REGISTER_BATCH_SIZE: usize = 64;
/// Maximum retries for `RmGetList` on `ERROR_MORE_DATA`.
const MAX_RM_GET_LIST_RETRIES: usize = 4;

/// `RM_UNIQUE_PROCESS` (`RestartManager.h`, 12 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
struct RmUniqueProcess {
    dw_process_id: u32,
    process_start_time: FILETIME,
}

/// `RM_PROCESS_INFO` (`RestartManager.h`, 668 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
struct RmProcessInfo {
    process: RmUniqueProcess,
    str_app_name: [u16; CCH_RM_MAX_APP_NAME + 1],
    str_service_short_name: [u16; CCH_RM_MAX_SVC_NAME + 1],
    application_type: i32,
    app_status: u32,
    ts_session_id: u32,
    b_restartable: BOOL,
}

impl Default for RmProcessInfo {
    fn default() -> Self {
        // SAFETY: `RmProcessInfo` is plain C POD data.
        unsafe { zeroed() }
    }
}

type RmStartSessionFn = unsafe extern "system" fn(
    p_session_handle: *mut u32,
    dw_session_flags: u32,
    str_session_key: *mut u16,
) -> u32;

type RmRegisterResourcesFn = unsafe extern "system" fn(
    dw_session_handle: u32,
    n_files: u32,
    rgs_file_names: *const *const u16,
    n_applications: u32,
    rg_applications: *const c_void,
    n_services: u32,
    rgs_service_names: *const *const u16,
) -> u32;

type RmGetListFn = unsafe extern "system" fn(
    dw_session_handle: u32,
    pn_proc_info_needed: *mut u32,
    pn_proc_info: *mut u32,
    rg_affected_apps: *mut RmProcessInfo,
    lpdw_reboot_reasons: *mut u32,
) -> u32;

type RmEndSessionFn = unsafe extern "system" fn(dw_session_handle: u32) -> u32;

/// RAII guard ensuring `RmEndSession` is always called for an opened Restart Manager session.
struct RmSessionGuard {
    handle: u32,
    end_session: RmEndSessionFn,
}

impl Drop for RmSessionGuard {
    fn drop(&mut self) {
        // SAFETY: `self.handle` was returned by a successful `RmStartSession`.
        unsafe {
            (self.end_session)(self.handle);
        }
    }
}

/// Queries processes holding open file handles on `path` (single file or directory).
///
/// For directories, collects up to 256 regular files (depth capped at 8) and registers
/// them using a recursive binary-split fallback so a single inaccessible file never aborts
/// the entire lock scan.
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

    let files: Vec<PathBuf> = if meta.is_dir() {
        collect_directory_files(path, MAX_DIR_FILES, MAX_DIR_DEPTH)
    } else {
        vec![path.to_path_buf()]
    };

    if files.is_empty() {
        return Ok(Vec::new());
    }

    let rstrtmgr = OwnedModule::load("rstrtmgr.dll")?;
    let start_addr = rstrtmgr
        .proc_addr(b"RmStartSession\0")
        .ok_or(PalError::Unsupported(
            "RmStartSession is unavailable in rstrtmgr.dll",
        ))?;
    let register_addr =
        rstrtmgr
            .proc_addr(b"RmRegisterResources\0")
            .ok_or(PalError::Unsupported(
                "RmRegisterResources is unavailable in rstrtmgr.dll",
            ))?;
    let get_list_addr = rstrtmgr
        .proc_addr(b"RmGetList\0")
        .ok_or(PalError::Unsupported(
            "RmGetList is unavailable in rstrtmgr.dll",
        ))?;
    let end_addr = rstrtmgr
        .proc_addr(b"RmEndSession\0")
        .ok_or(PalError::Unsupported(
            "RmEndSession is unavailable in rstrtmgr.dll",
        ))?;

    // SAFETY: Signatures match `RestartManager.h` exports in `rstrtmgr.dll`.
    let rm_start_session: RmStartSessionFn = unsafe { transmute(start_addr) };
    let rm_register_resources: RmRegisterResourcesFn = unsafe { transmute(register_addr) };
    let rm_get_list: RmGetListFn = unsafe { transmute(get_list_addr) };
    let rm_end_session: RmEndSessionFn = unsafe { transmute(end_addr) };

    let mut session_handle: u32 = 0;
    let mut session_key = [0u16; CCH_RM_SESSION_KEY + 1];
    // SAFETY: `session_key` has `CCH_RM_SESSION_KEY + 1` zeroed UTF-16 code units.
    let rc = unsafe { rm_start_session(&mut session_handle, 0, session_key.as_mut_ptr()) };
    if rc != ERROR_SUCCESS {
        return Err(map_win32_error(rc, None, "RmStartSession"));
    }

    let session = RmSessionGuard {
        handle: session_handle,
        end_session: rm_end_session,
    };

    let wide_paths: Vec<Vec<u16>> = files
        .iter()
        .map(|p| os_str_to_wide_null(p.as_os_str()))
        .collect();
    let mut registered_any = false;
    for chunk in wide_paths.chunks(REGISTER_BATCH_SIZE) {
        if register_with_binary_split(session.handle, rm_register_resources, chunk) {
            registered_any = true;
        }
    }

    if !registered_any {
        return Ok(Vec::new());
    }

    let raw_infos = query_rm_list(session.handle, rm_get_list)?;
    let mut holders = Vec::with_capacity(raw_infos.len());
    let mut seen: HashSet<(u32, String, Option<String>)> = HashSet::with_capacity(raw_infos.len());

    for info in raw_infos {
        let pid = info.process.dw_process_id;
        let app_name = decode_wide_z(&info.str_app_name).unwrap_or_default();
        let service_name = decode_wide_z(&info.str_service_short_name);
        if !seen.insert((pid, app_name.clone(), service_name.clone())) {
            continue;
        }
        holders.push(FileLockHolder {
            pid,
            app_name: Arc::from(app_name),
            service_name: service_name.map(Arc::<str>::from),
            app_type: Arc::from(rm_app_type_label(info.application_type)),
            restartable: info.b_restartable != 0,
        });
    }

    holders.sort_by(|a, b| (a.pid, a.app_name.as_ref()).cmp(&(b.pid, b.app_name.as_ref())));
    Ok(holders)
}

/// Registers `paths` with `RmRegisterResources`, recursively splitting the slice in half on
/// batch errors so a single inaccessible or transient file never fails the surrounding batch.
fn register_with_binary_split(
    session_handle: u32,
    rm_register_resources: RmRegisterResourcesFn,
    paths: &[Vec<u16>],
) -> bool {
    if paths.is_empty() {
        return false;
    }

    let ptrs: Vec<*const u16> = paths.iter().map(|w| w.as_ptr()).collect();
    // SAFETY: Every pointer in `ptrs` references a live null-terminated UTF-16 buffer in `paths`.
    let rc = unsafe {
        rm_register_resources(
            session_handle,
            ptrs.len() as u32,
            ptrs.as_ptr(),
            0,
            ptr::null(),
            0,
            ptr::null(),
        )
    };
    if rc == ERROR_SUCCESS {
        return true;
    }
    if paths.len() == 1 {
        return false;
    }

    let mid = paths.len() / 2;
    let left_ok = register_with_binary_split(session_handle, rm_register_resources, &paths[..mid]);
    let right_ok = register_with_binary_split(session_handle, rm_register_resources, &paths[mid..]);
    left_ok || right_ok
}

fn query_rm_list(
    session_handle: u32,
    rm_get_list: RmGetListFn,
) -> Result<Vec<RmProcessInfo>, PalError> {
    let mut needed: u32 = 0;
    let mut count: u32 = 0;
    let mut reboot_reasons: u32 = 0;

    // SAFETY: Null buffer queries required `RmProcessInfo` count.
    let rc = unsafe {
        rm_get_list(
            session_handle,
            &mut needed,
            &mut count,
            ptr::null_mut(),
            &mut reboot_reasons,
        )
    };
    if rc == ERROR_SUCCESS || needed == 0 {
        return Ok(Vec::new());
    }
    if rc != ERROR_MORE_DATA {
        return Err(map_win32_error(rc, None, "RmGetList"));
    }

    for _ in 0..MAX_RM_GET_LIST_RETRIES {
        let cap = (needed as usize).min(4096);
        let mut buf = vec![RmProcessInfo::default(); cap];
        count = buf.len() as u32;
        needed = 0;
        // SAFETY: `buf` is sized for `count` `RmProcessInfo` entries.
        let rc = unsafe {
            rm_get_list(
                session_handle,
                &mut needed,
                &mut count,
                buf.as_mut_ptr(),
                &mut reboot_reasons,
            )
        };
        if rc == ERROR_SUCCESS {
            buf.truncate((count as usize).min(buf.len()));
            return Ok(buf);
        }
        if rc == ERROR_MORE_DATA && needed > 0 {
            continue;
        }
        return Err(map_win32_error(rc, None, "RmGetList"));
    }

    Ok(Vec::new())
}

/// Iteratively collects up to `max_files` regular files inside `root` up to `max_depth`,
/// skipping symlinked directories to prevent cycles.
fn collect_directory_files(root: &Path, max_files: usize, max_depth: usize) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];

    while let Some((dir, depth)) = stack.pop() {
        if files.len() >= max_files {
            break;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if files.len() >= max_files {
                break;
            }
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_file() {
                files.push(entry.path());
            } else if ft.is_dir() && depth < max_depth {
                stack.push((entry.path(), depth + 1));
            }
        }
    }

    files
}

fn decode_wide_z(buf: &[u16]) -> Option<String> {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    if len == 0 {
        return None;
    }
    let s = String::from_utf16_lossy(&buf[..len]);
    let trimmed = s.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Maps Win32 `RM_APP_TYPE` enum values to stable human-readable classification strings.
pub(crate) fn rm_app_type_label(app_type: i32) -> &'static str {
    match app_type {
        1 => "MainWindow",
        2 => "OtherWindow",
        3 => "Service",
        4 => "Explorer",
        5 => "Console",
        1000 => "Critical",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::mem::{offset_of, size_of};

    use super::*;

    #[test]
    fn abi_rm_process_info_layout_matches_windows_sdk() {
        assert_eq!(size_of::<RmUniqueProcess>(), 12);
        assert_eq!(size_of::<RmProcessInfo>(), 668);
        assert_eq!(offset_of!(RmProcessInfo, process), 0);
        assert_eq!(offset_of!(RmProcessInfo, str_app_name), 12);
        assert_eq!(offset_of!(RmProcessInfo, str_service_short_name), 524);
        assert_eq!(offset_of!(RmProcessInfo, application_type), 652);
        assert_eq!(offset_of!(RmProcessInfo, app_status), 656);
        assert_eq!(offset_of!(RmProcessInfo, ts_session_id), 660);
        assert_eq!(offset_of!(RmProcessInfo, b_restartable), 664);
    }

    #[test]
    fn find_file_locks_queries_open_file_and_empty_directory_cleanly() {
        let dir = std::env::temp_dir().join(format!("loom-filelock-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create test dir");

        // Empty directory returns empty result without error.
        let empty = find_file_locks(&dir).expect("empty dir scan");
        assert!(empty.is_empty());

        // Missing path returns InvalidArgument.
        let missing = dir.join("does_not_exist.bin");
        assert!(matches!(
            find_file_locks(&missing),
            Err(PalError::InvalidArgument(_))
        ));

        // Open a file handle in the current process and verify Restart Manager completes cleanly.
        let file_path = dir.join("locked.dat");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&file_path)
            .expect("open temp file");
        file.write_all(b"loom lock test").expect("write temp file");
        file.flush().expect("flush temp file");

        let holders = find_file_locks(&file_path).expect("find_file_locks on open file");
        // Verify well-formed holder entries if Windows reports the open handle.
        for holder in &holders {
            assert!(holder.pid > 0);
            assert!(!holder.app_type.is_empty());
        }

        // Directory scan covering the open file also succeeds cleanly.
        let dir_holders = find_file_locks(&dir).expect("find_file_locks on dir");
        assert!(dir_holders.len() >= holders.len());

        drop(file);
        let _ = fs::remove_dir_all(&dir);
    }
}
