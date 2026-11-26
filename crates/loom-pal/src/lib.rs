//! Compile-time static Platform Abstraction Layer (PAL) facade for Loom.
//!
//! Exports [`NativeBackend`] via `#[cfg(target_os)]` type aliases so upper
//! layers remain completely free of OS-specific conditional compilation and
//! dynamic trait-object overhead.

#![warn(missing_docs)]

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
