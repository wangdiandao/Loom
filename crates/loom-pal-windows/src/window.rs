//! Own-process Win32 main window z-order, minimized-state, and restore helpers.
//!
//! All window queries are strictly scoped to top-level visible windows owned by
//! the current process (`std::process::id()`) and read window titles exclusively
//! via non-blocking `InternalGetWindowText` (never cross-process `GetWindowTextW`).

use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GetWindow, GetWindowThreadProcessId, HWND_NOTOPMOST, HWND_TOPMOST,
    InternalGetWindowText, IsIconic, IsWindowVisible, SW_RESTORE, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SetForegroundWindow, SetWindowPos, ShowWindow,
};
use windows_sys::core::BOOL;

/// Applies native Windows Always-on-Top (`HWND_TOPMOST` / `HWND_NOTOPMOST`) to Loom's window.
pub fn set_main_window_always_on_top(enabled: bool) {
    if let Some(hwnd) = find_main_window_hwnd() {
        let insert_after = if enabled {
            HWND_TOPMOST
        } else {
            HWND_NOTOPMOST
        };
        // SAFETY: `hwnd` is a live top-level window belonging to the current process.
        unsafe {
            SetWindowPos(
                hwnd,
                insert_after,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

/// Returns `true` when Loom's native main window is currently minimized (`IsIconic`).
pub fn is_main_window_minimized() -> bool {
    find_main_window_hwnd().is_some_and(|hwnd| {
        // SAFETY: `hwnd` is owned by the current process.
        unsafe { IsIconic(hwnd) != 0 }
    })
}

/// Restores Loom's primary native window if minimized and brings it to foreground focus.
pub fn restore_and_focus_main_window() {
    if let Some(hwnd) = find_main_window_hwnd() {
        // SAFETY: `hwnd` is a top-level window owned by the current process.
        unsafe {
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            }
            SetForegroundWindow(hwnd);
        }
    }
}

/// Locates the current process's primary top-level visible Win32 window (`HWND`).
///
/// Restricts all checks to `std::process::id()` and inspects the title using
/// non-blocking `InternalGetWindowText` to uphold Loom's anti-hang red lines.
fn find_main_window_hwnd() -> Option<HWND> {
    struct SearchCtx {
        pid: u32,
        found: HWND,
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` points to the stack-local `SearchCtx` for the duration of `EnumWindows`.
        let ctx = unsafe { &mut *(lparam as *mut SearchCtx) };
        let mut win_pid: u32 = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut win_pid);
            if win_pid == ctx.pid
                && IsWindowVisible(hwnd) != 0
                && GetWindow(hwnd, GW_OWNER).is_null()
            {
                let mut title_probe = [0u16; 8];
                if InternalGetWindowText(hwnd, title_probe.as_mut_ptr(), title_probe.len() as i32)
                    > 0
                {
                    ctx.found = hwnd;
                    return 0; // stop enumeration
                }
            }
        }
        1
    }

    let mut ctx = SearchCtx {
        pid: std::process::id(),
        found: std::ptr::null_mut(),
    };
    // SAFETY: `enum_proc` only reads/writes `ctx` while `EnumWindows` executes synchronously.
    unsafe {
        EnumWindows(Some(enum_proc), (&mut ctx as *mut SearchCtx) as LPARAM);
    }

    if ctx.found.is_null() {
        None
    } else {
        Some(ctx.found)
    }
}
