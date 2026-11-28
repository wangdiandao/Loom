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

/// Cross-platform static facade for OS Task Manager replacement CLI & handshake integration.
///
/// On Windows, delegates directly to `loom_pal_windows::taskmgr` without exposing
/// `#[cfg(windows)]` branches in upper crates. On other platforms, acts as a zero-overhead no-op.
pub mod taskmgr {
    /// Validated readiness handshake completion token returned when Loom is launched or
    /// awakened via the OS Task Manager replacement entry.
    pub struct HandshakeCompletion {
        #[cfg(windows)]
        inner: loom_pal_windows::taskmgr::ready::Completion,
    }

    impl HandshakeCompletion {
        /// Returns `true` if the launcher is still waiting and the window should be shown/focused.
        #[inline]
        pub fn should_show(&self) -> bool {
            #[cfg(windows)]
            {
                self.inner.should_show()
            }
            #[cfg(not(windows))]
            {
                false
            }
        }

        /// Signals the launcher that Loom's UI is visible and ready.
        #[inline]
        pub fn acknowledge(self) {
            #[cfg(windows)]
            {
                self.inner.acknowledge();
            }
        }
    }

    /// Inspects raw CLI arguments (`std::env::args().skip(1)`) for early Task Manager
    /// replacement launcher / probe / maintenance modes before GPUI initialization.
    ///
    /// Returns `Some(exit_code)` if the process handled a special mode and must exit immediately.
    #[inline]
    pub fn try_handle_cli_mode(args: &[String]) -> Option<i32> {
        #[cfg(windows)]
        {
            loom_pal_windows::taskmgr::try_handle_cli_mode(args)
        }
        #[cfg(not(windows))]
        {
            let _ = args;
            None
        }
    }

    /// Parses and queues any `--loom-taskmgr-entry` readiness handshake arguments.
    #[inline]
    pub fn queue_handshake_args(args: &[String]) -> bool {
        #[cfg(windows)]
        {
            loom_pal_windows::taskmgr::ready::queue_args(args)
        }
        #[cfg(not(windows))]
        {
            let _ = args;
            false
        }
    }

    /// Validates and completes all pending readiness handshakes, returning tokens to acknowledge
    /// once the main window is shown and focused.
    #[inline]
    pub fn complete_pending_handshakes() -> Vec<HandshakeCompletion> {
        #[cfg(windows)]
        {
            loom_pal_windows::taskmgr::ready::complete_pending()
                .into_iter()
                .map(|inner| HandshakeCompletion { inner })
                .collect()
        }
        #[cfg(not(windows))]
        {
            Vec::new()
        }
    }
}

/// Cross-platform static facade for persistent administrator startup & privilege transitions.
///
/// On Windows, delegates directly to `loom_pal_windows::admin` without exposing `#[cfg(windows)]`
/// branches in upper crates. On other platforms, acts as a zero-overhead no-op.
pub mod admin {
    use crate::PalError;

    /// Prepared privilege-switch handoff whose broker is already running and waiting for commit.
    pub struct PreparedSwitch {
        #[cfg(windows)]
        inner: loom_pal_windows::admin::PreparedSwitch,
    }

    impl PreparedSwitch {
        /// Commits the privilege switch so the broker waits for this process to exit and launches
        /// the replacement Loom instance.
        #[inline]
        pub fn commit(self) -> Result<(), PalError> {
            #[cfg(windows)]
            {
                self.inner
                    .commit()
                    .map_err(|message| PalError::OsError { code: -1, message })
            }
            #[cfg(not(windows))]
            {
                Err(PalError::Unsupported(
                    "persistent administrator startup is not supported on this platform",
                ))
            }
        }
    }

    /// Inspects raw CLI arguments (`std::env::args().skip(1)`) for early administrator broker
    /// or maintenance modes before GPUI initialization.
    #[inline]
    pub fn try_handle_cli_mode(args: &[String]) -> Option<i32> {
        #[cfg(windows)]
        {
            loom_pal_windows::admin::try_handle_cli_mode(args)
        }
        #[cfg(not(windows))]
        {
            let _ = args;
            None
        }
    }

    /// Routes a non-elevated cold launch through the registered scheduled task when persistent
    /// administrator startup is active. Returns `true` if the elevated broker accepted the handoff
    /// and this process should exit immediately.
    #[inline]
    pub fn route(args: &[String]) -> bool {
        #[cfg(windows)]
        {
            loom_pal_windows::admin::route(args)
        }
        #[cfg(not(windows))]
        {
            let _ = args;
            false
        }
    }

    /// Signals the privilege-switch broker's `done` event once the UI window is created and live.
    #[inline]
    pub fn acknowledge(args: &[String]) {
        #[cfg(windows)]
        {
            loom_pal_windows::admin::acknowledge(args);
        }
        #[cfg(not(windows))]
        {
            let _ = args;
        }
    }

    /// Prepares an elevated (`elevated = true`) or de-elevated (`elevated = false`) broker handoff
    /// that will reopen Loom at `restore_route` as soon as the current instance exits.
    #[inline]
    pub fn prepare_privilege_switch(
        restore_route: &str,
        elevated: bool,
    ) -> Result<PreparedSwitch, PalError> {
        #[cfg(windows)]
        {
            let inner = loom_pal_windows::admin::prepare_privilege_switch(restore_route, elevated)?;
            Ok(PreparedSwitch { inner })
        }
        #[cfg(not(windows))]
        {
            let _ = (restore_route, elevated);
            Err(PalError::Unsupported(
                "persistent administrator startup is not supported on this platform",
            ))
        }
    }
}

/// Callback invoked when a duplicate instance forwards activation CLI arguments to the primary.
pub type ActivationCallback = Arc<dyn Fn(Vec<String>) + Send + Sync + 'static>;

/// Cross-platform single-instance application lock guard.
///
/// On Windows, wraps `loom_pal_windows::WindowsSingleInstance` using a `SYNCHRONIZATION`-only
/// named mutex and a cross-UIPI `WM_COPYDATA` receiver window so secondary launches (including
/// Task Manager replacement triggers) wake and focus an existing standard or elevated instance.
/// On non-Windows platforms, wraps `single-instance`.
pub struct SingleInstanceGuard {
    #[cfg(windows)]
    inner: loom_pal_windows::WindowsSingleInstance,
    #[cfg(not(windows))]
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
        Self::acquire_or_notify(app_id, &[], None)
    }

    /// Acquires the single-instance lock or forwards `forward_args` to the existing owner.
    ///
    /// When this process becomes the primary instance and `on_activate` is supplied, duplicate
    /// launches invoke `on_activate(args)` to wake/reopen the main window.
    pub fn acquire_or_notify(
        app_id: &str,
        forward_args: &[String],
        on_activate: Option<ActivationCallback>,
    ) -> Result<Self, PalError> {
        if app_id.trim().is_empty() {
            return Err(PalError::InvalidArgument(
                "single-instance app_id cannot be empty".into(),
            ));
        }

        #[cfg(windows)]
        {
            let inner = loom_pal_windows::WindowsSingleInstance::acquire(
                app_id,
                forward_args,
                on_activate,
            )?;
            Ok(Self { inner })
        }

        #[cfg(not(windows))]
        {
            let _ = (forward_args, on_activate);
            let inner =
                single_instance::SingleInstance::new(app_id).map_err(|err| PalError::OsError {
                    code: -1,
                    message: format!("failed to create single-instance lock '{app_id}': {err}"),
                })?;
            Ok(Self { inner })
        }
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
