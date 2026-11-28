//! Non-blocking top-level window enumeration and zero-allocation UTF-16 scratch encoding.
//!
//! Strictly avoids cross-process `GetWindowTextW` and blocking `SendMessageW`; uses only
//! `InternalGetWindowText`, `IsHungAppWindow`, and `DwmGetWindowAttribute(DWMWA_CLOAKED)`.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;

use loom_pal_api::ProcessSampleScratch;
use windows_sys::Win32::Foundation::{HWND, LPARAM, TRUE};
use windows_sys::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongW, GetWindowThreadProcessId,
    InternalGetWindowText, IsHungAppWindow, IsWindowVisible, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};
use windows_sys::core::BOOL;

/// Maximum UTF-16 code units captured per top-level window title.
pub(super) const MAX_WINDOW_TITLE_CHARS: usize = 256;

/// Compact per-PID top-level window summary stored in a reusable map.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct WindowSummary {
    pub(super) title_offset: u32,
    pub(super) title_len: u16,
    pub(super) is_hung: bool,
}

/// Reusable state passed into the `EnumWindows` callback without heap allocations.
struct WindowEnumState<'a> {
    map: &'a mut HashMap<u32, WindowSummary>,
    titles: &'a mut String,
    utf16_buf: &'a mut Vec<u16>,
}

/// Runs a non-blocking `EnumWindows` pass using exclusively `InternalGetWindowText`,
/// `IsHungAppWindow`, and `DwmGetWindowAttribute(DWMWA_CLOAKED)`.
pub(super) fn collect_visible_windows(
    window_map: &mut HashMap<u32, WindowSummary>,
    window_titles: &mut String,
    utf16_title_buf: &mut Vec<u16>,
) {
    window_map.clear();
    window_titles.clear();

    let mut state = WindowEnumState {
        map: window_map,
        titles: window_titles,
        utf16_buf: utf16_title_buf,
    };

    // SAFETY: `state` lives for the entire synchronous duration of `EnumWindows`.
    unsafe {
        let _ = EnumWindows(
            Some(enum_window_proc),
            (&mut state as *mut WindowEnumState<'_>) as LPARAM,
        );
    }
}

/// Non-blocking `EnumWindows` callback.
///
/// Strictly avoids `GetWindowTextW` and `SendMessageW`; uses only `InternalGetWindowText`,
/// `IsHungAppWindow`, and `DwmGetWindowAttribute(DWMWA_CLOAKED)`.
unsafe extern "system" fn enum_window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    if hwnd.is_null() || lparam == 0 {
        return TRUE;
    }

    // SAFETY: All Win32 queries below are non-blocking kernel/DWM reads and `lparam`
    // points to our stack-resident `WindowEnumState` for the synchronous `EnumWindows` call.
    unsafe {
        if IsWindowVisible(hwnd) == 0 {
            return TRUE;
        }

        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if (ex_style & WS_EX_TOOLWINDOW) != 0 {
            return TRUE;
        }

        let owner = GetWindow(hwnd, GW_OWNER);
        if !owner.is_null() && (ex_style & WS_EX_APPWINDOW) == 0 {
            return TRUE;
        }

        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 {
            return TRUE;
        }

        let state = &mut *(lparam as *mut WindowEnumState<'_>);

        // Read title directly from kernel window struct (non-blocking) BEFORE DWM IPC so
        // titleless helper windows never incur a DwmGetWindowAttribute roundtrip.
        let written = InternalGetWindowText(
            hwnd,
            state.utf16_buf.as_mut_ptr(),
            state.utf16_buf.len() as i32,
        );
        if written <= 0 {
            return TRUE;
        }
        let wide_len = (written as usize).min(state.utf16_buf.len());

        // Check DWM cloaking to ignore hidden UWP / virtual-desktop windows.
        let mut cloaked: u32 = 0;
        let hr = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED as u32,
            (&mut cloaked as *mut u32).cast::<c_void>(),
            size_of::<u32>() as u32,
        );
        if hr == 0 && cloaked != 0 {
            return TRUE;
        }

        let is_hung = IsHungAppWindow(hwnd) != 0;

        if let Some(existing) = state.map.get_mut(&pid) {
            existing.is_hung |= is_hung;
            if existing.title_len == 0 && wide_len > 0 {
                let (off, len) = push_utf16_into_string(state.titles, &state.utf16_buf[..wide_len]);
                existing.title_offset = off;
                existing.title_len = len;
            }
        } else {
            let (title_offset, title_len) =
                push_utf16_into_string(state.titles, &state.utf16_buf[..wide_len]);
            state.map.insert(
                pid,
                WindowSummary {
                    title_offset,
                    title_len,
                    is_hung,
                },
            );
        }
    }

    TRUE
}

/// Decodes a UTF-16 slice directly into `scratch.string_buf` without intermediate allocation.
pub(super) fn push_utf16_into_scratch<Ext>(
    scratch: &mut ProcessSampleScratch<Ext>,
    wide: &[u16],
) -> (u32, u16) {
    push_utf16_into_string(&mut scratch.string_buf, wide)
}

/// Decodes a UTF-16 slice directly into a reusable UTF-8 `String` arena.
fn push_utf16_into_string(arena: &mut String, wide: &[u16]) -> (u32, u16) {
    if wide.is_empty() {
        return (0, 0);
    }
    let Ok(start) = u32::try_from(arena.len()) else {
        return (0, 0);
    };
    let start_usize = start as usize;
    let max_bytes = usize::from(u16::MAX);

    // Fast path for ASCII image names (avoids UTF-16 surrogate decoding overhead).
    let mut idx = 0;
    while idx < wide.len() {
        let code = wide[idx];
        if code == 0 {
            let written = arena.len() - start_usize;
            return if written == 0 {
                (0, 0)
            } else {
                (start, written as u16)
            };
        }
        if code >= 0x80 {
            break;
        }
        if arena.len() - start_usize >= max_bytes {
            return (start, (arena.len() - start_usize) as u16);
        }
        arena.push(code as u8 as char);
        idx += 1;
    }

    if idx < wide.len() {
        for decoded in char::decode_utf16(wide[idx..].iter().copied()) {
            let ch = decoded.unwrap_or(char::REPLACEMENT_CHARACTER);
            if ch == '\0' {
                break;
            }
            let next_len = arena.len() - start_usize + ch.len_utf8();
            if next_len > max_bytes {
                break;
            }
            arena.push(ch);
        }
    }

    let written = arena.len() - start_usize;
    if written == 0 {
        (0, 0)
    } else {
        (start, written as u16)
    }
}

/// Resolves an `(offset, len)` slice inside a string arena.
#[inline]
pub(super) fn slice_str(arena: &str, offset: u32, len: u16) -> &str {
    if len == 0 {
        return "";
    }
    let start = offset as usize;
    let Some(end) = start.checked_add(usize::from(len)) else {
        return "";
    };
    arena.get(start..end).unwrap_or("")
}
