//! Windows Terminal Services (WTS) user session enumeration and disconnect manager.
//!
//! Uses `WTSEnumerateSessionsW`, `WTSQuerySessionInformationW`, and `WTSDisconnectSession`
//! with RAII `WTSFreeMemory` guards and maps `ERROR_ACCESS_DENIED` to
//! [`PalError::ElevationRequired`].

use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;

use loom_pal_api::{PalError, SessionManager};
use loom_types::{UserSessionRecord, UserSessionState};
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::RemoteDesktop::{
    WTS_CONNECTSTATE_CLASS, WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW, WTSActive,
    WTSConnectQuery, WTSConnectState, WTSConnected, WTSDisconnectSession, WTSDisconnected,
    WTSDomainName, WTSEnumerateSessionsW, WTSFreeMemory, WTSIdle, WTSQuerySessionInformationW,
    WTSShadow, WTSUserName,
};

use crate::handle::map_win32_error;

/// RAII guard for heap buffers allocated by WTS APIs (`WTSFreeMemory` on drop).
struct WtsMemGuard(*mut c_void);

impl Drop for WtsMemGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` was allocated by a WTS API and is freed exactly once.
            unsafe {
                WTSFreeMemory(self.0);
            }
        }
    }
}

/// Windows WTS implementation of [`SessionManager`].
#[derive(Debug, Default)]
pub struct WindowsSessionManager;

impl WindowsSessionManager {
    /// Creates a new Windows WTS user session manager.
    pub fn new() -> Self {
        Self
    }
}

impl SessionManager for WindowsSessionManager {
    fn list_sessions(&self) -> Result<Vec<UserSessionRecord>, PalError> {
        let mut raw_sessions: *mut WTS_SESSION_INFOW = ptr::null_mut();
        let mut count: u32 = 0;

        // SAFETY: Enumerates sessions on the local host; allocated memory is guarded below.
        let ok = unsafe {
            WTSEnumerateSessionsW(
                WTS_CURRENT_SERVER_HANDLE,
                0,
                1,
                &mut raw_sessions,
                &mut count,
            )
        };
        if ok == 0 {
            // SAFETY: Reads thread-local error set by `WTSEnumerateSessionsW`.
            let err = unsafe { GetLastError() };
            return Err(map_win32_error(err, None, "WTSEnumerateSessionsW"));
        }

        let _guard = WtsMemGuard(raw_sessions.cast::<c_void>());
        if raw_sessions.is_null() || count == 0 {
            return Ok(Vec::new());
        }

        // SAFETY: `WTSEnumerateSessionsW` returned `count` initialized `WTS_SESSION_INFOW` entries.
        let infos = unsafe { std::slice::from_raw_parts(raw_sessions, count as usize) };
        let mut sessions = Vec::with_capacity(infos.len());

        for info in infos {
            let session_id = info.SessionId;
            let Some(user) = query_wts_string(session_id, WTSUserName) else {
                // Skip Session 0 (Services), RDP-Tcp listener rows, and sessions without a user.
                continue;
            };
            if user.is_empty() {
                continue;
            }

            let domain = query_wts_string(session_id, WTSDomainName).unwrap_or_default();
            let user_name: Arc<str> = if domain.is_empty() {
                Arc::from(user)
            } else {
                Arc::from(format!(r"{domain}\{user}"))
            };

            let raw_state = query_wts_connect_state(session_id).unwrap_or(info.State);
            let state = map_wts_connect_state(raw_state);

            sessions.push(UserSessionRecord {
                session_id,
                user_name,
                state,
                process_count: 0,
                cpu_permille: 0,
                memory_bytes: 0,
                disk_bytes_per_sec: 0,
                net_bytes_per_sec: 0,
                processes: Vec::new(),
            });
        }

        sessions.sort_by_key(|s| s.session_id);
        Ok(sessions)
    }

    fn disconnect_session(&self, session_id: u32) -> Result<(), PalError> {
        // SAFETY: Disconnects the specified session on the local host without blocking (`bWait = 0`).
        let ok = unsafe { WTSDisconnectSession(WTS_CURRENT_SERVER_HANDLE, session_id, 0) };
        if ok == 0 {
            // SAFETY: Reads thread-local error set by `WTSDisconnectSession`.
            let err = unsafe { GetLastError() };
            return Err(map_win32_error(err, None, "WTSDisconnectSession"));
        }
        Ok(())
    }
}

fn query_wts_string(session_id: u32, info_class: i32) -> Option<String> {
    let mut raw_ptr: *mut u16 = ptr::null_mut();
    let mut bytes_returned: u32 = 0;

    // SAFETY: Queries a NUL-terminated UTF-16 WTS property; buffer freed by `WtsMemGuard`.
    let ok = unsafe {
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            session_id,
            info_class,
            &mut raw_ptr,
            &mut bytes_returned,
        )
    };
    if ok == 0 || raw_ptr.is_null() {
        return None;
    }
    let _guard = WtsMemGuard(raw_ptr.cast::<c_void>());

    let u16_len = (bytes_returned as usize) / 2;
    if u16_len == 0 {
        return None;
    }
    // SAFETY: `raw_ptr` is readable for `u16_len` UTF-16 code units.
    let slice = unsafe { std::slice::from_raw_parts(raw_ptr, u16_len) };
    let nul_pos = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    let decoded = String::from_utf16_lossy(&slice[..nul_pos]);
    let trimmed = decoded.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn query_wts_connect_state(session_id: u32) -> Option<WTS_CONNECTSTATE_CLASS> {
    let mut raw_ptr: *mut u16 = ptr::null_mut();
    let mut bytes_returned: u32 = 0;

    // SAFETY: Queries `WTSConnectState` (`WTS_CONNECTSTATE_CLASS` / `i32`); buffer freed by guard.
    let ok = unsafe {
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            session_id,
            WTSConnectState,
            &mut raw_ptr,
            &mut bytes_returned,
        )
    };
    if ok == 0 || raw_ptr.is_null() || (bytes_returned as usize) < std::mem::size_of::<i32>() {
        return None;
    }
    let _guard = WtsMemGuard(raw_ptr.cast::<c_void>());
    // SAFETY: `bytes_returned >= size_of::<i32>()`.
    let state = unsafe { ptr::read_unaligned(raw_ptr.cast::<WTS_CONNECTSTATE_CLASS>()) };
    Some(state)
}

/// Maps a Windows `WTS_CONNECTSTATE_CLASS` into [`UserSessionState`].
#[inline]
#[allow(non_upper_case_globals)]
pub(crate) fn map_wts_connect_state(state: WTS_CONNECTSTATE_CLASS) -> UserSessionState {
    match state {
        WTSActive | WTSConnected | WTSConnectQuery | WTSShadow => UserSessionState::Active,
        WTSDisconnected => UserSessionState::Disconnected,
        WTSIdle => UserSessionState::Idle,
        _ => UserSessionState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::System::RemoteDesktop::{WTSDown, WTSInit, WTSListen};

    use super::*;

    #[test]
    fn map_wts_connect_state_maps_expected_variants() {
        assert_eq!(map_wts_connect_state(WTSActive), UserSessionState::Active);
        assert_eq!(
            map_wts_connect_state(WTSConnected),
            UserSessionState::Active
        );
        assert_eq!(
            map_wts_connect_state(WTSDisconnected),
            UserSessionState::Disconnected
        );
        assert_eq!(map_wts_connect_state(WTSIdle), UserSessionState::Idle);
        assert_eq!(map_wts_connect_state(WTSListen), UserSessionState::Unknown);
        assert_eq!(map_wts_connect_state(WTSDown), UserSessionState::Unknown);
        assert_eq!(map_wts_connect_state(WTSInit), UserSessionState::Unknown);
    }
}
