//! Fast, atomic local configuration persistence (`settings.toml`) for Loom.
//!
//! `loom-config` is intentionally UI-agnostic and runtime-free: it depends only
//! on `loom-types` and synchronous I/O so startup configuration loading stays
//! well below the `< 1 ms` budget and writes are crash-safe via
//! `tempfile -> sync_all -> atomic rename`.
//!
//! # Frozen `0.1.0` v1 Forward-Migration Contract
//!
//! Starting in Loom `0.1.0` ([`CONFIG_CONTRACT_VERSION`] `= "1.0"`), the persisted
//! `settings.toml` v1 schema ([`CURRENT_SCHEMA_VERSION`] `= 1`) is frozen under
//! the following compatibility guarantees:
//!
//! 1. **Golden v1 stability**: Existing v1 `settings.toml` files covering any subset
//!    of [`AppSettings`] fields must deserialize cleanly and round-trip without data loss.
//! 2. **Forward migration from legacy/partial configs**: Missing `schema_version` or
//!    `schema_version = 0` files are migrated forward to v1 via [`migrate_toml_value`],
//!    inheriting legacy `resource_display_mode` for omitted per-column display modes.
//! 3. **Forward tolerance for unknown optional keys**: Deserialization intentionally
//!    omits `#[serde(deny_unknown_fields)]` so future optional keys do not break v1
//!    readers.
//! 4. **Strict future-schema rejection**: Any `schema_version > CURRENT_SCHEMA_VERSION`
//!    (e.g., `schema_version = 2`) fails cleanly with [`ConfigError::UnsupportedSchemaVersion`].

#![warn(missing_docs)]

mod error;
mod schema;
mod store;

/// Frozen public configuration contract version established in Loom `0.1.0`.
///
/// Tracks the `settings.toml` v1 forward-migration and compatibility guarantee:
/// legacy/partial v0 files migrate forward cleanly, unknown optional keys are
/// ignored, and future incompatible `schema_version` values return
/// [`ConfigError::UnsupportedSchemaVersion`].
pub const CONFIG_CONTRACT_VERSION: &str = "1.0";

pub use error::ConfigError;
pub use schema::{
    AppSettings, CURRENT_SCHEMA_VERSION, MAX_RUN_TASK_HISTORY, TableColumnSetting, WindowGeometry,
    migrate_toml_value,
};
pub use store::{CONFIG_FILE_NAME, ConfigStore, default_config_path};

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use loom_types::{
        AccentColorPreference, LanguagePreference, ProcessViewMode, RefreshSpeed,
        ResourceDisplayMode, StartupPage, ThemePreference,
    };

    use super::*;

    fn sample_full_settings() -> AppSettings {
        let mut settings = AppSettings {
            schema_version: CURRENT_SCHEMA_VERSION,
            startup_page: StartupPage::Performance,
            refresh_speed: RefreshSpeed::High500Ms,
            theme: ThemePreference::Dark,
            accent_color: AccentColorPreference::custom(0x1D, 0xC5, 0xF4),
            language: LanguagePreference::EnUs,
            process_view_mode: ProcessViewMode::Tree,
            resource_display_mode: ResourceDisplayMode::Percent,
            always_on_top: true,
            minimize_to_tray: true,
            close_to_tray: true,
            sidebar_collapsed: true,
            memory_display_mode: ResourceDisplayMode::Percent,
            disk_display_mode: ResourceDisplayMode::Values,
            network_display_mode: ResourceDisplayMode::Percent,
            window_geometry: Some(WindowGeometry {
                x: 120,
                y: -40,
                width: 1440,
                height: 900,
                maximized: false,
            }),
            run_task_history: Vec::new(),
            processes_columns: vec![
                TableColumnSetting {
                    id: "name".to_owned(),
                    width: 260,
                    visible: true,
                },
                TableColumnSetting {
                    id: "pid".to_owned(),
                    width: 76,
                    visible: true,
                },
                TableColumnSetting {
                    id: "publisher".to_owned(),
                    width: 176,
                    visible: false,
                },
            ],
            details_columns: vec![
                TableColumnSetting {
                    id: "name".to_owned(),
                    width: 240,
                    visible: true,
                },
                TableColumnSetting {
                    id: "threads".to_owned(),
                    width: 72,
                    visible: true,
                },
            ],
            global_hotkey: Some("Ctrl+Shift+L".to_owned()),
            webview2_host_grouping: true,
            show_leak_warnings: true,
            leak_ignored_paths: Vec::new(),
            hide_windows_system_processes: false,
        };
        for i in 0..MAX_RUN_TASK_HISTORY {
            settings.push_run_task_history(format!(r"C:\Windows\System32\tool_{i}.exe --flag"));
        }
        settings
    }

    #[test]
    fn default_roundtrip_and_atomic_overwrite_leaves_no_temp_files() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let nested_path = dir
            .path()
            .join("nested")
            .join("loom")
            .join(CONFIG_FILE_NAME);
        let store = ConfigStore::new(nested_path.clone());

        // Roundtrip defaults into a previously non-existent nested directory.
        let defaults = AppSettings::default();
        store.save(&defaults).expect("save defaults");
        assert!(nested_path.exists());

        let raw_default = std::fs::read_to_string(&nested_path).expect("read default toml");
        assert!(
            raw_default.len() < 4096,
            "default settings.toml should stay compact (< 4 KB), got {} bytes",
            raw_default.len()
        );
        assert_eq!(store.load().expect("load defaults"), defaults);

        // Atomically overwrite with fully populated settings.
        let updated = sample_full_settings();
        store.save(&updated).expect("overwrite with full settings");

        let raw_updated = std::fs::read_to_string(&nested_path).expect("read updated toml");
        assert!(
            raw_updated.len() < 4096,
            "populated settings.toml should stay compact (< 4 KB), got {} bytes",
            raw_updated.len()
        );
        assert_eq!(store.load().expect("load updated"), updated);

        // Ensure no leftover temp files exist in the target directory.
        let entries: Vec<_> = std::fs::read_dir(nested_path.parent().expect("parent dir"))
            .expect("read config dir")
            .map(|entry| entry.expect("dir entry").file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from(CONFIG_FILE_NAME)]);
    }

    #[test]
    fn missing_file_fast_fallback() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let missing_path = dir.path().join("does_not_exist").join(CONFIG_FILE_NAME);
        let store = ConfigStore::new(missing_path);

        assert_eq!(
            store.load().expect("missing file loads defaults"),
            AppSettings::default()
        );

        let (loaded, err) = store.load_or_default();
        assert_eq!(loaded, AppSettings::default());
        assert!(err.is_none());

        assert_eq!(store.load_fast(), AppSettings::default());
    }

    #[test]
    fn corrupt_toml_returns_parse_error_and_safe_default_fallback() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join(CONFIG_FILE_NAME);
        std::fs::write(&path, "theme = [unclosed_array\nrefresh_speed = ???")
            .expect("write corrupt toml");

        let store = ConfigStore::new(path);
        let err = store.load().expect_err("corrupt TOML must return error");
        assert!(matches!(err, ConfigError::Parse(_)));

        let (fallback, reported_err) = store.load_or_default();
        assert_eq!(fallback, AppSettings::default());
        assert!(matches!(reported_err, Some(ConfigError::Parse(_))));

        assert_eq!(store.load_fast(), AppSettings::default());
    }

    #[test]
    fn schema_v0_and_partial_toml_forward_migration_to_v1() {
        // Explicit schema_version = 0 with legacy resource_display_mode and unknown extra field.
        let v0_raw = r#"
            schema_version = 0
            startup_page = "Details"
            theme = "Light"
            resource_display_mode = "Percent"
            disk_display_mode = "Values"
            always_on_top = true
            unknown_legacy_flag = 42
        "#;
        let migrated_v0 = migrate_toml_value(v0_raw).expect("migrate v0");
        assert_eq!(migrated_v0.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(migrated_v0.startup_page, StartupPage::Details);
        assert_eq!(migrated_v0.theme, ThemePreference::Light);
        assert_eq!(
            migrated_v0.resource_display_mode,
            ResourceDisplayMode::Percent
        );
        // Omitted per-column modes inherit legacy resource_display_mode; explicit override wins.
        assert_eq!(
            migrated_v0.memory_display_mode,
            ResourceDisplayMode::Percent
        );
        assert_eq!(migrated_v0.disk_display_mode, ResourceDisplayMode::Values);
        assert_eq!(
            migrated_v0.network_display_mode,
            ResourceDisplayMode::Percent
        );
        assert!(migrated_v0.always_on_top);
        assert!(!migrated_v0.sidebar_collapsed);
        assert_eq!(migrated_v0.window_geometry, None);

        // Partial TOML without schema_version.
        let partial_raw = r#"
            language = "EnUs"
            sidebar_collapsed = true

            [window_geometry]
            x = 10
            y = 20
            width = 1280
            height = 720
        "#;
        let migrated_partial = migrate_toml_value(partial_raw).expect("migrate partial");
        assert_eq!(migrated_partial.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(migrated_partial.language, LanguagePreference::EnUs);
        assert!(migrated_partial.sidebar_collapsed);
        assert_eq!(
            migrated_partial.window_geometry,
            Some(WindowGeometry {
                x: 10,
                y: 20,
                width: 1280,
                height: 720,
                maximized: false,
            })
        );

        // Empty TOML produces current defaults.
        assert_eq!(
            migrate_toml_value("").expect("empty toml"),
            AppSettings::default()
        );

        // Oversized or dirty run_task_history is sanitized on load/migration.
        let dirty_history_raw = r#"
            schema_version = 1
            run_task_history = [
                "  notepad.exe  ",
                "",
                "   ",
                "cmd.exe",
                "notepad.exe",
                "pwsh.exe"
            ]
        "#;
        let sanitized = migrate_toml_value(dirty_history_raw).expect("sanitize history");
        assert_eq!(
            sanitized.run_task_history,
            vec!["notepad.exe", "cmd.exe", "pwsh.exe"]
        );
    }

    #[test]
    fn save_normalizes_zero_schema_version_to_current() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let store = ConfigStore::new(dir.path().join(CONFIG_FILE_NAME));
        let legacy = AppSettings {
            schema_version: 0,
            always_on_top: true,
            ..AppSettings::default()
        };

        store.save(&legacy).expect("save legacy schema_version=0");
        let loaded = store.load().expect("reload normalized settings");
        assert_eq!(loaded.schema_version, CURRENT_SCHEMA_VERSION);
        assert!(loaded.always_on_top);
    }

    #[test]
    fn rejects_unsupported_future_and_invalid_schema_versions() {
        let future_raw = r#"
            schema_version = 99
            theme = "Dark"
        "#;
        let future_err = migrate_toml_value(future_raw).expect_err("future schema must error");
        assert!(matches!(
            future_err,
            ConfigError::UnsupportedSchemaVersion {
                found: 99,
                max_supported: CURRENT_SCHEMA_VERSION,
            }
        ));

        let negative_err =
            migrate_toml_value("schema_version = -1").expect_err("negative schema must error");
        assert!(matches!(negative_err, ConfigError::InvalidSchema(_)));

        let wrong_type_err =
            migrate_toml_value("schema_version = \"1\"").expect_err("string schema must error");
        assert!(matches!(wrong_type_err, ConfigError::InvalidSchema(_)));

        // Saving an unsupported future schema version is also rejected.
        let dir = tempfile::tempdir().expect("create temp dir");
        let store = ConfigStore::new(dir.path().join(CONFIG_FILE_NAME));
        let invalid = AppSettings {
            schema_version: CURRENT_SCHEMA_VERSION + 1,
            ..AppSettings::default()
        };
        assert!(matches!(
            store.save(&invalid),
            Err(ConfigError::UnsupportedSchemaVersion { .. })
        ));
    }

    #[test]
    fn push_run_task_history_deduplicates_trims_and_caps_at_limit() {
        let mut settings = AppSettings::default();

        // Blank entries are ignored.
        settings.push_run_task_history(String::new());
        settings.push_run_task_history("   \t\n  ".to_owned());
        assert!(settings.run_task_history.is_empty());

        // Entries are trimmed and ordered most-recent-first.
        settings.push_run_task_history("  notepad.exe  ".to_owned());
        settings.push_run_task_history("cmd.exe".to_owned());
        settings.push_run_task_history("pwsh.exe".to_owned());
        assert_eq!(
            settings.run_task_history,
            vec!["pwsh.exe", "cmd.exe", "notepad.exe"]
        );

        // Pushing a duplicate promotes it to index 0 without duplicating.
        settings.push_run_task_history(" notepad.exe ".to_owned());
        assert_eq!(
            settings.run_task_history,
            vec!["notepad.exe", "pwsh.exe", "cmd.exe"]
        );

        // Pushing beyond MAX_RUN_TASK_HISTORY evicts the oldest entries.
        for i in 0..(MAX_RUN_TASK_HISTORY + 5) {
            settings.push_run_task_history(format!("task_{i}.exe"));
        }
        assert_eq!(settings.run_task_history.len(), MAX_RUN_TASK_HISTORY);
        assert_eq!(
            settings.run_task_history[0],
            format!("task_{}.exe", MAX_RUN_TASK_HISTORY + 4)
        );
        assert_eq!(
            settings.run_task_history[MAX_RUN_TASK_HISTORY - 1],
            "task_5.exe"
        );
    }

    #[test]
    fn default_config_path_ends_with_settings_toml() {
        let path = default_config_path();
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(CONFIG_FILE_NAME)
        );
        assert_eq!(ConfigStore::default_store().path(), path.as_path());
    }

    #[test]
    fn warm_sync_load_stays_under_one_millisecond() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join(CONFIG_FILE_NAME);
        let store = ConfigStore::new(path.clone());

        let expected = sample_full_settings();
        store.save(&expected).expect("save sample settings");

        let byte_len = std::fs::metadata(&path).expect("stat settings.toml").len();
        assert!(
            byte_len < 4096,
            "typical settings.toml must stay under 4 KB, got {byte_len} bytes"
        );

        // Warm OS page cache and allocator before measuring.
        for _ in 0..8 {
            assert_eq!(store.load().expect("warmup load"), expected);
        }

        const SAMPLES: usize = 51;
        let mut durations = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let start = Instant::now();
            let loaded = store.load().expect("timed load");
            durations.push(start.elapsed());
            std::hint::black_box(loaded);
        }
        durations.sort_unstable();
        let median_elapsed = durations[SAMPLES / 2];

        assert!(
            median_elapsed < Duration::from_millis(1),
            "warm sync load exceeded < 1 ms budget: median {:?} per load ({byte_len} bytes)",
            median_elapsed
        );
    }

    #[test]
    fn global_hotkey_is_backward_compatible_sanitized_and_roundtrips() {
        // Legacy v1 TOML without `global_hotkey` loads cleanly as `None`.
        let old_v1_toml = r#"
            schema_version = 1
            startup_page = "Processes"
            refresh_speed = "Normal1S"
            theme = "System"
            language = "EnUs"
        "#;
        let loaded_legacy = migrate_toml_value(old_v1_toml).expect("load legacy v1 without hotkey");
        assert_eq!(loaded_legacy.global_hotkey, None);

        // Blank / whitespace-only hotkey strings normalize to `None`.
        let blank_toml = r#"
            schema_version = 1
            global_hotkey = "   \t  "
        "#;
        let loaded_blank = migrate_toml_value(blank_toml).expect("sanitize blank hotkey");
        assert_eq!(loaded_blank.global_hotkey, None);

        // Trimmed hotkey strings round-trip cleanly through ConfigStore.
        let dir = tempfile::tempdir().expect("create temp dir");
        let store = ConfigStore::new(dir.path().join(CONFIG_FILE_NAME));
        let with_hotkey = AppSettings {
            global_hotkey: Some("  Ctrl+Shift+L  ".to_owned()),
            ..AppSettings::default()
        };
        store.save(&with_hotkey).expect("save hotkey settings");
        let reloaded = store.load().expect("reload hotkey settings");
        assert_eq!(reloaded.global_hotkey.as_deref(), Some("Ctrl+Shift+L"));

        // Clearing back to `None` round-trips cleanly.
        let cleared = AppSettings {
            global_hotkey: Some("   ".to_owned()),
            ..reloaded
        };
        store.save(&cleared).expect("save cleared hotkey settings");
        let reloaded_cleared = store.load().expect("reload cleared hotkey settings");
        assert_eq!(reloaded_cleared.global_hotkey, None);
    }

    #[test]
    fn v1_contract_golden_toml_deserializes_and_roundtrips_identically() {
        assert_eq!(CONFIG_CONTRACT_VERSION, "1.0");
        assert_eq!(CURRENT_SCHEMA_VERSION, 1);

        // Complete golden v1 settings.toml covering every persisted field in AppSettings.
        const GOLDEN_V1_TOML: &str = r#"
schema_version = 1
startup_page = "Performance"
refresh_speed = "High500Ms"
theme = "Dark"
language = "EnUs"
process_view_mode = "Tree"
resource_display_mode = "Percent"
always_on_top = true
minimize_to_tray = true
close_to_tray = true
sidebar_collapsed = true
memory_display_mode = "Percent"
disk_display_mode = "Values"
network_display_mode = "Percent"
global_hotkey = "Ctrl+Shift+L"
run_task_history = [
    "pwsh.exe",
    "cmd.exe /k ver",
    "notepad.exe",
]

[window_geometry]
x = 120
y = -40
width = 1440
height = 900
maximized = true

[[processes_columns]]
id = "name"
width = 260
visible = true

[[processes_columns]]
id = "pid"
width = 76
visible = true

[[processes_columns]]
id = "publisher"
width = 176
visible = false

[[details_columns]]
id = "name"
width = 240
visible = true

[[details_columns]]
id = "threads"
width = 72
visible = true
"#;

        let expected = AppSettings {
            schema_version: 1,
            startup_page: StartupPage::Performance,
            refresh_speed: RefreshSpeed::High500Ms,
            theme: ThemePreference::Dark,
            accent_color: AccentColorPreference::System,
            language: LanguagePreference::EnUs,
            process_view_mode: ProcessViewMode::Tree,
            resource_display_mode: ResourceDisplayMode::Percent,
            always_on_top: true,
            minimize_to_tray: true,
            close_to_tray: true,
            sidebar_collapsed: true,
            memory_display_mode: ResourceDisplayMode::Percent,
            disk_display_mode: ResourceDisplayMode::Values,
            network_display_mode: ResourceDisplayMode::Percent,
            window_geometry: Some(WindowGeometry {
                x: 120,
                y: -40,
                width: 1440,
                height: 900,
                maximized: true,
            }),
            run_task_history: vec![
                "pwsh.exe".to_owned(),
                "cmd.exe /k ver".to_owned(),
                "notepad.exe".to_owned(),
            ],
            processes_columns: vec![
                TableColumnSetting {
                    id: "name".to_owned(),
                    width: 260,
                    visible: true,
                },
                TableColumnSetting {
                    id: "pid".to_owned(),
                    width: 76,
                    visible: true,
                },
                TableColumnSetting {
                    id: "publisher".to_owned(),
                    width: 176,
                    visible: false,
                },
            ],
            details_columns: vec![
                TableColumnSetting {
                    id: "name".to_owned(),
                    width: 240,
                    visible: true,
                },
                TableColumnSetting {
                    id: "threads".to_owned(),
                    width: 72,
                    visible: true,
                },
            ],
            global_hotkey: Some("Ctrl+Shift+L".to_owned()),
            webview2_host_grouping: true,
            show_leak_warnings: true,
            leak_ignored_paths: Vec::new(),
            hide_windows_system_processes: false,
        };

        let parsed = migrate_toml_value(GOLDEN_V1_TOML).expect("golden v1 TOML must deserialize");
        assert_eq!(parsed, expected);

        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join(CONFIG_FILE_NAME);
        let store = ConfigStore::new(path.clone());
        store.save(&parsed).expect("save golden v1 settings");
        let reloaded = store.load().expect("reload golden v1 settings");
        assert_eq!(reloaded, expected);

        let raw_saved = std::fs::read_to_string(&path).expect("read saved golden TOML");
        let reparsed = migrate_toml_value(&raw_saved).expect("reparse saved golden TOML");
        assert_eq!(reparsed, expected);
    }

    #[test]
    fn v1_contract_legacy_v0_and_partial_configs_migrate_through_store() {
        const LEGACY_V0_TOML: &str = r#"
schema_version = 0
startup_page = "Services"
refresh_speed = "Low2S"
theme = "Dark"
resource_display_mode = "Percent"
network_display_mode = "Values"
minimize_to_tray = true
"#;
        const PARTIAL_TOML: &str = r#"
theme = "Light"
language = "EnUs"
always_on_top = true
"#;

        let dir = tempfile::tempdir().expect("create temp dir");

        let v0_path = dir.path().join("legacy_v0.toml");
        std::fs::write(&v0_path, LEGACY_V0_TOML).expect("write legacy v0");
        let v0_store = ConfigStore::new(v0_path);
        let migrated_v0 = v0_store.load().expect("load and migrate v0");
        assert_eq!(migrated_v0.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(migrated_v0.startup_page, StartupPage::Services);
        assert_eq!(migrated_v0.refresh_speed, RefreshSpeed::Low2S);
        assert_eq!(migrated_v0.theme, ThemePreference::Dark);
        assert_eq!(
            migrated_v0.memory_display_mode,
            ResourceDisplayMode::Percent
        );
        assert_eq!(migrated_v0.disk_display_mode, ResourceDisplayMode::Percent);
        assert_eq!(
            migrated_v0.network_display_mode,
            ResourceDisplayMode::Values
        );
        assert!(migrated_v0.minimize_to_tray);
        v0_store.save(&migrated_v0).expect("persist migrated v0");
        assert_eq!(v0_store.load().expect("reload migrated v0"), migrated_v0);

        let partial_path = dir.path().join("partial.toml");
        std::fs::write(&partial_path, PARTIAL_TOML).expect("write partial");
        let partial_store = ConfigStore::new(partial_path);
        let migrated_partial = partial_store.load().expect("load and migrate partial");
        assert_eq!(migrated_partial.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(migrated_partial.theme, ThemePreference::Light);
        assert_eq!(migrated_partial.language, LanguagePreference::EnUs);
        assert!(migrated_partial.always_on_top);
        assert_eq!(migrated_partial.startup_page, StartupPage::default());
        partial_store
            .save(&migrated_partial)
            .expect("persist migrated partial");
        assert_eq!(
            partial_store.load().expect("reload migrated partial"),
            migrated_partial
        );
    }

    #[test]
    fn v1_contract_unknown_future_optional_keys_do_not_break_deserialization() {
        const V1_WITH_UNKNOWN_FUTURE_KEYS: &str = r#"
schema_version = 1
startup_page = "Details"
theme = "Light"
language = "ZhCn"
future_experimental_toggle = true
future_accent_palette = "cobalt"
future_tags = ["alpha", "beta"]

[window_geometry]
x = 64
y = 48
width = 1280
height = 800
maximized = false
future_monitor_id = "DISPLAY1"

[[processes_columns]]
id = "cpu"
width = 92
visible = true
future_pinned_side = "right"

[future_section]
enabled = true
threshold_ms = 250
"#;

        let parsed = migrate_toml_value(V1_WITH_UNKNOWN_FUTURE_KEYS)
            .expect("v1 TOML with unknown future optional keys must deserialize cleanly");
        assert_eq!(parsed.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(parsed.startup_page, StartupPage::Details);
        assert_eq!(parsed.theme, ThemePreference::Light);
        assert_eq!(parsed.language, LanguagePreference::ZhCn);
        assert_eq!(
            parsed.window_geometry,
            Some(WindowGeometry {
                x: 64,
                y: 48,
                width: 1280,
                height: 800,
                maximized: false,
            })
        );
        assert_eq!(
            parsed.processes_columns,
            vec![TableColumnSetting {
                id: "cpu".to_owned(),
                width: 92,
                visible: true,
            }]
        );

        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join(CONFIG_FILE_NAME);
        std::fs::write(&path, V1_WITH_UNKNOWN_FUTURE_KEYS).expect("write forward-compat toml");
        let store = ConfigStore::new(path);
        assert_eq!(
            store.load().expect("ConfigStore loads unknown keys"),
            parsed
        );
    }

    #[test]
    fn v1_contract_rejects_future_schema_version_2_with_structured_error() {
        const V2_TOML: &str = r#"
schema_version = 2
startup_page = "Processes"
theme = "Dark"
"#;

        let err =
            migrate_toml_value(V2_TOML).expect_err("schema_version = 2 must be rejected cleanly");
        assert!(matches!(
            err,
            ConfigError::UnsupportedSchemaVersion {
                found: 2,
                max_supported: 1,
            }
        ));

        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join(CONFIG_FILE_NAME);
        std::fs::write(&path, V2_TOML).expect("write v2 toml");
        let store = ConfigStore::new(path);

        let load_err = store
            .load()
            .expect_err("ConfigStore::load must reject schema_version = 2");
        assert!(matches!(
            load_err,
            ConfigError::UnsupportedSchemaVersion {
                found: 2,
                max_supported: 1,
            }
        ));

        let (fallback, reported_err) = store.load_or_default();
        assert_eq!(fallback, AppSettings::default());
        assert!(matches!(
            reported_err,
            Some(ConfigError::UnsupportedSchemaVersion {
                found: 2,
                max_supported: 1,
            })
        ));
    }

    #[test]
    fn additive_v1_process_preferences_default_sanitize_and_roundtrip() {
        // Legacy v1 TOML without the new optional keys inherits defaults cleanly.
        let legacy_v1 = r#"
            schema_version = 1
            theme = "Dark"
        "#;
        let loaded = migrate_toml_value(legacy_v1).expect("load legacy v1 without new prefs");
        assert!(loaded.webview2_host_grouping);
        assert!(loaded.show_leak_warnings);
        assert!(loaded.leak_ignored_paths.is_empty());
        assert!(!loaded.hide_windows_system_processes);
        assert_eq!(loaded.accent_color, AccentColorPreference::System);

        // Custom hex accent color and System accent color round-trip cleanly via ConfigStore.
        let custom_accent = migrate_toml_value(
            r##"
                schema_version = 1
                accent_color = "#8764b8"
            "##,
        )
        .expect("parse custom hex accent_color");
        assert_eq!(
            custom_accent.accent_color,
            AccentColorPreference::custom(0x87, 0x64, 0xB8)
        );
        let dir_accent = tempfile::tempdir().expect("create accent temp dir");
        let store_accent = ConfigStore::new(dir_accent.path().join(CONFIG_FILE_NAME));
        store_accent
            .save(&custom_accent)
            .expect("save custom accent");
        let raw_accent =
            std::fs::read_to_string(store_accent.path()).expect("read saved accent toml");
        assert!(raw_accent.contains("accent_color = \"#8764B8\""));
        assert_eq!(
            store_accent.load().expect("reload custom accent"),
            custom_accent
        );

        // Non-default values and dirty `leak_ignored_paths` sanitize and round-trip via ConfigStore.
        let dir = tempfile::tempdir().expect("create temp dir");
        let store = ConfigStore::new(dir.path().join(CONFIG_FILE_NAME));
        let custom = AppSettings {
            webview2_host_grouping: false,
            show_leak_warnings: false,
            leak_ignored_paths: vec![
                "  C:\\Tools\\worker.exe  ".to_owned(),
                "".to_owned(),
                "   ".to_owned(),
                "C:\\Tools\\worker.exe".to_owned(),
                "custom_agent.exe".to_owned(),
            ],
            hide_windows_system_processes: true,
            ..AppSettings::default()
        };
        store.save(&custom).expect("save custom additive v1 prefs");
        let reloaded = store.load().expect("reload custom additive v1 prefs");
        assert!(!reloaded.webview2_host_grouping);
        assert!(!reloaded.show_leak_warnings);
        assert_eq!(
            reloaded.leak_ignored_paths,
            vec!["C:\\Tools\\worker.exe", "custom_agent.exe"]
        );
        assert!(reloaded.hide_windows_system_processes);
    }

    #[test]
    fn utf8_bom_prefixed_settings_load_cleanly() {
        let bom_toml = "\u{feff}schema_version = 1\ntheme = \"Dark\"\nlanguage = \"EnUs\"\n";
        let parsed = migrate_toml_value(bom_toml).expect("BOM-prefixed TOML must parse");
        assert_eq!(parsed.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(parsed.theme, ThemePreference::Dark);
        assert_eq!(parsed.language, LanguagePreference::EnUs);
    }
}
