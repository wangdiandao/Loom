//! Own-process Win32 main window z-order, minimized-state, and restore helpers.
//!
//! All window queries are strictly scoped to top-level visible windows owned by
//! the current process (`std::process::id()`) and read window titles exclusively
//! via non-blocking `InternalGetWindowText` (never cross-process `GetWindowTextW`).

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HWND, LPARAM};
use windows_sys::Win32::Graphics::Dwm::DwmGetColorizationColor;
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
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

/// Decodes a Windows DWM `AccentColor` registry `REG_DWORD` (`0xAABBGGRR`, ABGR) into `(r, g, b)`.
#[inline]
pub(crate) const fn decode_dwm_abgr(abgr: u32) -> (u8, u8, u8) {
    (
        (abgr & 0xFF) as u8,
        ((abgr >> 8) & 0xFF) as u8,
        ((abgr >> 16) & 0xFF) as u8,
    )
}

/// Decodes a `DwmGetColorizationColor` `0xAARRGGBB` (ARGB) value into `(r, g, b)`.
#[inline]
pub(crate) const fn decode_dwm_argb(argb: u32) -> (u8, u8, u8) {
    (
        ((argb >> 16) & 0xFF) as u8,
        ((argb >> 8) & 0xFF) as u8,
        (argb & 0xFF) as u8,
    )
}

/// Reads the active Windows system accent color as sRGB `(r, g, b)`.
///
/// Queries `HKCU\Software\Microsoft\Windows\DWM\AccentColor` (`REG_DWORD`, ABGR) first,
/// falling back to `DwmGetColorizationColor` (ARGB) if the registry value is unavailable.
pub fn system_accent_color() -> Option<(u8, u8, u8)> {
    // UTF-16 null-terminated "Software\Microsoft\Windows\DWM"
    const DWM_SUBKEY: &[u16] = &[
        b'S' as u16,
        b'o' as u16,
        b'f' as u16,
        b't' as u16,
        b'w' as u16,
        b'a' as u16,
        b'r' as u16,
        b'e' as u16,
        b'\\' as u16,
        b'M' as u16,
        b'i' as u16,
        b'c' as u16,
        b'r' as u16,
        b'o' as u16,
        b's' as u16,
        b'o' as u16,
        b'f' as u16,
        b't' as u16,
        b'\\' as u16,
        b'W' as u16,
        b'i' as u16,
        b'n' as u16,
        b'd' as u16,
        b'o' as u16,
        b'w' as u16,
        b's' as u16,
        b'\\' as u16,
        b'D' as u16,
        b'W' as u16,
        b'M' as u16,
        0,
    ];
    // UTF-16 null-terminated "AccentColor"
    const ACCENT_COLOR_VALUE: &[u16] = &[
        b'A' as u16,
        b'c' as u16,
        b'c' as u16,
        b'e' as u16,
        b'n' as u16,
        b't' as u16,
        b'C' as u16,
        b'o' as u16,
        b'l' as u16,
        b'o' as u16,
        b'r' as u16,
        0,
    ];

    let mut abgr: u32 = 0;
    let mut size: u32 = std::mem::size_of::<u32>() as u32;
    // SAFETY: `DWM_SUBKEY` and `ACCENT_COLOR_VALUE` are static null-terminated UTF-16 strings
    // and `abgr` has `size` bytes matching `RRF_RT_REG_DWORD`.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            DWM_SUBKEY.as_ptr(),
            ACCENT_COLOR_VALUE.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut abgr as *mut u32).cast(),
            &mut size,
        )
    };
    if status == ERROR_SUCCESS && size == std::mem::size_of::<u32>() as u32 {
        return Some(decode_dwm_abgr(abgr));
    }

    let mut argb: u32 = 0;
    let mut opaque: BOOL = 0;
    // SAFETY: Both output pointers are valid stack locals.
    let hr = unsafe { DwmGetColorizationColor(&mut argb, &mut opaque) };
    if hr >= 0 {
        return Some(decode_dwm_argb(argb));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_dwm_abgr_and_argb_byte_orders_accurately() {
        // Real Windows 11 DWM sample: AccentColor = 0xFFF4C51D, DwmGetColorizationColor = 0xC41DC5F4
        assert_eq!(decode_dwm_abgr(0xFFF4_C51D), (0x1D, 0xC5, 0xF4));
        assert_eq!(decode_dwm_argb(0xC41D_C5F4), (0x1D, 0xC5, 0xF4));
        assert_eq!(decode_dwm_abgr(0xFFD4_7800), (0x00, 0x78, 0xD4));
        assert_eq!(decode_dwm_argb(0xFF00_78D4), (0x00, 0x78, 0xD4));
    }

    #[test]
    fn system_accent_color_query_succeeds_without_panicking() {
        // On a desktop Windows session this returns Some((r, g, b)); even in minimal CI runners
        // it must complete quickly and cleanly without panicking.
        let _ = system_accent_color();
    }
}
