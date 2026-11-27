//! Windows global wakeup hotkey parser and zero-idle Win32 `RegisterHotKey` controller.
//!
//! A lightweight OS message-loop thread (`loom-hotkey`) is spawned **only** when a hotkey is
//! actively configured and terminated immediately via `PostThreadMessageW(WM_QUIT)` when the
//! hotkey is cleared or the controller is dropped, keeping Loom's OS thread budget lean.

use std::ptr;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use loom_pal_api::PalError;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey, UnregisterHotKey,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetMessageW, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, WM_HOTKEY, WM_QUIT,
};

use crate::handle::map_win32_error;

/// Stable per-thread hotkey identifier passed to `RegisterHotKey` / `UnregisterHotKey`.
const LOOM_WAKEUP_HOTKEY_ID: i32 = 0x4C4D; // 'LM'

/// Parses a human-readable hotkey specification (e.g. `"Ctrl+Shift+L"`, `"Alt+F12"`)
/// into Win32 `(fsModifiers, vk)` values.
///
/// Supported modifiers (case-insensitive): `Ctrl`/`Control`, `Alt`, `Shift`, `Win`/`Super`/`Meta`.
/// Supported primary keys (case-insensitive): `A`..`Z`, `0`..`9`, `F1`..`F12`.
/// At least one modifier and exactly one primary key are required.
pub fn parse_hotkey_spec(spec: &str) -> Result<(u32, u32), PalError> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err(PalError::InvalidArgument(
            "hotkey specification cannot be empty".into(),
        ));
    }

    let mut modifiers: u32 = 0;
    let mut vk: Option<u32> = None;

    for raw_part in trimmed.split('+') {
        let token = raw_part.trim();
        if token.is_empty() {
            return Err(PalError::InvalidArgument(format!(
                "invalid empty token in hotkey '{spec}'"
            )));
        }

        let upper = token.to_ascii_uppercase();
        match upper.as_str() {
            "CTRL" | "CONTROL" => modifiers |= MOD_CONTROL,
            "ALT" => modifiers |= MOD_ALT,
            "SHIFT" => modifiers |= MOD_SHIFT,
            "WIN" | "SUPER" | "META" => modifiers |= MOD_WIN,
            _ => {
                let parsed_vk = parse_primary_vk(&upper).ok_or_else(|| {
                    PalError::InvalidArgument(format!(
                        "unsupported hotkey token '{token}' in '{spec}'"
                    ))
                })?;
                if vk.replace(parsed_vk).is_some() {
                    return Err(PalError::InvalidArgument(format!(
                        "multiple primary keys specified in hotkey '{spec}'"
                    )));
                }
            }
        }
    }

    if modifiers == 0 {
        return Err(PalError::InvalidArgument(format!(
            "hotkey '{spec}' must include at least one modifier (Ctrl, Alt, Shift, or Win)"
        )));
    }

    let primary_vk = vk.ok_or_else(|| {
        PalError::InvalidArgument(format!(
            "hotkey '{spec}' must include a primary key (A-Z, 0-9, or F1-F12)"
        ))
    })?;

    Ok((modifiers, primary_vk))
}

fn parse_primary_vk(upper: &str) -> Option<u32> {
    let bytes = upper.as_bytes();
    if bytes.len() == 1 {
        let ch = bytes[0];
        if ch.is_ascii_uppercase() || ch.is_ascii_digit() {
            return Some(u32::from(ch));
        }
    }

    if let Some(rest) = upper.strip_prefix('F')
        && let Ok(num) = rest.parse::<u32>()
        && (1..=12).contains(&num)
    {
        // VK_F1 = 0x70 .. VK_F12 = 0x7B.
        return Some(0x70 + (num - 1));
    }

    None
}

struct HotkeyWorker {
    thread_id: u32,
    handle: JoinHandle<()>,
}

/// Manages a single system-wide Win32 global wakeup hotkey on a dedicated on-demand thread.
///
/// When no hotkey is configured (`None`), zero background threads are kept running.
pub struct WindowsGlobalHotkeyController {
    on_trigger: Arc<dyn Fn() + Send + Sync>,
    active_spec: Option<String>,
    worker: Option<HotkeyWorker>,
}

impl std::fmt::Debug for WindowsGlobalHotkeyController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsGlobalHotkeyController")
            .field("active_spec", &self.active_spec)
            .field("thread_running", &self.worker.is_some())
            .finish()
    }
}

impl WindowsGlobalHotkeyController {
    /// Creates a new controller in the disabled state (zero threads spawned).
    pub fn new(on_trigger: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            on_trigger,
            active_spec: None,
            worker: None,
        }
    }

    /// Returns the currently registered hotkey specification, if any.
    #[inline]
    pub fn active_hotkey(&self) -> Option<&str> {
        self.active_spec.as_deref()
    }

    /// Unregisters any active hotkey and shuts down the listener thread cleanly.
    pub fn clear(&mut self) {
        self.stop_worker();
        self.active_spec = None;
    }

    /// Registers `spec` as the active global wakeup hotkey, or clears the active hotkey
    /// when `spec` is `None` or blank.
    pub fn set_hotkey(&mut self, spec: Option<&str>) -> Result<(), PalError> {
        let normalized = spec.map(str::trim).filter(|s| !s.is_empty());
        let Some(next_spec) = normalized else {
            self.clear();
            return Ok(());
        };

        let (modifiers, vk) = parse_hotkey_spec(next_spec)?;

        if self
            .active_spec
            .as_deref()
            .is_some_and(|cur| cur.eq_ignore_ascii_case(next_spec))
            && self.worker.is_some()
        {
            return Ok(());
        }

        // Stop previous listener before registering the new shortcut so identical combos never collide.
        self.clear();

        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<u32, PalError>>(1);
        let callback = Arc::clone(&self.on_trigger);

        let handle = thread::Builder::new()
            .name("loom-hotkey".to_owned())
            .spawn(move || {
                hotkey_thread_main(modifiers | MOD_NOREPEAT, vk, callback, ready_tx);
            })
            .map_err(PalError::Io)?;

        match ready_rx.recv() {
            Ok(Ok(thread_id)) => {
                self.worker = Some(HotkeyWorker { thread_id, handle });
                self.active_spec = Some(next_spec.to_owned());
                Ok(())
            }
            Ok(Err(err)) => {
                let _ = handle.join();
                Err(err)
            }
            Err(_) => {
                let _ = handle.join();
                Err(PalError::OsError {
                    code: -1,
                    message: "hotkey worker exited before reporting registration status".into(),
                })
            }
        }
    }

    fn stop_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            // SAFETY: `worker.thread_id` is the OS thread ID of our live message-loop thread.
            unsafe {
                PostThreadMessageW(worker.thread_id, WM_QUIT, 0, 0);
            }
            let _ = worker.handle.join();
        }
    }
}

impl Drop for WindowsGlobalHotkeyController {
    fn drop(&mut self) {
        self.clear();
    }
}

fn hotkey_thread_main(
    modifiers: u32,
    vk: u32,
    on_trigger: Arc<dyn Fn() + Send + Sync>,
    ready_tx: mpsc::SyncSender<Result<u32, PalError>>,
) {
    // Force Windows to create a message queue for this thread before returning its thread ID.
    let mut msg = MSG::default();
    // SAFETY: `PeekMessageW` with `PM_NOREMOVE` initializes the calling thread's message queue.
    unsafe {
        PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_NOREMOVE);
    }

    // SAFETY: Registers thread-scoped hotkey (`hwnd = NULL`) with `LOOM_WAKEUP_HOTKEY_ID`.
    let registered =
        unsafe { RegisterHotKey(ptr::null_mut(), LOOM_WAKEUP_HOTKEY_ID, modifiers, vk) };
    if registered == 0 {
        // SAFETY: Reads thread-local Win32 error set by `RegisterHotKey`.
        let err = unsafe { GetLastError() };
        let _ = ready_tx.send(Err(map_win32_error(err, None, "RegisterHotKey")));
        return;
    }

    // SAFETY: Always safe to read current Win32 thread ID.
    let thread_id = unsafe { GetCurrentThreadId() };
    if ready_tx.send(Ok(thread_id)).is_err() {
        // SAFETY: Unregisters hotkey owned by this thread before exiting.
        unsafe {
            UnregisterHotKey(ptr::null_mut(), LOOM_WAKEUP_HOTKEY_ID);
        }
        return;
    }

    // Blocking Win32 message loop: consumes 0% CPU while idle, exits immediately on `WM_QUIT`.
    loop {
        // SAFETY: Standard Win32 `GetMessageW` loop on this thread's message queue.
        let status = unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) };
        if status <= 0 {
            break;
        }
        if msg.message == WM_HOTKEY && msg.wParam == LOOM_WAKEUP_HOTKEY_ID as usize {
            on_trigger();
        }
    }

    // SAFETY: Unregisters the thread-owned hotkey cleanly before thread exit.
    unsafe {
        UnregisterHotKey(ptr::null_mut(), LOOM_WAKEUP_HOTKEY_ID);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hotkey_spec_handles_valid_specs_case_insensitively() {
        assert_eq!(
            parse_hotkey_spec("Ctrl+Shift+L").expect("Ctrl+Shift+L"),
            (MOD_CONTROL | MOD_SHIFT, u32::from(b'L'))
        );
        assert_eq!(
            parse_hotkey_spec("  ctrl + alt + l ").expect("ctrl+alt+l"),
            (MOD_CONTROL | MOD_ALT, u32::from(b'L'))
        );
        assert_eq!(
            parse_hotkey_spec("Control+Shift+M").expect("Control+Shift+M"),
            (MOD_CONTROL | MOD_SHIFT, u32::from(b'M'))
        );
        assert_eq!(
            parse_hotkey_spec("Alt+Shift+T").expect("Alt+Shift+T"),
            (MOD_ALT | MOD_SHIFT, u32::from(b'T'))
        );
        assert_eq!(
            parse_hotkey_spec("Win+9").expect("Win+9"),
            (MOD_WIN, u32::from(b'9'))
        );
        assert_eq!(
            parse_hotkey_spec("Ctrl+F1").expect("Ctrl+F1"),
            (MOD_CONTROL, 0x70)
        );
        assert_eq!(
            parse_hotkey_spec("Ctrl+Alt+F12").expect("Ctrl+Alt+F12"),
            (MOD_CONTROL | MOD_ALT, 0x7B)
        );
    }

    #[test]
    fn parse_hotkey_spec_rejects_invalid_specs() {
        for invalid in [
            "",
            "   ",
            "L",
            "F12",
            "Ctrl",
            "Ctrl+Shift",
            "Ctrl++L",
            "Ctrl+L+M",
            "Ctrl+F0",
            "Ctrl+F13",
            "Ctrl+Space",
            "SuperDuper+L",
        ] {
            let err = parse_hotkey_spec(invalid)
                .expect_err(&format!("expected invalid spec '{invalid}' to fail"));
            assert!(
                matches!(err, PalError::InvalidArgument(_)),
                "unexpected error for '{invalid}': {err:?}"
            );
        }
    }

    #[test]
    fn controller_lifecycle_stays_idle_when_disabled_and_cleans_up_on_clear() {
        let mut ctrl = WindowsGlobalHotkeyController::new(Arc::new(|| {}));
        assert!(ctrl.active_hotkey().is_none());
        assert!(ctrl.worker.is_none());

        // Setting `None` or blank keeps zero threads spawned.
        ctrl.set_hotkey(None).expect("set None");
        ctrl.set_hotkey(Some("   ")).expect("set blank");
        assert!(ctrl.worker.is_none());

        // Invalid spec fails without spawning any thread.
        assert!(ctrl.set_hotkey(Some("NotAKey")).is_err());
        assert!(ctrl.worker.is_none());

        // Clearing is idempotent.
        ctrl.clear();
        assert!(ctrl.active_hotkey().is_none());
        assert!(ctrl.worker.is_none());
    }
}
