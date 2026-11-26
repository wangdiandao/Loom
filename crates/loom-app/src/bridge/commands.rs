//! Typed UI command helpers dispatching [`EngineCommand`]s and persisting [`loom_config::AppSettings`].

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Entity, Window};
use loom_engine::EngineCommand;
use loom_types::{
    LanguagePreference, ProcessKey, ProcessViewMode, RefreshSpeed, StartupPage, ThemePreference,
};

use crate::bootstrap;
use crate::bridge::UiStore;
use crate::i18n;

/// Applies a [`ThemePreference`] (`System` / `Light` / `Dark`) to GPUI Kit immediately.
pub(crate) fn apply_theme(pref: ThemePreference, window: Option<&mut Window>, cx: &mut App) {
    match pref {
        ThemePreference::System => Theme::sync_system_appearance(window, cx),
        ThemePreference::Light => Theme::change(ThemeMode::Light, window, cx),
        ThemePreference::Dark => Theme::change(ThemeMode::Dark, window, cx),
    }
}

/// Ergonomic namespace of typed UI command & preference mutation helpers.
pub(crate) struct UiCommands;

impl UiCommands {
    /// Updates the fast-lane refresh speed in the engine and persists it to `settings.toml`.
    pub(crate) fn set_refresh_speed(store: &Entity<UiStore>, speed: RefreshSpeed, cx: &mut App) {
        store.update(cx, |store, cx| {
            let _ = store.dispatch_command(EngineCommand::SetRefreshSpeed(speed));
            let _ = store.update_settings(|s| s.refresh_speed = speed, cx);
        });
    }

    /// Updates and immediately applies the application theme preference (`System` / `Light` / `Dark`).
    pub(crate) fn set_theme(
        store: &Entity<UiStore>,
        theme: ThemePreference,
        window: Option<&mut Window>,
        cx: &mut App,
    ) {
        store.update(cx, |store, cx| {
            let _ = store.update_settings(|s| s.theme = theme, cx);
        });
        apply_theme(theme, window, cx);
    }

    /// Updates and immediately applies the UI and tray localization (`zh-CN` / `en-US`).
    pub(crate) fn set_language(
        store: &Entity<UiStore>,
        language: LanguagePreference,
        cx: &mut App,
    ) {
        i18n::apply_locale(language);
        store.update(cx, |store, cx| {
            let _ = store.update_settings(|s| s.language = language, cx);
        });
        bootstrap::refresh_localized_shell_chrome(cx);
        cx.refresh_windows();
    }

    /// Updates and persists the default startup navigation page.
    pub(crate) fn set_startup_page(store: &Entity<UiStore>, page: StartupPage, cx: &mut App) {
        store.update(cx, |store, cx| {
            let _ = store.update_settings(|s| s.startup_page = page, cx);
        });
    }

    /// Updates, persists, and live-applies the native window Always-on-Top state.
    pub(crate) fn set_always_on_top(store: &Entity<UiStore>, enabled: bool, cx: &mut App) {
        store.update(cx, |store, cx| {
            let _ = store.update_settings(|s| s.always_on_top = enabled, cx);
        });
        bootstrap::apply_always_on_top(enabled);
    }

    /// Updates and persists whether minimizing hides Loom to the system tray.
    pub(crate) fn set_minimize_to_tray(store: &Entity<UiStore>, enabled: bool, cx: &mut App) {
        store.update(cx, |store, cx| {
            let _ = store.update_settings(|s| s.minimize_to_tray = enabled, cx);
        });
    }

    /// Updates and persists whether closing the window keeps Loom resident in the system tray.
    pub(crate) fn set_close_to_tray(store: &Entity<UiStore>, enabled: bool, cx: &mut App) {
        store.update(cx, |store, cx| {
            let _ = store.update_settings(|s| s.close_to_tray = enabled, cx);
        });
    }

    /// Updates and persists whether the left navigation sidebar is collapsed.
    pub(crate) fn set_sidebar_collapsed(store: &Entity<UiStore>, collapsed: bool, cx: &mut App) {
        store.update(cx, |store, cx| {
            let _ = store.update_settings(|s| s.sidebar_collapsed = collapsed, cx);
        });
    }

    /// Updates the global case-insensitive search query in the engine.
    pub(crate) fn set_search_query(store: &Entity<UiStore>, query: String, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::SetSearchQuery(query));
    }

    /// Switches the Processes view between Grouped and Process Tree modes and persists it.
    pub(crate) fn set_process_view_mode(
        store: &Entity<UiStore>,
        mode: ProcessViewMode,
        cx: &mut App,
    ) {
        store.update(cx, |store, cx| {
            let _ = store.dispatch_command(EngineCommand::SetProcessViewMode(mode));
            let _ = store.update_settings(|s| s.process_view_mode = mode, cx);
        });
    }

    /// Toggles expansion of a parent process row in the Processes view.
    pub(crate) fn toggle_process_expanded(store: &Entity<UiStore>, key: ProcessKey, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::ToggleProcessExpanded(key));
    }

    /// Requests an immediate fast-lane sampling pass (even when paused).
    pub(crate) fn force_refresh(store: &Entity<UiStore>, cx: &mut App) {
        let _ = store.read(cx).dispatch_command(EngineCommand::ForceRefresh);
    }

    /// Requests an on-demand elevated relaunch preserving the given route identifier.
    pub(crate) fn relaunch_elevated(store: &Entity<UiStore>, route: &str, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::RelaunchElevated(route.to_owned()));
    }
}
