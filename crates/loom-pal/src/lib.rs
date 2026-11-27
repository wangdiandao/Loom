//! Compile-time static Platform Abstraction Layer (PAL) facade for Loom.
//!
//! Exports [`NativeBackend`] via `#[cfg(target_os)]` type aliases and provides
//! lightweight cross-platform desktop lifecycle helpers such as
//! [`SingleInstanceGuard`] so upper layers remain free of OS-specific
//! conditional compilation and dynamic trait-object overhead.

#![warn(missing_docs)]

use std::sync::Arc;

pub use loom_pal_api::*;

/// Active compile-time native backend on Windows.
#[cfg(windows)]
pub type NativeBackend = loom_pal_windows::WindowsBackend;

/// Active compile-time native backend on macOS.
#[cfg(target_os = "macos")]
pub type NativeBackend = loom_pal_macos::MacosBackend;

/// Active compile-time native backend on Linux.
#[cfg(target_os = "linux")]
pub type NativeBackend = loom_pal_linux::LinuxBackend;

/// Cross-platform static facade for host application window operations.
///
/// Delegates directly to [`NativeBackend`] without introducing `#[cfg(target_os)]`
/// branches or dynamic dispatch.
pub mod window {
    use crate::{NativeBackend, PlatformBackend};

    /// Toggles native always-on-top z-order for Loom's primary application window.
    #[inline]
    pub fn set_main_window_always_on_top(enabled: bool) {
        NativeBackend::set_main_window_always_on_top(enabled);
    }

    /// Returns `true` when Loom's primary native window is currently minimized.
    #[inline]
    pub fn is_main_window_minimized() -> bool {
        NativeBackend::is_main_window_minimized()
    }

    /// Restores Loom's primary native window (if minimized) and brings it to foreground focus.
    #[inline]
    pub fn restore_and_focus_main_window() {
        NativeBackend::restore_and_focus_main_window();
    }
}

/// Cross-platform single-instance application lock guard.
///
/// Wraps OS-native named mutex / domain-socket / flock primitives via `single-instance`
/// so `loom-app` can detect an already-running Loom instance without platform `#[cfg]` code.
/// The underlying OS lock is automatically released when this guard is dropped.
pub struct SingleInstanceGuard {
    inner: single_instance::SingleInstance,
}

impl std::fmt::Debug for SingleInstanceGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SingleInstanceGuard")
            .field("is_single", &self.is_single())
            .finish()
    }
}

impl SingleInstanceGuard {
    /// Attempts to acquire the OS-level single-instance lock identified by `app_id`.
    ///
    /// Call [`Self::is_single`] on the returned guard to determine whether this
    /// process is the primary instance (`true`) or a duplicate launch (`false`).
    pub fn acquire(app_id: &str) -> Result<Self, PalError> {
        if app_id.trim().is_empty() {
            return Err(PalError::InvalidArgument(
                "single-instance app_id cannot be empty".into(),
            ));
        }

        let inner =
            single_instance::SingleInstance::new(app_id).map_err(|err| PalError::OsError {
                code: -1,
                message: format!("failed to create single-instance lock '{app_id}': {err}"),
            })?;

        Ok(Self { inner })
    }

    /// Returns `true` if this guard owns the primary single-instance lock.
    #[inline]
    pub fn is_single(&self) -> bool {
        self.inner.is_single()
    }
}

/// Cross-platform global wakeup hotkey controller.
///
/// On Windows, wraps [`loom_pal_windows::WindowsGlobalHotkeyController`] backed by Win32
/// `RegisterHotKey`/`UnregisterHotKey` and spawns a lightweight listener thread **only**
/// while a hotkey is actively configured. On Linux and macOS, acts as a zero-thread safe
/// no-op (`Ok(())` when disabled, [`PalError::Unsupported`] when attempting to register).
#[derive(Debug)]
pub struct GlobalHotkeyController {
    #[cfg(windows)]
    inner: loom_pal_windows::WindowsGlobalHotkeyController,
    #[cfg(not(windows))]
    active_spec: Option<String>,
}

impl GlobalHotkeyController {
    /// Creates a new controller in the disabled state that invokes `on_trigger` whenever
    /// the registered global wakeup hotkey fires.
    pub fn new(on_trigger: Arc<dyn Fn() + Send + Sync>) -> Self {
        #[cfg(windows)]
        {
            Self {
                inner: loom_pal_windows::WindowsGlobalHotkeyController::new(on_trigger),
            }
        }
        #[cfg(not(windows))]
        {
            let _ = on_trigger;
            Self { active_spec: None }
        }
    }

    /// Returns the currently registered hotkey specification, if any.
    #[inline]
    pub fn active_hotkey(&self) -> Option<&str> {
        #[cfg(windows)]
        {
            self.inner.active_hotkey()
        }
        #[cfg(not(windows))]
        {
            self.active_spec.as_deref()
        }
    }

    /// Registers `spec` (e.g., `"Ctrl+Shift+L"`) as the active global wakeup hotkey, or
    /// unregisters and shuts down the listener thread when `spec` is `None` or blank.
    pub fn set_hotkey(&mut self, spec: Option<&str>) -> Result<(), PalError> {
        #[cfg(windows)]
        {
            self.inner.set_hotkey(spec)
        }
        #[cfg(not(windows))]
        {
            let normalized = spec.map(str::trim).filter(|s| !s.is_empty());
            if normalized.is_none() {
                self.active_spec = None;
                return Ok(());
            }
            Err(PalError::Unsupported(
                "global wakeup hotkey is not supported on this platform",
            ))
        }
    }

    /// Unregisters any active global wakeup hotkey and releases associated OS resources.
    pub fn clear(&mut self) {
        #[cfg(windows)]
        {
            self.inner.clear();
        }
        #[cfg(not(windows))]
        {
            self.active_spec = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_instance_guard_detects_duplicate_and_releases_on_drop() {
        let unique_id = format!("loom-pal-single-instance-test-{}", std::process::id());

        {
            let primary = SingleInstanceGuard::acquire(&unique_id).expect("acquire primary");
            assert!(primary.is_single());

            let secondary = SingleInstanceGuard::acquire(&unique_id).expect("acquire secondary");
            assert!(!secondary.is_single());
        }

        // After dropping both guards, a fresh acquisition becomes the primary instance again.
        let reacquired = SingleInstanceGuard::acquire(&unique_id).expect("reacquire after drop");
        assert!(reacquired.is_single());
    }

    #[test]
    fn single_instance_guard_rejects_empty_id() {
        let err = SingleInstanceGuard::acquire("   ").expect_err("empty app_id must error");
        assert!(matches!(err, PalError::InvalidArgument(_)));
    }

    #[test]
    fn native_backend_exposes_platform_profile_and_window_helpers() {
        assert_ne!(NativeBackend::PLATFORM, Default::default());
        assert!(!NativeBackend::PLATFORM.os_name.is_empty());

        // Window host helpers must be safe to call in headless test processes without a main window.
        assert!(!window::is_main_window_minimized());
        window::set_main_window_always_on_top(false);
        window::restore_and_focus_main_window();
    }
}
