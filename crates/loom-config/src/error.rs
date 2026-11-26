//! Structured error types for `loom-config`.

use thiserror::Error;

/// Structured errors returned when loading, migrating, or persisting Loom settings.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Failed to read, create parent directories for, or sync configuration files on disk.
    #[error("configuration I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Failed to atomically replace the target configuration file after syncing the temporary file.
    #[error("configuration atomic persist error: {0}")]
    Persist(#[source] std::io::Error),

    /// Failed to parse TOML settings or deserialize fields.
    #[error("configuration parse error: {0}")]
    Parse(#[from] toml::de::Error),

    /// Failed to serialize settings to TOML.
    #[error("configuration serialization error: {0}")]
    Serialize(#[from] toml::ser::Error),

    /// Encountered a `schema_version` field with an invalid type or out-of-range value.
    #[error("invalid configuration schema: {0}")]
    InvalidSchema(String),

    /// Encountered a configuration written by a newer unsupported schema version.
    #[error(
        "unsupported configuration schema version {found} (maximum supported: {max_supported})"
    )]
    UnsupportedSchemaVersion {
        /// Schema version found in the configuration payload.
        found: u32,
        /// Highest schema version supported by this build of Loom.
        max_supported: u32,
    },
}
