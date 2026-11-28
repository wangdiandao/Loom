//! Cross-UIPI Windows single-instance lock and `WM_COPYDATA` activation wakeup.
//!
//! Uses `CreateMutexExW` requesting only `SYNCHRONIZATION` so a standard-privilege
//! secondary launch can still open an elevated primary instance's mutex without
//! `ERROR_ACCESS_DENIED`, and registers a dedicated hidden receiver window with
//! `ChangeWindowMessageFilterEx(WM_COPYDATA, MSGFLT_ALLOW)` to forward CLI
//! activation payloads (including `--loom-taskmgr-entry` handshakes) across UIPI.

use std::collections::HashSet;
use std::ffi::c_void;
use std::ptr;
use std::sync::{Arc, LazyLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use loom_pal_api::PalError;
use parking_lot::Mutex;
use windows_sys::Win32::Foundation::{
    GetLastError, HWND, LPARAM, LRESULT, WAIT_ABANDONED, WAIT_OBJECT_0, WPARAM,
};
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{
    CreateMutexExW, ReleaseMutex, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW,
    DestroyWindow, DispatchMessageW, FindWindowW, GWLP_USERDATA, GetMessageW, GetWindowLongPtrW,
    GetWindowThreadProcessId, MSG, MSGFLT_ALLOW, PostMessageW, PostQuitMessage, RegisterClassW,
    SMTO_ABORTIFHUNG, SMTO_BLOCK, SMTO_ERRORONEXIT, SendMessageTimeoutW, SetWindowLongPtrW,
    TranslateMessage, WM_CLOSE, WM_COPYDATA, WM_NCDESTROY, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::handle::{OwnedHandle, to_wide_null};

const COPYDATA_MAGIC: usize = 0x4C4F_4F4D; // "LOOM"
const MAX_PAYLOAD_BYTES: usize = 32 * 1024;
const MAX_FORWARD_ARGS: usize = 64;

/// Callback invoked when a secondary Loom instance forwards activation arguments.
pub type ActivationCallback = Arc<dyn Fn(Vec<String>) + Send + Sync + 'static>;

/// Tracks mutex IDs currently owned by this process to prevent Win32 same-thread mutex
/// reentrancy from falsely treating a second in-process `acquire` as an owner.
static OWNED_IDS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Owning guard for a cross-UIPI Windows single-instance lock and optional receiver thread.
pub struct WindowsSingleInstance {
    app_id: String,
    mutex: OwnedHandle,
    is_owner: bool,
    receiver_hwnd: isize,
    receiver_thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for WindowsSingleInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsSingleInstance")
            .field("is_owner", &self.is_owner)
            .finish()
    }
}

impl WindowsSingleInstance {
    /// Attempts to acquire the named mutex `app_id`.
    ///
    /// - If this process acquires ownership (`is_single() == true`), optionally starts a
    ///   lightweight hidden `WM_COPYDATA` receiver window that invokes `on_activate` whenever
    ///   a duplicate instance launches.
    /// - If another instance already owns the mutex (`is_single() == false`), forwards
    ///   `forward_args` to the existing instance's hidden window via `WM_COPYDATA` and grants
    ///   it foreground focus via `AllowSetForegroundWindow`.
    pub fn acquire(
        app_id: &str,
        forward_args: &[String],
        on_activate: Option<ActivationCallback>,
    ) -> Result<Self, PalError> {
        if app_id.trim().is_empty() {
            return Err(PalError::InvalidArgument(
                "single-instance app_id cannot be empty".into(),
            ));
        }

        let mutex_name = to_wide_null(&format!("Local\\{app_id}-sim"));
        // Request only `SYNCHRONIZATION` so a standard-privilege process can open
        // a mutex created by an elevated Loom instance without `ACCESS_DENIED`.
        // SAFETY: `mutex_name` is a valid NUL-terminated UTF-16 string.
        let raw_mutex = unsafe {
            CreateMutexExW(
                ptr::null(),
                mutex_name.as_ptr(),
                0,
                SYNCHRONIZATION_SYNCHRONIZE,
            )
        };
        let Some(mutex) = OwnedHandle::from_raw(raw_mutex) else {
            let code = unsafe { GetLastError() };
            return Err(PalError::OsError {
                code: code as i32,
                message: format!("CreateMutexExW failed for '{app_id}' (os error {code})"),
            });
        };

        let is_owner = {
            let mut owned = OWNED_IDS.lock();
            if owned.contains(app_id) {
                false
            } else if matches!(
                unsafe { WaitForSingleObject(mutex.as_raw(), 0) },
                WAIT_OBJECT_0 | WAIT_ABANDONED
            ) {
                owned.insert(app_id.to_owned());
                true
            } else {
                false
            }
        };

        let class_name = format!("{app_id}-sic");
        let title_name = format!("{app_id}-siw");

        if is_owner {
            let receiver = if let Some(cb) = on_activate {
                match spawn_receiver_thread(class_name, title_name, cb) {
                    Ok(res) => res,
                    Err(err) => {
                        OWNED_IDS.lock().remove(app_id);
                        unsafe {
                            let _ = ReleaseMutex(mutex.as_raw());
                        }
                        return Err(err);
                    }
                }
            } else {
                (0, None)
            };
            return Ok(Self {
                app_id: app_id.to_owned(),
                mutex,
                is_owner: true,
                receiver_hwnd: receiver.0,
                receiver_thread: receiver.1,
            });
        }

        // Duplicate instance: notify the running owner via `WM_COPYDATA` (wait briefly for
        // its receiver window if it just started).
        let _ = notify_existing_instance(&class_name, &title_name, forward_args);

        Ok(Self {
            app_id: app_id.to_owned(),
            mutex,
            is_owner: false,
            receiver_hwnd: 0,
            receiver_thread: None,
        })
    }

    /// Returns `true` if this guard owns the primary single-instance lock.
    #[inline]
    pub fn is_single(&self) -> bool {
        self.is_owner
    }
}

impl Drop for WindowsSingleInstance {
    fn drop(&mut self) {
        if self.receiver_hwnd != 0 {
            let hwnd = self.receiver_hwnd as HWND;
            // SAFETY: Posts `WM_CLOSE` to cleanly tear down our own hidden receiver window.
            unsafe {
                let _ = PostMessageW(hwnd, WM_CLOSE, 0, 0);
            }
        }
        if let Some(handle) = self.receiver_thread.take() {
            let _ = handle.join();
        }
        if self.is_owner {
            OWNED_IDS.lock().remove(&self.app_id);
            // SAFETY: This process owns `self.mutex`.
            unsafe {
                let _ = ReleaseMutex(self.mutex.as_raw());
            }
        }
    }
}

fn encode_args(args: &[String]) -> Option<Vec<u8>> {
    let joined = args.join("\x1f");
    let mut bytes = joined.into_bytes();
    bytes.push(0);
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return None;
    }
    Some(bytes)
}

fn decode_args(bytes: &[u8]) -> Option<Vec<String>> {
    if bytes.is_empty() || bytes.len() > MAX_PAYLOAD_BYTES || bytes.last() != Some(&0) {
        return None;
    }
    let text = std::str::from_utf8(&bytes[..bytes.len() - 1]).ok()?;
    if text.contains('\0') {
        return None;
    }
    if text.is_empty() {
        return Some(Vec::new());
    }
    let args: Vec<String> = text
        .split('\x1f')
        .take(MAX_FORWARD_ARGS + 1)
        .map(str::to_owned)
        .collect();
    if args.len() > MAX_FORWARD_ARGS {
        return None;
    }
    Some(args)
}

fn notify_existing_instance(class_name: &str, title_name: &str, args: &[String]) -> bool {
    let wide_class = to_wide_null(class_name);
    let wide_title = to_wide_null(title_name);
    let Some(payload) = encode_args(args) else {
        return false;
    };

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        // SAFETY: Locates the primary Loom instance's hidden activation window.
        let hwnd = unsafe { FindWindowW(wide_class.as_ptr(), wide_title.as_ptr()) };
        if !hwnd.is_null() {
            let mut owner_pid: u32 = 0;
            unsafe {
                GetWindowThreadProcessId(hwnd, &mut owner_pid);
                if owner_pid != 0 {
                    let _ = AllowSetForegroundWindow(owner_pid);
                }
            }
            let cds = COPYDATASTRUCT {
                dwData: COPYDATA_MAGIC,
                cbData: payload.len() as u32,
                lpData: payload.as_ptr() as *mut c_void,
            };
            let mut result: usize = 0;
            // SAFETY: `cds` and `payload` stay alive for the synchronous timeout call.
            let sent = unsafe {
                SendMessageTimeoutW(
                    hwnd,
                    WM_COPYDATA,
                    0,
                    (&cds as *const COPYDATASTRUCT) as LPARAM,
                    SMTO_ABORTIFHUNG | SMTO_BLOCK | SMTO_ERRORONEXIT,
                    2000,
                    &mut result,
                )
            };
            return sent != 0 && result == 1;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn spawn_receiver_thread(
    class_name: String,
    title_name: String,
    callback: ActivationCallback,
) -> Result<(isize, Option<JoinHandle<()>>), PalError> {
    let (tx, rx) = mpsc::sync_channel::<Result<isize, u32>>(1);
    let handle = thread::Builder::new()
        .name("loom-single-instance".into())
        .spawn(move || {
            let wide_class = to_wide_null(&class_name);
            let wide_title = to_wide_null(&title_name);
            // SAFETY: Gets the current executable module handle.
            let hinst = unsafe { GetModuleHandleW(ptr::null()) };
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(receiver_wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: ptr::null_mut(),
                hCursor: ptr::null_mut(),
                hbrBackground: ptr::null_mut(),
                lpszMenuName: ptr::null(),
                lpszClassName: wide_class.as_ptr(),
            };
            // SAFETY: Registers window class (ignoring `ERROR_CLASS_ALREADY_EXISTS`).
            unsafe {
                let _ = RegisterClassW(&wc);
            }

            // SAFETY: Creates a zero-size hidden top-level popup window to receive `WM_COPYDATA`.
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    wide_class.as_ptr(),
                    wide_title.as_ptr(),
                    WS_POPUP,
                    0,
                    0,
                    0,
                    0,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    hinst,
                    ptr::null(),
                )
            };
            if hwnd.is_null() {
                let code = unsafe { GetLastError() };
                let _ = tx.send(Err(code));
                return;
            }

            // Allow ONLY `WM_COPYDATA` through UIPI so standard-privilege launches can wake
            // an elevated Loom instance cleanly.
            let allowed = unsafe {
                ChangeWindowMessageFilterEx(hwnd, WM_COPYDATA, MSGFLT_ALLOW, ptr::null_mut())
            };
            if allowed == 0 {
                let code = unsafe { GetLastError() };
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                let _ = tx.send(Err(code));
                return;
            }

            let cb_box = Box::into_raw(Box::new(callback)) as isize;
            // SAFETY: Freed in `WM_NCDESTROY` before the window exits.
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, cb_box);
            }

            let _ = tx.send(Ok(hwnd as isize));

            let mut msg: MSG = unsafe { std::mem::zeroed() };
            // SAFETY: Standard Win32 message loop blocking with 0% CPU until `WM_CLOSE`.
            while unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) } > 0 {
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        })
        .map_err(PalError::Io)?;

    match rx.recv() {
        Ok(Ok(hwnd)) => Ok((hwnd, Some(handle))),
        Ok(Err(code)) => {
            let _ = handle.join();
            Err(PalError::OsError {
                code: code as i32,
                message: format!(
                    "failed to initialize single-instance receiver window (os error {code})"
                ),
            })
        }
        Err(_) => {
            let _ = handle.join();
            Err(PalError::OsError {
                code: -1,
                message: "single-instance receiver thread exited unexpectedly".into(),
            })
        }
    }
}

unsafe extern "system" fn receiver_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: `GWLP_USERDATA` holds our boxed `ActivationCallback` until `WM_NCDESTROY`.
    let raw_cb = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const ActivationCallback;
    match msg {
        WM_COPYDATA if !raw_cb.is_null() && lparam != 0 => {
            let cds = unsafe { &*(lparam as *const COPYDATASTRUCT) };
            if cds.dwData != COPYDATA_MAGIC
                || cds.lpData.is_null()
                || (cds.cbData as usize) > MAX_PAYLOAD_BYTES
            {
                return 0;
            }
            let bytes =
                unsafe { std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize) };
            if let Some(args) = decode_args(bytes) {
                let cb = unsafe { &*raw_cb };
                cb(args);
                return 1;
            }
            0
        }
        WM_CLOSE => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            0
        }
        WM_NCDESTROY => {
            if !raw_cb.is_null() {
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Box::from_raw(raw_cb as *mut ActivationCallback));
                }
            }
            unsafe {
                PostQuitMessage(0);
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_instance_forwards_activation_args_across_wm_copydata() {
        let id = format!(
            "loom-test-si-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let (tx, rx) = mpsc::channel();
        let primary = WindowsSingleInstance::acquire(
            &id,
            &[],
            Some(Arc::new(move |args| {
                let _ = tx.send(args);
            })),
        )
        .expect("acquire primary");
        assert!(primary.is_single());

        let forwarded = vec![
            "--loom-taskmgr-entry".to_owned(),
            "deadbeef01234567".to_owned(),
            "42".to_owned(),
            "999".to_owned(),
        ];
        let secondary =
            WindowsSingleInstance::acquire(&id, &forwarded, None).expect("acquire secondary");
        assert!(!secondary.is_single());

        let received = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("primary must receive forwarded args");
        assert_eq!(received, forwarded);
    }
}
