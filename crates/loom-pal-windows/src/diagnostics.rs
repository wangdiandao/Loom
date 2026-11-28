//! Windows kernel diagnostics: Wait Chain Traversal (WCT) and cancellable streaming MiniDump.
//!
//! Both `advapi32.dll` WCT entry points and `dbghelp.dll` `MiniDumpWriteDump` are loaded
//! dynamically via `LoadLibraryExW(..., LOAD_LIBRARY_SEARCH_SYSTEM32)` / `GetProcAddress`
//! with RAII guards so Loom carries no static link dependency on `dbghelp.dll` and never
//! leaks sessions or kernel handles.

use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::OpenOptions;
use std::mem::{size_of, zeroed};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use loom_pal_api::PalError;
use loom_types::{ProcessKey, WaitChainNode};
use windows_sys::Win32::Foundation::{ERROR_CANCELLED, FALSE, FILETIME, GetLastError, HANDLE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_VM_READ, QueryFullProcessImageNameW,
};
use windows_sys::core::BOOL;

pub(crate) use crate::handle::OwnedModule;
use crate::handle::{
    OwnedHandle, decode_wide_nul_nonempty, filetime_to_u64, map_win32_error, open_verified_process,
};

/// Maximum WCT chain length per Win32 `WCT_MAX_NODE_COUNT`.
const WCT_MAX_NODE_COUNT: usize = 16;

/// Maximum UTF-16 characters in `WAITCHAIN_NODE_INFO::LockObject::ObjectName`.
const WCT_OBJNAME_LENGTH: usize = 128;

/// Enable cross-process wait chain traversal (`WCT_OUT_OF_PROC_FLAG`).
const WCT_OUT_OF_PROC_FLAG: u32 = 1;

/// Safety cap on threads inspected per process so pathological thread counts cannot stall diagnostics.
const MAX_THREADS_PER_WCT_SCAN: usize = 512;

/// Win32 `WCT_OBJECT_TYPE` values from `wct.h`.
const WCT_CRITICAL_SECTION_TYPE: i32 = 1;
const WCT_SEND_MESSAGE_TYPE: i32 = 2;
const WCT_MUTEX_TYPE: i32 = 3;
const WCT_ALPC_TYPE: i32 = 4;
const WCT_COM_TYPE: i32 = 5;
const WCT_THREAD_WAIT_TYPE: i32 = 6;
const WCT_PROCESS_WAIT_TYPE: i32 = 7;
const WCT_THREAD_TYPE: i32 = 8;
const WCT_COM_ACTIVATION_TYPE: i32 = 9;
const WCT_UNKNOWN_TYPE: i32 = 10;
const WCT_SOCKET_IO_TYPE: i32 = 11;
const WCT_SMB_IO_TYPE: i32 = 12;

/// Win32 `MINIDUMP_TYPE` flags for full-memory diagnostic dumps.
const MINIDUMP_WITH_FULL_MEMORY: u32 = 0x0000_0002;
const MINIDUMP_WITH_HANDLE_DATA: u32 = 0x0000_0004;
const MINIDUMP_WITH_UNLOADED_MODULES: u32 = 0x0000_0020;
const MINIDUMP_WITH_THREAD_INFO: u32 = 0x0000_1000;

/// Win32 `MINIDUMP_CALLBACK_TYPE::CancelCallback` (`6` in `minidumpapiset.h`).
const MINIDUMP_CANCEL_CALLBACK: u32 = 6;

/// `HRESULT_FROM_WIN32(ERROR_CANCELLED)` (`0x800704C7`).
const HRESULT_ERROR_CANCELLED: u32 = 0x8007_04C7;

#[repr(C)]
#[derive(Clone, Copy)]
struct WctLockObject {
    object_name: [u16; WCT_OBJNAME_LENGTH],
    timeout: i64,
    alertable: BOOL,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct WctThreadObject {
    process_id: u32,
    thread_id: u32,
    wait_time: u32,
    context_switches: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
union WctNodeUnion {
    lock_object: WctLockObject,
    thread_object: WctThreadObject,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct WaitChainNodeInfo {
    object_type: i32,
    object_status: i32,
    data: WctNodeUnion,
}

impl Default for WaitChainNodeInfo {
    fn default() -> Self {
        // SAFETY: `WaitChainNodeInfo` is plain C POD data.
        unsafe { zeroed() }
    }
}

type OpenThreadWaitChainSessionFn = unsafe extern "system" fn(u32, *const c_void) -> *mut c_void;
type CloseThreadWaitChainSessionFn = unsafe extern "system" fn(*mut c_void);
type GetThreadWaitChainFn = unsafe extern "system" fn(
    *mut c_void,
    usize,
    u32,
    u32,
    *mut u32,
    *mut WaitChainNodeInfo,
    *mut BOOL,
) -> BOOL;

/// RAII guard ensuring `CloseThreadWaitChainSession` always runs even on early error returns.
struct WctSessionGuard {
    handle: *mut c_void,
    close_fn: CloseThreadWaitChainSessionFn,
}

impl Drop for WctSessionGuard {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: `self.handle` is a valid WCT session opened by `OpenThreadWaitChainSession`.
            unsafe {
                (self.close_fn)(self.handle);
            }
        }
    }
}

/// Inspects the Windows Wait Chain Traversal (WCT) dependency graph for all threads of `key`.
pub(crate) fn analyze_wait_chain(key: ProcessKey) -> Result<Vec<WaitChainNode>, PalError> {
    if key.pid == 0 || key.pid == 4 {
        return Err(PalError::Unsupported(
            "wait chain analysis is not supported for Windows kernel system processes",
        ));
    }

    // Verify target process identity before opening WCT session.
    let _verified_proc = open_verified_process(key, PROCESS_QUERY_LIMITED_INFORMATION)?;

    let advapi32 = OwnedModule::load("advapi32.dll")?;
    let open_addr =
        advapi32
            .proc_addr(b"OpenThreadWaitChainSession\0")
            .ok_or(PalError::Unsupported(
                "OpenThreadWaitChainSession is unavailable on this Windows version",
            ))?;
    let close_addr =
        advapi32
            .proc_addr(b"CloseThreadWaitChainSession\0")
            .ok_or(PalError::Unsupported(
                "CloseThreadWaitChainSession is unavailable on this Windows version",
            ))?;
    let get_addr = advapi32
        .proc_addr(b"GetThreadWaitChain\0")
        .ok_or(PalError::Unsupported(
            "GetThreadWaitChain is unavailable on this Windows version",
        ))?;

    // SAFETY: Signatures match Win32 `wct.h` definitions in `advapi32.dll`.
    let open_session: OpenThreadWaitChainSessionFn = unsafe { std::mem::transmute(open_addr) };
    let close_session: CloseThreadWaitChainSessionFn = unsafe { std::mem::transmute(close_addr) };
    let get_chain: GetThreadWaitChainFn = unsafe { std::mem::transmute(get_addr) };

    // Open a synchronous WCT session (flags = 0, callback = NULL).
    // SAFETY: Synchronous session takes flags 0 and null callback.
    let session_handle = unsafe { open_session(0, ptr::null()) };
    if session_handle.is_null() {
        // SAFETY: Reads thread-local Win32 error set by `OpenThreadWaitChainSession`.
        let err = unsafe { GetLastError() };
        return Err(map_win32_error(
            err,
            Some(key),
            "OpenThreadWaitChainSession",
        ));
    }
    let session = WctSessionGuard {
        handle: session_handle,
        close_fn: close_session,
    };

    let thread_ids = enumerate_process_threads(key.pid)?;
    let mut proc_cache: HashMap<u32, (Option<ProcessKey>, Option<Arc<str>>)> = HashMap::new();
    let mut roots = Vec::new();

    for tid in thread_ids.into_iter().take(MAX_THREADS_PER_WCT_SCAN) {
        let mut nodes = [WaitChainNodeInfo::default(); WCT_MAX_NODE_COUNT];
        let mut node_count = WCT_MAX_NODE_COUNT as u32;
        let mut is_cycle: BOOL = 0;

        // SAFETY: `session.handle` is valid and `nodes` has `WCT_MAX_NODE_COUNT` capacity.
        let ok = unsafe {
            get_chain(
                session.handle,
                0,
                WCT_OUT_OF_PROC_FLAG,
                tid,
                &mut node_count,
                nodes.as_mut_ptr(),
                &mut is_cycle,
            )
        };
        if ok == 0 {
            continue;
        }

        let count = (node_count as usize).min(WCT_MAX_NODE_COUNT);
        let deadlocked = is_cycle != 0;
        // Skip threads that are running normally (single self-thread node, no wait dependency).
        if count <= 1 && !deadlocked {
            continue;
        }

        if let Some(root) = build_wait_chain_tree(&nodes[..count], deadlocked, &mut proc_cache) {
            roots.push(root);
        }
    }

    Ok(roots)
}

/// Enumerates all thread IDs owned by `pid` via `CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD)`.
fn enumerate_process_threads(pid: u32) -> Result<Vec<u32>, PalError> {
    // SAFETY: `CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)` takes no pointers.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    let Some(snapshot) = OwnedHandle::from_raw(raw) else {
        // SAFETY: Reads thread-local Win32 error set by `CreateToolhelp32Snapshot`.
        let err = unsafe { GetLastError() };
        return Err(map_win32_error(err, None, "CreateToolhelp32Snapshot"));
    };

    // SAFETY: `THREADENTRY32` is C POD with `dwSize` initialized before calling `Thread32First`.
    let mut entry: THREADENTRY32 = unsafe { zeroed() };
    entry.dwSize = size_of::<THREADENTRY32>() as u32;

    let mut tids = Vec::new();
    // SAFETY: `snapshot` is a valid thread snapshot handle and `entry.dwSize` is set.
    let mut has_next = unsafe { Thread32First(snapshot.as_raw(), &mut entry) };
    while has_next != 0 {
        if entry.th32OwnerProcessID == pid {
            tids.push(entry.th32ThreadID);
        }
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        // SAFETY: Continues iteration over the valid thread snapshot.
        has_next = unsafe { Thread32Next(snapshot.as_raw(), &mut entry) };
    }

    Ok(tids)
}

/// Folds a linear WCT node array `[n0, n1, ..., nk]` into a nested [`WaitChainNode`] tree.
fn build_wait_chain_tree(
    nodes: &[WaitChainNodeInfo],
    is_deadlocked: bool,
    proc_cache: &mut HashMap<u32, (Option<ProcessKey>, Option<Arc<str>>)>,
) -> Option<WaitChainNode> {
    let mut child: Option<WaitChainNode> = None;

    for info in nodes.iter().rev() {
        let mut node = convert_wct_node(info, is_deadlocked, proc_cache);
        if let Some(next_child) = child.take() {
            node.children.push(next_child);
        }
        child = Some(node);
    }

    child
}

fn convert_wct_node(
    info: &WaitChainNodeInfo,
    is_deadlocked: bool,
    proc_cache: &mut HashMap<u32, (Option<ProcessKey>, Option<Arc<str>>)>,
) -> WaitChainNode {
    if info.object_type == WCT_THREAD_TYPE {
        // SAFETY: `object_type == WCT_THREAD_TYPE` selects `data.thread_object`.
        let thread = unsafe { info.data.thread_object };
        let (process_key, process_name) = resolve_process_identity(thread.process_id, proc_cache);
        WaitChainNode {
            thread_id: thread.thread_id,
            process_key,
            process_name,
            object_type: Arc::from("Thread"),
            object_name: None,
            is_deadlocked,
            children: Vec::new(),
        }
    } else {
        // SAFETY: All non-thread `WCT_OBJECT_TYPE` variants select `data.lock_object`.
        let lock = unsafe { info.data.lock_object };
        let object_name = decode_wide_nul_nonempty(&lock.object_name).map(Arc::<str>::from);
        WaitChainNode {
            thread_id: 0,
            process_key: None,
            process_name: None,
            object_type: Arc::from(wct_object_type_label(info.object_type)),
            object_name,
            is_deadlocked,
            children: Vec::new(),
        }
    }
}

/// Maps a Win32 `WCT_OBJECT_TYPE` discriminant to a stable human-readable name.
pub(crate) fn wct_object_type_label(object_type: i32) -> &'static str {
    match object_type {
        WCT_CRITICAL_SECTION_TYPE => "CriticalSection",
        WCT_SEND_MESSAGE_TYPE => "SendMessage",
        WCT_MUTEX_TYPE => "Mutex",
        WCT_ALPC_TYPE => "Alpc",
        WCT_COM_TYPE => "Com",
        WCT_THREAD_WAIT_TYPE => "ThreadWait",
        WCT_PROCESS_WAIT_TYPE => "ProcessWait",
        WCT_THREAD_TYPE => "Thread",
        WCT_COM_ACTIVATION_TYPE => "ComActivation",
        WCT_SOCKET_IO_TYPE => "SocketIo",
        WCT_SMB_IO_TYPE => "SmbIo",
        WCT_UNKNOWN_TYPE => "Unknown",
        _ => "Unknown",
    }
}

/// Resolves `(Option<ProcessKey>, Option<short_name>)` for `pid`, memoizing per scan.
fn resolve_process_identity(
    pid: u32,
    cache: &mut HashMap<u32, (Option<ProcessKey>, Option<Arc<str>>)>,
) -> (Option<ProcessKey>, Option<Arc<str>>) {
    if pid == 0 {
        return (None, None);
    }
    if let Some(entry) = cache.get(&pid) {
        return entry.clone();
    }

    let resolved = (|| {
        // SAFETY: `OpenProcess` with `PROCESS_QUERY_LIMITED_INFORMATION` is read-only.
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
        let handle = OwnedHandle::from_raw(raw)?;

        let mut create = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: `handle` is valid and all `FILETIME` outputs are stack locals.
        let time_ok = unsafe {
            GetProcessTimes(
                handle.as_raw(),
                &mut create,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        };
        let key = (time_ok != 0).then(|| ProcessKey::new(pid, filetime_to_u64(create)));

        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        // SAFETY: `handle` has `PROCESS_QUERY_LIMITED_INFORMATION` and `buf` is writable.
        let name_ok =
            unsafe { QueryFullProcessImageNameW(handle.as_raw(), 0, buf.as_mut_ptr(), &mut len) };
        let short_name = if name_ok != 0 && len > 0 {
            let full = String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]);
            Path::new(&full)
                .file_name()
                .and_then(|s| s.to_str())
                .map(Arc::<str>::from)
        } else {
            None
        };

        Some((key, short_name))
    })()
    .unwrap_or((None, None));

    cache.insert(pid, resolved.clone());
    resolved
}

// ---------------------------------------------------------------------------
// Streaming MiniDump (`dbghelp.dll::MiniDumpWriteDump` with `CancelCallback`)
// ---------------------------------------------------------------------------

/// `MINIDUMP_CALLBACK_INPUT` header prefix (`#include <pshpack4.h>` in `minidumpapiset.h`).
#[repr(C, packed(4))]
struct MiniDumpCallbackInputHeader {
    process_id: u32,
    process_handle: HANDLE,
    callback_type: u32,
}

/// `MINIDUMP_CALLBACK_OUTPUT` payload for `CancelCallback` (`#include <pshpack4.h>`).
#[repr(C)]
struct MiniDumpCancelOutput {
    check_cancel: BOOL,
    cancel: BOOL,
}

type MiniDumpCallbackRoutine = unsafe extern "system" fn(
    callback_param: *mut c_void,
    callback_input: *const MiniDumpCallbackInputHeader,
    callback_output: *mut c_void,
) -> BOOL;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[repr(C, packed(4))]
struct MiniDumpCallbackInformation {
    callback_routine: MiniDumpCallbackRoutine,
    callback_param: *mut c_void,
}

#[cfg(target_arch = "x86")]
#[repr(C)]
struct MiniDumpCallbackInformation {
    callback_routine: MiniDumpCallbackRoutine,
    callback_param: *mut c_void,
}

type MiniDumpWriteDumpFn = unsafe extern "system" fn(
    h_process: HANDLE,
    process_id: u32,
    h_file: HANDLE,
    dump_type: u32,
    exception_param: *const c_void,
    user_stream_param: *const c_void,
    callback_param: *const MiniDumpCallbackInformation,
) -> BOOL;

/// `dbghelp.dll` callback routine polling the shared cancellation `AtomicBool` on `CancelCallback`.
unsafe extern "system" fn minidump_cancel_callback(
    callback_param: *mut c_void,
    callback_input: *const MiniDumpCallbackInputHeader,
    callback_output: *mut c_void,
) -> BOOL {
    if callback_param.is_null() || callback_input.is_null() || callback_output.is_null() {
        return 1;
    }

    // SAFETY: `callback_input` is valid for the duration of the callback; read unaligned because
    // `MiniDumpCallbackInputHeader` is `packed(4)`.
    let callback_type = unsafe { ptr::addr_of!((*callback_input).callback_type).read_unaligned() };
    if callback_type == MINIDUMP_CANCEL_CALLBACK {
        // SAFETY: `callback_param` points to the caller's `&AtomicBool` for the duration of `MiniDumpWriteDump`.
        let cancel_flag = unsafe { &*(callback_param as *const AtomicBool) };
        // SAFETY: For `CancelCallback`, `callback_output` points to `{ BOOL CheckCancel; BOOL Cancel; }`.
        let out = unsafe { &mut *(callback_output as *mut MiniDumpCancelOutput) };
        out.check_cancel = 1;
        out.cancel = if cancel_flag.load(Ordering::Relaxed) {
            1
        } else {
            0
        };
    }

    1
}

/// Streams a process memory minidump to `dest`, periodically checking `cancel`.
///
/// If cancelled or if an error occurs, the file handle is closed and any partial `.DMP` file
/// is removed cleanly before returning.
pub(crate) fn write_minidump(
    key: ProcessKey,
    dest: &Path,
    cancel: &AtomicBool,
) -> Result<(), PalError> {
    if key.pid == 0 || key.pid == 4 {
        return Err(PalError::Unsupported(
            "minidump is not supported for Windows kernel system processes",
        ));
    }

    if cancel.load(Ordering::Relaxed) {
        return Err(PalError::Cancelled);
    }

    let proc_handle = open_verified_process(key, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ)?;

    let dbghelp = OwnedModule::load("dbghelp.dll")?;
    let write_dump_addr =
        dbghelp
            .proc_addr(b"MiniDumpWriteDump\0")
            .ok_or(PalError::Unsupported(
                "MiniDumpWriteDump is unavailable in dbghelp.dll",
            ))?;

    // SAFETY: Signature matches `MiniDumpWriteDump` in `dbghelp.dll`.
    let write_dump: MiniDumpWriteDumpFn = unsafe { std::mem::transmute(write_dump_addr) };

    if cancel.load(Ordering::Relaxed) {
        return Err(PalError::Cancelled);
    }

    if let Some(parent) = dest.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dest)?;

    let callback_info = MiniDumpCallbackInformation {
        callback_routine: minidump_cancel_callback,
        callback_param: (cancel as *const AtomicBool) as *mut c_void,
    };

    let dump_type = MINIDUMP_WITH_FULL_MEMORY
        | MINIDUMP_WITH_HANDLE_DATA
        | MINIDUMP_WITH_UNLOADED_MODULES
        | MINIDUMP_WITH_THREAD_INFO;

    // SAFETY: `proc_handle` has `PROCESS_QUERY_INFORMATION | PROCESS_VM_READ`, `file` is open
    // for writing, and `callback_info` + `cancel` outlive the synchronous `MiniDumpWriteDump` call.
    let ok = unsafe {
        write_dump(
            proc_handle.as_raw(),
            key.pid,
            file.as_raw_handle() as HANDLE,
            dump_type,
            ptr::null(),
            ptr::null(),
            &callback_info,
        )
    };
    // SAFETY: Captures Win32 error immediately before dropping the file handle.
    let last_err = if ok == 0 {
        unsafe { GetLastError() }
    } else {
        0
    };

    // Always close the file handle before attempting any cleanup removal.
    drop(file);

    if ok == 0 {
        let _ = std::fs::remove_file(dest);
        if cancel.load(Ordering::Relaxed)
            || last_err == ERROR_CANCELLED
            || last_err == HRESULT_ERROR_CANCELLED
        {
            return Err(PalError::Cancelled);
        }
        return Err(map_win32_error(last_err, Some(key), "MiniDumpWriteDump"));
    }

    if cancel.load(Ordering::Relaxed) {
        let _ = std::fs::remove_file(dest);
        return Err(PalError::Cancelled);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::mem::{offset_of, size_of};

    use super::*;

    #[test]
    fn abi_struct_layouts_match_windows_sdk() {
        // `WAITCHAIN_NODE_INFO`: 4 (type) + 4 (status) + 272 (LockObject: 256 + 8 + 4 + 4 pad) = 280 bytes.
        assert_eq!(size_of::<WaitChainNodeInfo>(), 280);

        // `MINIDUMP_CALLBACK_INPUT` uses `#include <pshpack4.h>`:
        // ProcessId at 0, ProcessHandle at 4, CallbackType at 4 + pointer size.
        assert_eq!(offset_of!(MiniDumpCallbackInputHeader, process_id), 0);
        assert_eq!(offset_of!(MiniDumpCallbackInputHeader, process_handle), 4);
        assert_eq!(
            offset_of!(MiniDumpCallbackInputHeader, callback_type),
            4 + size_of::<HANDLE>()
        );
    }

    #[test]
    fn wct_object_type_labels_cover_standard_types() {
        assert_eq!(
            wct_object_type_label(WCT_CRITICAL_SECTION_TYPE),
            "CriticalSection"
        );
        assert_eq!(wct_object_type_label(WCT_SEND_MESSAGE_TYPE), "SendMessage");
        assert_eq!(wct_object_type_label(WCT_MUTEX_TYPE), "Mutex");
        assert_eq!(wct_object_type_label(WCT_ALPC_TYPE), "Alpc");
        assert_eq!(wct_object_type_label(WCT_COM_TYPE), "Com");
        assert_eq!(wct_object_type_label(WCT_THREAD_WAIT_TYPE), "ThreadWait");
        assert_eq!(wct_object_type_label(WCT_PROCESS_WAIT_TYPE), "ProcessWait");
        assert_eq!(wct_object_type_label(WCT_THREAD_TYPE), "Thread");
        assert_eq!(wct_object_type_label(999), "Unknown");
    }
}
