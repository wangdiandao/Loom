//! Configuration schema definitions and forward migration helpers.

use loom_types::{
    LanguagePreference, ProcessViewMode, RefreshSpeed, ResourceDisplayMode, StartupPage,
    ThemePreference,
};
use serde::{Deserialize, Serialize};

use crate::ConfigError;

/// Current persisted configuration schema version written to `settings.toml`.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Maximum number of recent commands retained in [`AppSettings::run_task_history`].
pub const MAX_RUN_TASK_HISTORY: usize = 16;

/// Persisted main-window bounds and maximize state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowGeometry {
    /// Horizontal screen coordinate of the top-left corner in logical pixels.
    pub x: i32,
    /// Vertical screen coordinate of the top-left corner in logical pixels.
    pub y: i32,
    /// Window width in logical pixels.
    pub width: u32,
    /// Window height in logical pixels.
    pub height: u32,
    /// Whether the window was maximized when last persisted.
    #[serde(default)]
    pub maximized: bool,
}

#[inline]
const fn default_true() -> bool {
    true
}

/// Persisted order, width, and visibility configuration for a single table column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableColumnSetting {
    /// Stable lowercase column identifier (e.g., `"name"`, `"pid"`, `"cpu"`).
    pub id: String,
    /// Saved column width in rounded logical pixels.
    pub width: u32,
    /// Whether this column is currently visible in the table.
    #[serde(default = "default_true")]
    pub visible: bool,
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
    /// Legacy/global value vs. percentage formatting for resource columns.
    pub resource_display_mode: ResourceDisplayMode,
    /// Keep the Loom main window always on top.
    pub always_on_top: bool,
    /// Hide the main window to the system tray when minimized.
    pub minimize_to_tray: bool,
    /// Hide the main window to the system tray when closing instead of exiting.
    pub close_to_tray: bool,
    /// Whether the left navigation sidebar starts collapsed.
    pub sidebar_collapsed: bool,
    /// Value vs. percentage formatting for the Memory column.
    pub memory_display_mode: ResourceDisplayMode,
    /// Value vs. percentage formatting for the Disk column.
    pub disk_display_mode: ResourceDisplayMode,
    /// Value vs. percentage formatting for the Network column.
    pub network_display_mode: ResourceDisplayMode,
    /// Last persisted main-window position, size, and maximize state.
    pub window_geometry: Option<WindowGeometry>,
    /// Most-recently-used commands launched via the "Run new task" dialog.
    pub run_task_history: Vec<String>,
    /// Persisted column layout (order, width, visibility) for the Processes table.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub processes_columns: Vec<TableColumnSetting>,
    /// Persisted column layout (order, width, visibility) for the Details table.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details_columns: Vec<TableColumnSetting>,
    /// Optional global system wakeup hotkey string (e.g., `"Ctrl+Shift+L"`), or `None` when disabled.
    #[serde(default)]
    pub global_hotkey: Option<String>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            startup_page: StartupPage::default(),
            refresh_speed: RefreshSpeed::default(),
            theme: ThemePreference::default(),
            language: LanguagePreference::default(),
            process_view_mode: ProcessViewMode::default(),
            resource_display_mode: ResourceDisplayMode::default(),
            always_on_top: false,
            minimize_to_tray: false,
            close_to_tray: false,
            sidebar_collapsed: false,
            memory_display_mode: ResourceDisplayMode::default(),
            disk_display_mode: ResourceDisplayMode::default(),
            network_display_mode: ResourceDisplayMode::default(),
            window_geometry: None,
            run_task_history: Vec::new(),
            processes_columns: Vec::new(),
            details_columns: Vec::new(),
            global_hotkey: None,
        }
    }
}

impl AppSettings {
    /// Records a command in the most-recently-used "Run new task" history.
    ///
    /// Leading and trailing whitespace is trimmed, blank commands are ignored,
    /// existing duplicates are moved to the front, and the history is capped
    /// at [`MAX_RUN_TASK_HISTORY`] entries.
    pub fn push_run_task_history(&mut self, cmd: String) {
        let trimmed = cmd.trim();
        if trimmed.is_empty() {
            return;
        }
        let normalized = if trimmed.len() == cmd.len() {
            cmd
        } else {
            trimmed.to_owned()
        };
        self.run_task_history.retain(|entry| entry != &normalized);
        self.run_task_history.insert(0, normalized);
        self.run_task_history.truncate(MAX_RUN_TASK_HISTORY);
    }

    /// Normalizes `run_task_history` by trimming entries, removing blanks and duplicates,
    /// and enforcing [`MAX_RUN_TASK_HISTORY`].
    pub(crate) fn sanitize_run_task_history(&mut self) {
        let mut cleaned = Vec::with_capacity(self.run_task_history.len().min(MAX_RUN_TASK_HISTORY));
        for entry in self.run_task_history.drain(..) {
            let trimmed = entry.trim();
            if trimmed.is_empty() {
                continue;
            }
            if cleaned.iter().any(|existing: &String| existing == trimmed) {
                continue;
            }
            if trimmed.len() == entry.len() {
                cleaned.push(entry);
            } else {
                cleaned.push(trimmed.to_owned());
            }
            if cleaned.len() == MAX_RUN_TASK_HISTORY {
                break;
            }
        }
        self.run_task_history = cleaned;
    }

    /// Normalizes `global_hotkey` by trimming whitespace and collapsing blank strings to `None`.
    pub(crate) fn sanitize_global_hotkey(&mut self) {
        self.global_hotkey = self.global_hotkey.take().and_then(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else if trimmed.len() == raw.len() {
                Some(raw)
            } else {
                Some(trimmed.to_owned())
            }
        });
    }
}

/// Parses raw TOML text and applies forward schema migrations up to [`CURRENT_SCHEMA_VERSION`].
///
/// - Missing `schema_version` or `schema_version = 0` is migrated forward to
///   [`CURRENT_SCHEMA_VERSION`].
/// - If legacy `resource_display_mode` is set while any of `memory_display_mode`,
///   `disk_display_mode`, or `network_display_mode` are absent, those omitted
///   per-column display modes inherit `resource_display_mode`.
/// - Unknown keys are ignored so older/partial configs preserve known fields cleanly.
/// - Schema versions greater than [`CURRENT_SCHEMA_VERSION`] return
///   [`ConfigError::UnsupportedSchemaVersion`].
pub fn migrate_toml_value(raw: &str) -> Result<AppSettings, ConfigError> {
    let mut table: toml::Table = toml::from_str(raw)?;

    let found_version = match table.get("schema_version") {
        None => 0_u32,
        Some(toml::Value::Integer(raw_ver)) => u32::try_from(*raw_ver).map_err(|_| {
            ConfigError::InvalidSchema(format!(
                "`schema_version` must be a non-negative 32-bit integer, found {raw_ver}"
            ))
        })?,
        Some(other) => {
            return Err(ConfigError::InvalidSchema(format!(
                "`schema_version` must be an integer, found {}",
                other.type_str()
            )));
        }
    };

    if found_version > CURRENT_SCHEMA_VERSION {
        return Err(ConfigError::UnsupportedSchemaVersion {
            found: found_version,
            max_supported: CURRENT_SCHEMA_VERSION,
        });
    }

    // Migrate v0 / missing schema_version forward to v1.
    if found_version < CURRENT_SCHEMA_VERSION {
        table.insert(
            "schema_version".to_owned(),
            toml::Value::Integer(i64::from(CURRENT_SCHEMA_VERSION)),
        );
    }

    // Inherit legacy `resource_display_mode` for any per-column display modes omitted
    // from older configs (both v0 and early v1 scaffolds).
    if let Some(legacy_mode) = table.get("resource_display_mode").cloned() {
        for key in [
            "memory_display_mode",
            "disk_display_mode",
            "network_display_mode",
        ] {
            if !table.contains_key(key) {
                table.insert(key.to_owned(), legacy_mode.clone());
            }
        }
    }

    let mut settings: AppSettings = toml::Value::Table(table).try_into()?;
    settings.schema_version = CURRENT_SCHEMA_VERSION;
    settings.sanitize_run_task_history();
    settings.sanitize_global_hotkey();
    Ok(settings)
}
