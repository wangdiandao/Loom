//! Hang-protected kernel handle scanner backed by
//! `NtQuerySystemInformation(SystemExtendedHandleInformation)`.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::path::Path;
use std::ptr;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use loom_pal_api::PalError;
use loom_types::FileLockHolder;
use windows_sys::Wdk::System::SystemInformation::NtQuerySystemInformation;
use windows_sys::Win32::Foundation::{
    DuplicateHandle, FALSE, HANDLE, INVALID_HANDLE_VALUE, STATUS_INFO_LENGTH_MISMATCH,
    STATUS_SUCCESS,
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

use crate::handle::{OwnedHandle, map_win32_error, os_str_to_wide_null};

use super::{normalize_final_path, path_matches_target};

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

/// `CancelSynchronousIo` requires `THREAD_TERMINATE`.
const CANCEL_IO_THREAD_ACCESS: u32 = THREAD_TERMINATE;

/// Access mask representing a pure `SYNCHRONIZE`-only handle with no file read/write/attribute
/// permissions (commonly synchronous wait/pipe handles rather than real file locks).
const PURE_SYNCHRONIZE_ACCESS_MASK: u32 = 0x0010_0000;

/// Header of NT `SYSTEM_HANDLE_INFORMATION_EX` (`SystemExtendedHandleInformation = 64`).
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct SystemHandleInformationExHeader {
    pub(super) number_of_handles: usize,
    pub(super) reserved: usize,
}

/// Entry of NT `SYSTEM_HANDLE_INFORMATION_EX` (`SYSTEM_HANDLE_TABLE_ENTRY_INFO_EX`).
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct SystemHandleTableEntryInfoEx {
    pub(super) object: *mut c_void,
    pub(super) unique_process_id: usize,
    pub(super) handle_value: usize,
    pub(super) granted_access: u32,
    pub(super) creator_back_trace_index: u16,
    pub(super) object_type_index: u16,
    pub(super) handle_attributes: u32,
    pub(super) reserved: u32,
}

/// Returns `true` when `granted_access` is a pure `SYNCHRONIZE`-only mask (`0x0010_0000`)
/// carrying no file read/write/attribute rights.
#[inline]
pub(super) fn is_blocking_pipe_access_mask(granted_access: u32) -> bool {
    granted_access == PURE_SYNCHRONIZE_ACCESS_MASK
}

/// Hang-protected kernel handle scanner backed by `NtQuerySystemInformation(SystemExtendedHandleInformation)`.
///
/// Discovers the OS-version-specific `"File"` `ObjectTypeIndex` dynamically from a short-lived
/// handle to `path`, skips blocking named-pipe access masks, and runs duplicated handle path
/// resolution on a bounded probe worker thread with `CancelSynchronousIo` timeouts.
pub(super) fn scan_file_locks_via_handles(
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
        let clamped = (len as usize).min(buf.len());
        let full = String::from_utf16_lossy(&buf[..clamped]);
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
