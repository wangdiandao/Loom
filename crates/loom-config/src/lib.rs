//! Fast, atomic local configuration persistence (`settings.toml`) for Loom.
//!
//! Full schema migration and atomic `tempfile -> sync_all -> rename` storage
//! are implemented in subsequent `0.0.1` tasks.

#![warn(missing_docs)]

use loom_types::{
    LanguagePreference, ProcessViewMode, RefreshSpeed, ResourceDisplayMode, StartupPage,
    ThemePreference,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Structured errors returned when loading or persisting Loom settings.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Failed to read or write configuration files on disk.
    #[error("configuration I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Failed to parse TOML settings.
    #[error("configuration parse error: {0}")]
    Parse(#[from] toml::de::Error),

    /// Failed to serialize settings to TOML.
    #[error("configuration serialization error: {0}")]
    Serialize(#[from] toml::ser::Error),
}

/// Top-level persisted user settings (`settings.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Configuration schema version for forward migrations.
    pub schema_version: u32,
    /// Default navigation page opened at startup.
    pub startup_page: StartupPage,
    /// Real-time sampling cadence.
    pub refresh_speed: RefreshSpeed,
    /// Visual theme preference.
    pub theme: ThemePreference,
    /// UI language preference.
    pub language: LanguagePreference,
    /// Default view mode on the Processes page.
    pub process_view_mode: ProcessViewMode,
    /// Value vs. percentage formatting for memory/disk/network columns.
    pub resource_display_mode: ResourceDisplayMode,
    /// Keep the Loom main window always on top.
    pub always_on_top: bool,
    /// Hide the main window to the system tray when minimized.
    pub minimize_to_tray: bool,
    /// Hide the main window to the system tray when closing instead of exiting.
    pub close_to_tray: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            schema_version: 1,
            startup_page: StartupPage::default(),
            refresh_speed: RefreshSpeed::default(),
            theme: ThemePreference::default(),
            language: LanguagePreference::default(),
            process_view_mode: ProcessViewMode::default(),
            resource_display_mode: ResourceDisplayMode::default(),
            always_on_top: false,
            minimize_to_tray: false,
            close_to_tray: false,
        }
    }
}
