//! Host operating system identity types advertised statically by platform backends.

use serde::{Deserialize, Serialize};

/// Operating system family identifier for the active platform backend.
///
/// Embedded inside [`PlatformProfile`] and published on [`crate::AppSnapshot`]
/// so UI and engine layers can render OS-appropriate wording without introducing
/// `#[cfg(target_os)]` branches outside `loom-pal`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlatformKind {
    /// Microsoft Windows.
    Windows,
    /// Linux / POSIX desktop and server hosts.
    Linux,
    /// Apple macOS.
    Macos,
    /// Unspecified, synthetic, or mock platform backend.
    #[default]
    Unknown,
}

/// Static platform identity profile published by a `PlatformBackend` implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct PlatformProfile {
    /// Operating system family identifier.
    pub kind: PlatformKind,
    /// Human-readable operating system display name (e.g., `"Windows"`, `"Linux"`, `"macOS"`).
    pub os_name: &'static str,
}

impl PlatformProfile {
    /// Static profile for Microsoft Windows hosts.
    pub const WINDOWS: Self = Self::new(PlatformKind::Windows, "Windows");
    /// Static profile for Linux hosts.
    pub const LINUX: Self = Self::new(PlatformKind::Linux, "Linux");
    /// Static profile for Apple macOS hosts.
    pub const MACOS: Self = Self::new(PlatformKind::Macos, "macOS");
    /// Fallback profile for unknown or mock backends.
    pub const UNKNOWN: Self = Self::new(PlatformKind::Unknown, "Unknown");

    /// Creates a compile-time [`PlatformProfile`].
    #[inline]
    pub const fn new(kind: PlatformKind, os_name: &'static str) -> Self {
        Self { kind, os_name }
    }
}

impl Default for PlatformProfile {
    #[inline]
    fn default() -> Self {
        Self::UNKNOWN
    }
}
