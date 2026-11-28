//! Restart Manager (`rstrtmgr.dll`) file lock detection with RAII session teardown and
//! recursive binary-split batch resource registration.

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

use crate::handle::{OwnedModule, decode_wide_nul_nonempty, map_win32_error, os_str_to_wide_null};

/// Length of `RM_SESSION_KEY` in UTF-16 code units excluding the null terminator (`CCH_RM_SESSION_KEY`).
const CCH_RM_SESSION_KEY: usize = 32;
/// `CCH_RM_MAX_APP_NAME` (`RestartManager.h`).
const CCH_RM_MAX_APP_NAME: usize = 255;
/// `CCH_RM_MAX_SVC_NAME` (`RestartManager.h`).
const CCH_RM_MAX_SVC_NAME: usize = 63;

/// Maximum number of files collected when scanning a directory for Restart Manager.
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
pub(super) struct RmUniqueProcess {
    pub(super) dw_process_id: u32,
    pub(super) process_start_time: FILETIME,
}

/// `RM_PROCESS_INFO` (`RestartManager.h`, 668 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct RmProcessInfo {
    pub(super) process: RmUniqueProcess,
    pub(super) str_app_name: [u16; CCH_RM_MAX_APP_NAME + 1],
    pub(super) str_service_short_name: [u16; CCH_RM_MAX_SVC_NAME + 1],
    pub(super) application_type: i32,
    pub(super) app_status: u32,
    pub(super) ts_session_id: u32,
    pub(super) b_restartable: BOOL,
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

pub(super) fn find_locks_via_restart_manager(
    path: &Path,
    is_dir: bool,
) -> Result<Vec<FileLockHolder>, PalError> {
    let files: Vec<PathBuf> = if is_dir {
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
        let app_name = decode_wide_nul_nonempty(&info.str_app_name).unwrap_or_default();
        let service_name = decode_wide_nul_nonempty(&info.str_service_short_name);
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

/// Maps Win32 `RM_APP_TYPE` enum values to stable human-readable classification strings.
pub(super) fn rm_app_type_label(app_type: i32) -> &'static str {
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
