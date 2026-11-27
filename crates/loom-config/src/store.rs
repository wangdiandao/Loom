//! Atomic on-disk configuration storage for `settings.toml`.

use std::io::Write;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;

use crate::{AppSettings, CURRENT_SCHEMA_VERSION, ConfigError, migrate_toml_value};

/// Default configuration file name persisted inside Loom's config directory.
pub const CONFIG_FILE_NAME: &str = "settings.toml";

/// Returns the default platform-specific configuration file path (`settings.toml`).
///
/// Uses [`ProjectDirs::from`] with qualifier `"io"`, organization `"loom"`, and
/// application `"Loom"`. Falls back gracefully to the current working directory
/// or system temporary directory if the OS configuration directory cannot be resolved.
pub fn default_config_path() -> PathBuf {
    if let Some(project_dirs) = ProjectDirs::from("io", "loom", "Loom") {
        return project_dirs.config_dir().join(CONFIG_FILE_NAME);
    }

    if let Ok(cwd) = std::env::current_dir() {
        return cwd.join(CONFIG_FILE_NAME);
    }

    std::env::temp_dir().join("Loom").join(CONFIG_FILE_NAME)
}

/// Fast, atomic file-backed store for [`AppSettings`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    /// Creates a [`ConfigStore`] targeting a specific file `path`.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Creates a [`ConfigStore`] pointing at [`default_config_path`].
    #[must_use]
    pub fn default_store() -> Self {
        Self::new(default_config_path())
    }

    /// Returns the filesystem path managed by this store.
    #[inline]
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Synchronously loads and migrates [`AppSettings`] from disk.
    ///
    /// If the configuration file does not exist, returns `Ok(AppSettings::default())`
    /// without touching the filesystem further.
    pub fn load(&self) -> Result<AppSettings, ConfigError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AppSettings::default());
            }
            Err(err) => return Err(ConfigError::Io(err)),
        };

        migrate_toml_value(&raw)
    }

    /// Loads [`AppSettings`] from disk, falling back to [`AppSettings::default`] on error.
    ///
    /// Any I/O, parse, or schema error is logged via `tracing` and returned alongside
    /// the default settings so startup never panics or blocks on corrupt configuration files.
    #[must_use]
    pub fn load_or_default(&self) -> (AppSettings, Option<ConfigError>) {
        match self.load() {
            Ok(settings) => (settings, None),
            Err(err) => {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %err,
                    "failed to load Loom settings; falling back to defaults"
                );
                (AppSettings::default(), Some(err))
            }
        }
    }

    /// Fast startup loader returning [`AppSettings`] without propagating errors.
    ///
    /// Missing files immediately return [`AppSettings::default`]; corrupt or
    /// unreadable files log a warning via `tracing` and fall back to defaults.
    #[must_use]
    pub fn load_fast(&self) -> AppSettings {
        self.load_or_default().0
    }

    /// Atomically persists [`AppSettings`] to disk using `tempfile -> sync_all -> rename`.
    ///
    /// Missing parent directories are created automatically. The temporary file is
    /// created in the same parent directory as [`Self::path`] so the final rename is
    /// atomic on the target filesystem and leaves no stray temp files behind.
    pub fn save(&self, settings: &AppSettings) -> Result<(), ConfigError> {
        let mut normalized = settings.clone();
        if normalized.schema_version == 0 {
            normalized.schema_version = CURRENT_SCHEMA_VERSION;
        } else if normalized.schema_version > CURRENT_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSchemaVersion {
                found: normalized.schema_version,
                max_supported: CURRENT_SCHEMA_VERSION,
            });
        }
        normalized.sanitize_run_task_history();
        normalized.sanitize_global_hotkey();

        let serialized = toml::to_string_pretty(&normalized)?;

        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }

        let mut temp_file = tempfile::Builder::new()
            .prefix(".settings.toml.")
            .suffix(".tmp")
            .tempfile_in(parent)?;
        temp_file.write_all(serialized.as_bytes())?;
        temp_file.as_file().sync_all()?;
        temp_file
            .persist(&self.path)
            .map_err(|err| ConfigError::Persist(err.error))?;

        Ok(())
    }
}
