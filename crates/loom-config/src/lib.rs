//! Fast, atomic local configuration persistence (`settings.toml`) for Loom.
//!
//! `loom-config` is intentionally UI-agnostic and runtime-free: it depends only
//! on `loom-types` and synchronous I/O so startup configuration loading stays
//! well below the `< 1 ms` budget and writes are crash-safe via
//! `tempfile -> sync_all -> atomic rename`.

#![warn(missing_docs)]

mod error;
mod schema;
mod store;

pub use error::ConfigError;
pub use schema::{
    AppSettings, CURRENT_SCHEMA_VERSION, MAX_RUN_TASK_HISTORY, WindowGeometry, migrate_toml_value,
};
pub use store::{CONFIG_FILE_NAME, ConfigStore, default_config_path};

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use loom_types::{
        LanguagePreference, ProcessViewMode, RefreshSpeed, ResourceDisplayMode, StartupPage,
        ThemePreference,
    };

    use super::*;

    fn sample_full_settings() -> AppSettings {
        let mut settings = AppSettings {
            schema_version: CURRENT_SCHEMA_VERSION,
            startup_page: StartupPage::Performance,
            refresh_speed: RefreshSpeed::High500Ms,
            theme: ThemePreference::Dark,
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
}
