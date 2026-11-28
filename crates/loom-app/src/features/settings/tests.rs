//! Unit and GPUI integration tests for the Settings vertical feature slice.

use std::sync::Arc;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{AppContext as _, TestAppContext, VisualTestContext};
use loom_config::{AppSettings, ConfigStore};
use loom_engine::EngineCoordinator;
use loom_pal::{NativeBackend, PlatformBackend};
use loom_types::{
    AccentColorPreference, AdminStartupStatus, TaskManagerReplacementState,
    TaskManagerReplacementStatus, ThemePreference,
};
use loom_ui_kit::hsla_to_rgb_u8;

use crate::bridge::UiStore;

use super::SettingsView;
use super::options::{
    AdminStartupPresentation, StatusTone, TaskMgrReplacementPresentation, parse_custom_accent_hex,
};

#[test]
fn custom_accent_hex_validation_accepts_valid_and_rejects_malformed() {
    assert_eq!(
        parse_custom_accent_hex("#8764B8"),
        Some(AccentColorPreference::custom(0x87, 0x64, 0xB8))
    );
    assert_eq!(
        parse_custom_accent_hex("  0078d4 "),
        Some(AccentColorPreference::custom(0x00, 0x78, 0xD4))
    );
    assert_eq!(
        parse_custom_accent_hex("#0af"),
        Some(AccentColorPreference::custom(0x00, 0xAA, 0xFF))
    );
    assert_eq!(parse_custom_accent_hex(""), None);
    assert_eq!(parse_custom_accent_hex("#GGGGGG"), None);
    assert_eq!(parse_custom_accent_hex("#12345"), None);
    assert_eq!(parse_custom_accent_hex("not-a-color"), None);
}

#[test]
fn admin_startup_and_taskmgr_presentations_handle_all_status_variants() {
    // Loading (None) disables switches cleanly with no spurious badges.
    let admin_loading = AdminStartupPresentation::from_status(None, false);
    assert!(!admin_loading.checked);
    assert!(admin_loading.switch_disabled);
    assert!(admin_loading.description.is_none());
    assert!(admin_loading.badge_label.is_none());

    // Admin startup enabled but broken scheduled task -> warning + repair CTA.
    let admin_repair = AdminStartupPresentation::from_status(
        Some(&AdminStartupStatus {
            supported: true,
            eligible: true,
            elevated: false,
            enabled: true,
            ready: false,
            unprotected_path: false,
            can_drop_elevation: false,
            error: None,
        }),
        false,
    );
    assert!(admin_repair.checked);
    assert!(admin_repair.needs_repair);
    assert!(!admin_repair.switch_disabled);
    assert_eq!(admin_repair.tone, StatusTone::Warning);
    assert!(admin_repair.badge_label.is_some());

    // IFEO conflict -> danger badge & detail message, switch disabled.
    let tm_conflict = TaskMgrReplacementPresentation::from_status(
        Some(&TaskManagerReplacementStatus {
            state: TaskManagerReplacementState::Conflict,
            can_enable: false,
            can_restore: false,
            unprotected_path: false,
            conflict_detail: Some("ProcessHacker.exe".to_owned()),
            recovery_path: None,
        }),
        false,
    );
    assert!(!tm_conflict.checked);
    assert!(tm_conflict.switch_disabled);
    assert!(!tm_conflict.show_restore);
    assert_eq!(tm_conflict.tone, StatusTone::Danger);
    assert!(
        tm_conflict
            .description
            .as_deref()
            .is_some_and(|d| d.contains("ProcessHacker.exe"))
    );

    // IFEO revoked with restore capability -> warning badge & restore CTA.
    let tm_revoked = TaskMgrReplacementPresentation::from_status(
        Some(&TaskManagerReplacementStatus {
            state: TaskManagerReplacementState::Revoked,
            can_enable: true,
            can_restore: true,
            unprotected_path: false,
            conflict_detail: None,
            recovery_path: None,
        }),
        false,
    );
    assert!(tm_revoked.show_restore);
    assert_eq!(tm_revoked.tone, StatusTone::Warning);
    assert!(tm_revoked.badge_label.is_some());
}

#[gpui_kit::test]
fn settings_view_diagnostic_toggles_persist_and_reload_cleanly(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);

    let dir = tempfile::tempdir().expect("create temp config dir");
    let config_path = dir.path().join("settings.toml");
    let config_store = ConfigStore::new(config_path.clone());
    let settings = AppSettings::default();
    assert!(settings.webview2_host_grouping);
    assert!(settings.show_leak_warnings);
    assert!(!settings.hide_windows_system_processes);

    let backend = NativeBackend::initialize().expect("initialize native backend");
    let coordinator =
        Arc::new(EngineCoordinator::new(backend, settings.clone()).expect("start coordinator"));
    let store = cx.new(|cx| UiStore::new(coordinator.clone(), config_store, settings, cx));

    let mut settings_view_handle = None;
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |window, cx| {
            let view = cx.new(|cx| SettingsView::new(store.clone(), window, cx));
            settings_view_handle = Some(view.clone());
            cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
        })
        .expect("open settings test window")
    });
    let settings_view = settings_view_handle.expect("SettingsView constructed");

    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    vcx.update(|window, cx| window.draw(cx).clear(cx));

    // Flip all 3 additive v1 toggles via SettingsView.
    vcx.update(|_window, cx| {
        settings_view.update(cx, |view, cx| {
            view.set_webview2_host_grouping(false, cx);
            view.set_show_leak_warnings(false, cx);
            view.set_hide_system_processes(true, cx);
        });
    });

    // Verify in-memory UiStore state updated immediately.
    vcx.update(|_window, cx| {
        let s = store.read(cx).settings();
        assert!(!s.webview2_host_grouping);
        assert!(!s.show_leak_warnings);
        assert!(s.hide_windows_system_processes);
    });

    // Verify persisted settings.toml reloads identically.
    let reloaded = ConfigStore::new(config_path)
        .load()
        .expect("reload persisted settings.toml");
    assert!(!reloaded.webview2_host_grouping);
    assert!(!reloaded.show_leak_warnings);
    assert!(reloaded.hide_windows_system_processes);

    // On platforms advertising TASK_MANAGER_REPLACEMENT / ADMIN_STARTUP (Windows),
    // SettingsView must query read-only status asynchronously on mount without panicking.
    let caps = NativeBackend::CAPABILITIES;
    if caps.contains(loom_types::CapabilityFlags::TASK_MANAGER_REPLACEMENT)
        || caps.contains(loom_types::CapabilityFlags::ADMIN_STARTUP)
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            vcx.run_until_parked();
            let ready = vcx.update(|_, cx| {
                let v = settings_view.read(cx);
                (!caps.contains(loom_types::CapabilityFlags::TASK_MANAGER_REPLACEMENT)
                    || v.taskmgr_status.is_some())
                    && (!caps.contains(loom_types::CapabilityFlags::ADMIN_STARTUP)
                        || v.admin_status.is_some())
            });
            if ready {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if caps.contains(loom_types::CapabilityFlags::TASK_MANAGER_REPLACEMENT) {
            let status = vcx.update(|_, cx| settings_view.read(cx).taskmgr_status.clone());
            assert!(
                status.is_some(),
                "SettingsView should resolve read-only TaskManagerReplacementStatus on mount"
            );
        }
        if caps.contains(loom_types::CapabilityFlags::ADMIN_STARTUP) {
            let status = vcx.update(|_, cx| settings_view.read(cx).admin_status.clone());
            assert!(
                status.is_some(),
                "SettingsView should resolve read-only AdminStartupStatus on mount"
            );
        }
        vcx.update(|window, cx| window.draw(cx).clear(cx));
    }

    drop(settings_view);
    vcx.update(|window, _| window.remove_window());
    vcx.run_until_parked();
    coordinator.shutdown();
}

#[gpui_kit::test]
fn settings_view_accent_color_defaults_to_system_switches_live_and_persists(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);

    let dir = tempfile::tempdir().expect("create temp accent config dir");
    let config_path = dir.path().join("settings.toml");
    let config_store = ConfigStore::new(config_path.clone());
    let settings = AppSettings {
        theme: ThemePreference::Light,
        ..AppSettings::default()
    };
    assert_eq!(settings.accent_color, AccentColorPreference::System);

    let backend = NativeBackend::initialize().expect("initialize native backend");
    let coordinator =
        Arc::new(EngineCoordinator::new(backend, settings.clone()).expect("start coordinator"));
    let store = cx.new(|cx| UiStore::new(coordinator.clone(), config_store, settings, cx));

    let mut settings_view_handle = None;
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |window, cx| {
            crate::bridge::apply_theme(
                ThemePreference::Light,
                AccentColorPreference::System,
                Some(window),
                cx,
            );
            let view = cx.new(|cx| SettingsView::new(store.clone(), window, cx));
            settings_view_handle = Some(view.clone());
            cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
        })
        .expect("open accent settings test window")
    });
    let settings_view = settings_view_handle.expect("SettingsView constructed");

    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    vcx.update(|window, cx| window.draw(cx).clear(cx));

    // Reject malformed hex strings without mutating active preference.
    vcx.update(|window, cx| {
        settings_view.update(cx, |view, cx| {
            assert!(!view.apply_custom_accent_hex("not-hex", window, cx));
        });
        assert_eq!(
            store.read(cx).settings().accent_color,
            AccentColorPreference::System
        );
    });

    // Choose a custom violet accent color (#8764B8) and verify live theme + persistence.
    let violet = AccentColorPreference::custom(0x87, 0x64, 0xB8);
    vcx.update(|window, cx| {
        settings_view.update(cx, |view, cx| {
            assert!(view.apply_custom_accent_hex("#8764B8", window, cx));
        });
    });
    vcx.update(|window, cx| {
        window.draw(cx).clear(cx);
        assert_eq!(store.read(cx).settings().accent_color, violet);
        let picker_rgb = settings_view
            .read(cx)
            .accent_color_picker
            .read(cx)
            .value()
            .map(hsla_to_rgb_u8);
        assert_eq!(picker_rgb, Some((0x87, 0x64, 0xB8)));
        // Active GPUI theme primary must adopt the violet hue (~265 deg).
        let primary_h_deg = cx.theme().primary.h * 360.0;
        assert!(
            (primary_h_deg - 265.0).abs() < 5.0,
            "expected violet primary hue near 265 deg, got {primary_h_deg:.1}"
        );
    });

    let reloaded_custom = ConfigStore::new(config_path.clone())
        .load()
        .expect("reload custom accent settings.toml");
    assert_eq!(reloaded_custom.accent_color, violet);

    // Toggle back to Follow System and verify persistence + remember last custom on re-disable.
    vcx.update(|window, cx| {
        settings_view.update(cx, |view, cx| {
            view.set_follow_system_accent(true, window, cx);
        });
    });
    vcx.update(|window, cx| {
        window.draw(cx).clear(cx);
        assert_eq!(
            store.read(cx).settings().accent_color,
            AccentColorPreference::System
        );
    });
    let reloaded_sys = ConfigStore::new(config_path)
        .load()
        .expect("reload system accent settings.toml");
    assert_eq!(reloaded_sys.accent_color, AccentColorPreference::System);

    vcx.update(|window, cx| {
        settings_view.update(cx, |view, cx| {
            view.set_follow_system_accent(false, window, cx);
        });
        assert_eq!(store.read(cx).settings().accent_color, violet);
    });

    drop(settings_view);
    vcx.update(|window, _| window.remove_window());
    vcx.run_until_parked();
    coordinator.shutdown();
}
