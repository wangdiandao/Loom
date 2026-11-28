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

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::fs;
use std::mem::{size_of, transmute, zeroed};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use loom_pal_api::{PalError, SystemOperator};
use loom_types::FileLockHolder;
use windows_sys::Wdk::System::SystemInformation::NtQuerySystemInformation;
use windows_sys::Win32::Foundation::{
    DuplicateHandle, ERROR_MORE_DATA, ERROR_SUCCESS, FALSE, FILETIME, HANDLE, INVALID_HANDLE_VALUE,
    STATUS_INFO_LENGTH_MISMATCH, STATUS_SUCCESS,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK, GetFileType,
    GetFinalPathNameByHandleW, OPEN_EXISTING, VOLUME_NAME_DOS,
};
use windows_sys::Win32::System::IO::CancelSynchronousIo;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThreadId, OpenProcess, OpenThread, PROCESS_DUP_HANDLE,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW, THREAD_TERMINATE,
};
use windows_sys::core::BOOL;

use crate::diagnostics::OwnedModule;
use crate::handle::{OwnedHandle, map_win32_error, os_str_to_wide_null};
use crate::operator::WindowsSystemOperator;

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

/// NT `SYSTEM_INFORMATION_CLASS::SystemExtendedHandleInformation` (`64`).
const SYSTEM_EXTENDED_HANDLE_INFORMATION_CLASS: i32 = 64;
/// Maximum buffer size (`64 MiB`) allocated for `SystemExtendedHandleInformation`.
const MAX_HANDLE_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
/// Maximum per-handle probe timeout before cancelling synchronous I/O.
const PER_HANDLE_PROBE_TIMEOUT: Duration = Duration::from_millis(20);
/// Hard total wall-clock budget for the supplementary kernel handle scan.
const TOTAL_HANDLE_SCAN_BUDGET: Duration = Duration::from_millis(2000);
/// Maximum consecutive stalled handles before aborting the handle scan early.
const MAX_STALLED_HANDLES: usize = 8;

/// `THREAD_SET_CONTEXT | THREAD_SUSPEND_RESUME | SYNCHRONIZE` — `CancelSynchronousIo` requires
/// `THREAD_TERMINATE`.
const CANCEL_IO_THREAD_ACCESS: u32 = THREAD_TERMINATE;

/// Access mask representing a pure `SYNCHRONIZE`-only handle with no file read/write/attribute
/// permissions (commonly synchronous wait/pipe handles rather than real file locks).
const PURE_SYNCHRONIZE_ACCESS_MASK: u32 = 0x0010_0000;

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

/// Header of NT `SYSTEM_HANDLE_INFORMATION_EX` (`SystemExtendedHandleInformation = 64`).
#[repr(C)]
#[derive(Clone, Copy)]
struct SystemHandleInformationExHeader {
    number_of_handles: usize,
    reserved: usize,
}

/// Entry of NT `SYSTEM_HANDLE_INFORMATION_EX` (`SYSTEM_HANDLE_TABLE_ENTRY_INFO_EX`).
#[repr(C)]
#[derive(Clone, Copy)]
struct SystemHandleTableEntryInfoEx {
    object: *mut c_void,
    unique_process_id: usize,
    handle_value: usize,
    granted_access: u32,
    creator_back_trace_index: u16,
    object_type_index: u16,
    handle_attributes: u32,
    reserved: u32,
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

fn find_locks_via_restart_manager(
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

    Ok(holders)
}

/// Merges supplementary kernel-handle lock holders into `primary`, preserving Restart Manager's
/// richer metadata whenever a PID was already discovered by Restart Manager.
pub(crate) fn merge_lock_holders(primary: &mut Vec<FileLockHolder>, extra: Vec<FileLockHolder>) {
    let mut existing_pids: HashSet<u32> = primary.iter().map(|h| h.pid).collect();
    for holder in extra {
        if existing_pids.insert(holder.pid) {
            primary.push(holder);
        }
    }
}

/// Returns `true` when `granted_access` is a pure `SYNCHRONIZE`-only mask (`0x0010_0000`)
/// carrying no file read/write/attribute rights.
#[inline]
pub(crate) fn is_blocking_pipe_access_mask(granted_access: u32) -> bool {
    granted_access == PURE_SYNCHRONIZE_ACCESS_MASK
}

/// Normalizes a Win32 final path (stripping `\\?\` or `\\?\UNC\` prefixes and trailing backslashes)
/// for case-insensitive comparison.
pub(crate) fn normalize_final_path(raw: &str) -> String {
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
pub(crate) fn path_matches_target(candidate: &str, target: &str, target_is_dir: bool) -> bool {
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

/// Hang-protected kernel handle scanner backed by `NtQuerySystemInformation(SystemExtendedHandleInformation)`.
///
/// Discovers the OS-version-specific `"File"` `ObjectTypeIndex` dynamically from a short-lived
/// handle to `path`, skips blocking named-pipe access masks, and runs duplicated handle path
/// resolution on a bounded probe worker thread with `CancelSynchronousIo` timeouts.
pub(crate) fn scan_file_locks_via_handles(
    path: &Path,
    is_dir: bool,
) -> Result<Vec<FileLockHolder>, PalError> {
    let probe_handle = open_target_attribute_handle(path)?;
    let target_norm = query_handle_final_path(probe_handle.as_raw())
        .map(|p| normalize_final_path(&p))
        .ok_or_else(|| {
            PalError::InvalidArgument(format!("cannot normalize path: {}", path.display()))
        })?;

    let snapshot = query_system_extended_handles()?;
    let my_pid = std::process::id() as usize;
    let probe_val = probe_handle.as_raw() as usize;

    // Resolve the kernel `ObjectTypeIndex` for `File` objects dynamically from our own probe handle.
    let Some(file_type_index) = snapshot.iter().find_map(|entry| {
        (entry.unique_process_id == my_pid && entry.handle_value == probe_val)
            .then_some(entry.object_type_index)
    }) else {
        return Ok(Vec::new());
    };
    // Close our probe handle before inspecting other handles.
    drop(probe_handle);

    // Group candidate `File` handle values by PID, filtering out kernel PIDs and blocking pipe masks.
    let mut by_pid: HashMap<u32, Vec<usize>> = HashMap::new();
    for entry in &snapshot {
        if entry.object_type_index != file_type_index {
            continue;
        }
        if is_blocking_pipe_access_mask(entry.granted_access) {
            continue;
        }
        let pid = entry.unique_process_id as u32;
        if pid == 0 || pid == 4 {
            continue;
        }
        if entry.unique_process_id == my_pid && entry.handle_value == probe_val {
            continue;
        }
        by_pid.entry(pid).or_default().push(entry.handle_value);
    }

    if by_pid.is_empty() {
        return Ok(Vec::new());
    }

    let mut prober = HandlePathProber::spawn()?;
    let deadline = Instant::now() + TOTAL_HANDLE_SCAN_BUDGET;
    let mut stalled_count = 0usize;
    let mut holders = Vec::new();
    let current_proc = unsafe { GetCurrentProcess() };

    for (pid, handles) in by_pid {
        if Instant::now() >= deadline || stalled_count >= MAX_STALLED_HANDLES {
            break;
        }

        // SAFETY: Opens target process only for handle duplication and image-name query.
        let raw_proc = unsafe {
            OpenProcess(
                PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION,
                FALSE,
                pid,
            )
        };
        let Some(proc_handle) = OwnedHandle::from_raw(raw_proc) else {
            continue;
        };

        let mut matched = false;
        for raw_handle_val in handles {
            if Instant::now() >= deadline || stalled_count >= MAX_STALLED_HANDLES {
                break;
            }
            let mut dup_raw: HANDLE = ptr::null_mut();
            // SAFETY: Duplicates handle into current process with `FILE_READ_ATTRIBUTES` for path query.
            let ok = unsafe {
                DuplicateHandle(
                    proc_handle.as_raw(),
                    raw_handle_val as HANDLE,
                    current_proc,
                    &mut dup_raw,
                    FILE_READ_ATTRIBUTES,
                    FALSE,
                    0,
                )
            };
            if ok == 0 || dup_raw.is_null() {
                continue;
            }

            match prober.probe_and_close(dup_raw, PER_HANDLE_PROBE_TIMEOUT) {
                ProbeResult::Path(Some(candidate_raw)) => {
                    let candidate_norm = normalize_final_path(&candidate_raw);
                    if path_matches_target(&candidate_norm, &target_norm, is_dir) {
                        matched = true;
                        break;
                    }
                }
                ProbeResult::Path(None) => {}
                ProbeResult::TimedOut => {
                    stalled_count += 1;
                    // Respawn a fresh probe worker if the previous one stayed wedged.
                    if stalled_count < MAX_STALLED_HANDLES
                        && let Ok(fresh) = HandlePathProber::spawn()
                    {
                        prober = fresh;
                    } else {
                        break;
                    }
                }
            }
        }

        if matched {
            let app_name = query_process_display_name(proc_handle.as_raw(), pid);
            holders.push(FileLockHolder {
                pid,
                app_name: Arc::from(app_name),
                service_name: None,
                app_type: Arc::from("Process"),
                restartable: false,
            });
        }
    }

    Ok(holders)
}

fn open_target_attribute_handle(path: &Path) -> Result<OwnedHandle, PalError> {
    let wide = os_str_to_wide_null(path.as_os_str());
    // SAFETY: Opens `path` (file or directory) with `FILE_READ_ATTRIBUTES` and full sharing.
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE || raw.is_null() {
        let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        return Err(map_win32_error(code, None, "CreateFileW(filelock)"));
    }
    OwnedHandle::from_raw(raw).ok_or_else(|| PalError::OsError {
        code: -1,
        message: "CreateFileW returned null handle".into(),
    })
}

fn query_handle_final_path(handle: HANDLE) -> Option<String> {
    let mut buf = [0u16; 1024];
    // SAFETY: `handle` is a valid file/directory handle; `buf` holds 1024 UTF-16 units.
    let len = unsafe {
        GetFinalPathNameByHandleW(
            handle,
            buf.as_mut_ptr(),
            buf.len() as u32,
            VOLUME_NAME_DOS | FILE_NAME_NORMALIZED,
        )
    } as usize;
    if len == 0 {
        return None;
    }
    if len < buf.len() {
        return Some(String::from_utf16_lossy(&buf[..len]));
    }
    let mut big = vec![0u16; len + 1];
    let len2 = unsafe {
        GetFinalPathNameByHandleW(
            handle,
            big.as_mut_ptr(),
            big.len() as u32,
            VOLUME_NAME_DOS | FILE_NAME_NORMALIZED,
        )
    } as usize;
    if len2 > 0 && len2 < big.len() {
        Some(String::from_utf16_lossy(&big[..len2]))
    } else {
        None
    }
}

fn query_system_extended_handles() -> Result<Vec<SystemHandleTableEntryInfoEx>, PalError> {
    let mut capacity: usize = 2 * 1024 * 1024;
    let mut buf: Vec<u8> = Vec::new();

    for _ in 0..8 {
        if capacity > MAX_HANDLE_SNAPSHOT_BYTES {
            return Err(PalError::OsError {
                code: -1,
                message: "SystemExtendedHandleInformation buffer exceeded 64 MiB safety cap".into(),
            });
        }
        buf.resize(capacity, 0);
        let mut needed: u32 = 0;
        // SAFETY: Queries `SystemExtendedHandleInformation` (`64`) into `buf`.
        let status = unsafe {
            NtQuerySystemInformation(
                SYSTEM_EXTENDED_HANDLE_INFORMATION_CLASS,
                buf.as_mut_ptr().cast::<c_void>(),
                buf.len() as u32,
                &mut needed,
            )
        };
        if status == STATUS_SUCCESS {
            if buf.len() < size_of::<SystemHandleInformationExHeader>() {
                return Ok(Vec::new());
            }
            // SAFETY: Header is located at byte offset 0 of `buf`.
            let header = unsafe {
                ptr::read_unaligned(buf.as_ptr().cast::<SystemHandleInformationExHeader>())
            };
            let entry_bytes = buf.len() - size_of::<SystemHandleInformationExHeader>();
            let max_entries = entry_bytes / size_of::<SystemHandleTableEntryInfoEx>();
            let count = header.number_of_handles.min(max_entries);
            let mut out = Vec::with_capacity(count);
            let base = unsafe {
                buf.as_ptr()
                    .add(size_of::<SystemHandleInformationExHeader>())
            };
            for idx in 0..count {
                // SAFETY: `idx < count <= max_entries`, strictly inside `buf`.
                let entry = unsafe {
                    ptr::read_unaligned(
                        base.add(idx * size_of::<SystemHandleTableEntryInfoEx>())
                            .cast::<SystemHandleTableEntryInfoEx>(),
                    )
                };
                out.push(entry);
            }
            return Ok(out);
        }
        if status == STATUS_INFO_LENGTH_MISMATCH {
            capacity = ((needed as usize).max(capacity)).saturating_mul(2);
            continue;
        }
        return Err(PalError::OsError {
            code: status,
            message: format!(
                "NtQuerySystemInformation(SystemExtendedHandleInformation) failed (0x{:08x})",
                status as u32
            ),
        });
    }

    Ok(Vec::new())
}

fn query_process_display_name(proc_handle: HANDLE, pid: u32) -> String {
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `proc_handle` has `PROCESS_QUERY_LIMITED_INFORMATION`.
    let ok = unsafe { QueryFullProcessImageNameW(proc_handle, 0, buf.as_mut_ptr(), &mut len) };
    if ok != 0 && len > 0 {
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        if let Some(name) = Path::new(&full).file_name().and_then(|n| n.to_str())
            && !name.is_empty()
        {
            return name.to_owned();
        }
    }
    format!("PID {pid}")
}

enum ProbeResult {
    Path(Option<String>),
    TimedOut,
}

/// Isolated single worker thread for probing duplicated file handles with strict timeout
/// and `CancelSynchronousIo` cancellation so wedged pipes/devices can never hang the caller.
struct HandlePathProber {
    req_tx: mpsc::Sender<usize>,
    resp_rx: mpsc::Receiver<Option<String>>,
    worker_thread: Option<OwnedHandle>,
}

impl HandlePathProber {
    fn spawn() -> Result<Self, PalError> {
        let (req_tx, req_rx) = mpsc::channel::<usize>();
        let (resp_tx, resp_rx) = mpsc::channel::<Option<String>>();
        let (tid_tx, tid_rx) = mpsc::channel::<u32>();

        std::thread::Builder::new()
            .name("loom-filelock-probe".into())
            .spawn(move || {
                // SAFETY: Returns calling Win32 thread ID.
                let tid = unsafe { GetCurrentThreadId() };
                let _ = tid_tx.send(tid);
                while let Ok(raw_val) = req_rx.recv() {
                    let Some(handle) = OwnedHandle::from_raw(raw_val as HANDLE) else {
                        if resp_tx.send(None).is_err() {
                            break;
                        }
                        continue;
                    };
                    // SAFETY: Only resolve final path for real disk files/directories (`FILE_TYPE_DISK`).
                    let path = if unsafe { GetFileType(handle.as_raw()) } == FILE_TYPE_DISK {
                        query_handle_final_path(handle.as_raw())
                    } else {
                        None
                    };
                    drop(handle);
                    if resp_tx.send(path).is_err() {
                        break;
                    }
                }
            })
            .map_err(PalError::Io)?;

        let worker_thread = tid_rx
            .recv_timeout(Duration::from_secs(1))
            .ok()
            .and_then(|tid| {
                // SAFETY: Opens worker thread for `CancelSynchronousIo` on timeout.
                let raw = unsafe { OpenThread(CANCEL_IO_THREAD_ACCESS, FALSE, tid) };
                OwnedHandle::from_raw(raw)
            });

        Ok(Self {
            req_tx,
            resp_rx,
            worker_thread,
        })
    }

    /// Transfers ownership of `dup_handle` to the worker thread, waits up to `timeout` for
    /// its final path, and cancels synchronous I/O if stalled.
    fn probe_and_close(&self, dup_handle: HANDLE, timeout: Duration) -> ProbeResult {
        if self.req_tx.send(dup_handle as usize).is_err() {
            // Worker already exited; close handle here.
            drop(OwnedHandle::from_raw(dup_handle));
            return ProbeResult::TimedOut;
        }
        match self.resp_rx.recv_timeout(timeout) {
            Ok(path) => ProbeResult::Path(path),
            Err(_) => {
                if let Some(ref thread) = self.worker_thread {
                    // SAFETY: Best-effort cancellation of any blocked kernel IRP on the probe thread.
                    unsafe {
                        let _ = CancelSynchronousIo(thread.as_raw());
                    }
                }
                ProbeResult::TimedOut
            }
        }
    }
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
    fn abi_rm_process_info_and_extended_handle_layouts_match_windows_sdk() {
        assert_eq!(size_of::<RmUniqueProcess>(), 12);
        assert_eq!(size_of::<RmProcessInfo>(), 668);
        assert_eq!(offset_of!(RmProcessInfo, process), 0);
        assert_eq!(offset_of!(RmProcessInfo, str_app_name), 12);
        assert_eq!(offset_of!(RmProcessInfo, str_service_short_name), 524);
        assert_eq!(offset_of!(RmProcessInfo, application_type), 652);
        assert_eq!(offset_of!(RmProcessInfo, app_status), 656);
        assert_eq!(offset_of!(RmProcessInfo, ts_session_id), 660);
        assert_eq!(offset_of!(RmProcessInfo, b_restartable), 664);

        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(size_of::<SystemHandleInformationExHeader>(), 16);
            assert_eq!(size_of::<SystemHandleTableEntryInfoEx>(), 40);
            assert_eq!(
                offset_of!(SystemHandleTableEntryInfoEx, unique_process_id),
                8
            );
            assert_eq!(offset_of!(SystemHandleTableEntryInfoEx, handle_value), 16);
            assert_eq!(offset_of!(SystemHandleTableEntryInfoEx, granted_access), 24);
            assert_eq!(
                offset_of!(SystemHandleTableEntryInfoEx, object_type_index),
                30
            );
        }
    }

    #[test]
    fn path_normalization_matching_and_holder_merge_rules() {
        assert_eq!(
            normalize_final_path(r"\\?\C:\Temp\foo.txt"),
            r"C:\Temp\foo.txt"
        );
        assert_eq!(normalize_final_path(r"\\?\C:\Temp\"), r"C:\Temp");
        assert_eq!(normalize_final_path(r"\\?\C:\"), r"C:\");
        assert_eq!(
            normalize_final_path(r"\\?\UNC\server\share\dir\"),
            r"\\server\share\dir"
        );

        assert!(path_matches_target(
            r"C:\Temp\Foo.txt",
            r"c:\temp\foo.txt",
            false
        ));
        assert!(!path_matches_target(
            r"C:\Temp\Foo.txt.bak",
            r"C:\Temp\Foo.txt",
            false
        ));
        assert!(path_matches_target(r"C:\Temp\Dir", r"C:\Temp\Dir", true));
        assert!(path_matches_target(
            r"C:\Temp\Dir\sub\a.dat",
            r"C:\Temp\Dir",
            true
        ));
        assert!(!path_matches_target(
            r"C:\Temp\DirOther\a.dat",
            r"C:\Temp\Dir",
            true
        ));

        assert!(is_blocking_pipe_access_mask(0x0010_0000));
        assert!(!is_blocking_pipe_access_mask(0x0012_019F));
        assert!(!is_blocking_pipe_access_mask(0x0012_0089));

        let mut primary = vec![FileLockHolder {
            pid: 100,
            app_name: Arc::from("RichApp"),
            service_name: Some(Arc::from("svc")),
            app_type: Arc::from("Service"),
            restartable: true,
        }];
        merge_lock_holders(
            &mut primary,
            vec![
                FileLockHolder {
                    pid: 100,
                    app_name: Arc::from("raw.exe"),
                    service_name: None,
                    app_type: Arc::from("Process"),
                    restartable: false,
                },
                FileLockHolder {
                    pid: 200,
                    app_name: Arc::from("cmd.exe"),
                    service_name: None,
                    app_type: Arc::from("Process"),
                    restartable: false,
                },
            ],
        );
        assert_eq!(primary.len(), 2);
        assert_eq!(primary[0].app_name.as_ref(), "RichApp");
        assert_eq!(primary[1].pid, 200);
    }

    #[test]
    fn find_file_locks_queries_open_file_and_handle_scanner_detects_open_file_and_directory() {
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

        // Open a file handle in the current process and verify both Restart Manager and
        // SystemExtendedHandleInformation detect it cleanly.
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
        for holder in &holders {
            assert!(holder.pid > 0);
            assert!(!holder.app_type.is_empty());
        }

        let handle_holders =
            scan_file_locks_via_handles(&file_path, false).expect("handle scan on open file");
        let my_pid = std::process::id();
        assert!(
            handle_holders.iter().any(|h| h.pid == my_pid),
            "SystemExtendedHandleInformation scanner must detect current process holding open file"
        );

        let dir_handle_holders =
            scan_file_locks_via_handles(&dir, true).expect("handle scan on dir with open child");
        assert!(
            dir_handle_holders.iter().any(|h| h.pid == my_pid),
            "SystemExtendedHandleInformation directory scan must detect process holding child file"
        );

        // Directory scan covering the open file also succeeds cleanly.
        let dir_holders = find_file_locks(&dir).expect("find_file_locks on dir");
        assert!(dir_holders.len() >= holders.len());

        drop(file);
        let _ = fs::remove_dir_all(&dir);
    }
}
