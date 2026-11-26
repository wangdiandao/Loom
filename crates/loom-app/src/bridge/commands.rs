//! Typed UI command helpers dispatching [`EngineCommand`]s and persisting [`loom_config::AppSettings`].

use std::path::PathBuf;

use crossbeam_channel::Receiver;
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Entity, Window};
use loom_engine::{CommandEvent, EngineCommand, EngineCommandError};
use loom_types::{
    CpuAffinityMask, DetailsSortColumn, DetailsViewMode, LanguagePreference, ProcessKey,
    ProcessPriority, ProcessSortColumn, ProcessViewMode, RefreshSpeed, ResourceDisplayMode,
    RunTaskRequest, SortDirection, StartupPage, ThemePreference,
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

    /// Launches a new task via the slow-lane operator.
    #[allow(dead_code)] // Consumed by 0.0.3 Run New Task dialog (Issue #10).
    pub(crate) fn run_new_task(store: &Entity<UiStore>, req: RunTaskRequest, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::RunNewTask(req));
    }

    /// Terminates a single process verified by [`ProcessKey`].
    #[allow(dead_code)] // Consumed by 0.0.3 Processes & Details slices (Issues #10 & #11).
    pub(crate) fn terminate_process(store: &Entity<UiStore>, key: ProcessKey, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::TerminateProcess(key));
    }

    /// Terminates a verified process tree in bottom-up (leaves-to-root) order.
    #[allow(dead_code)] // Consumed by 0.0.3 Details slice (Issue #11).
    pub(crate) fn terminate_process_tree(store: &Entity<UiStore>, key: ProcessKey, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::TerminateProcessTree(key));
    }

    /// Enables or disables Efficiency Mode (`EcoQoS`) for `key`.
    #[allow(dead_code)] // Consumed by 0.0.3 Processes & Details slices (Issues #10 & #11).
    pub(crate) fn set_efficiency_mode(
        store: &Entity<UiStore>,
        key: ProcessKey,
        enabled: bool,
        cx: &mut App,
    ) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::SetEfficiencyMode { key, enabled });
    }

    /// Sets the scheduling priority class of `key`.
    #[allow(dead_code)] // Consumed by 0.0.3 Details slice (Issue #11).
    pub(crate) fn set_priority(
        store: &Entity<UiStore>,
        key: ProcessKey,
        priority: ProcessPriority,
        cx: &mut App,
    ) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::SetPriority { key, priority });
    }

    /// Applies a new multi-group CPU affinity mask to `key`.
    #[allow(dead_code)] // Consumed by 0.0.3 Set Affinity dialog (Issue #11).
    pub(crate) fn set_affinity(
        store: &Entity<UiStore>,
        key: ProcessKey,
        mask: CpuAffinityMask,
        cx: &mut App,
    ) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::SetAffinity { key, mask });
    }

    /// Queries the current multi-group CPU affinity mask of `key`, returning a oneshot reply receiver.
    #[allow(dead_code)] // Consumed by 0.0.3 Set Affinity dialog (Issue #11).
    pub(crate) fn get_affinity_async(
        store: &Entity<UiStore>,
        key: ProcessKey,
        cx: &App,
    ) -> Result<Receiver<CommandEvent>, EngineCommandError> {
        store
            .read(cx)
            .dispatch_command_with_reply(EngineCommand::GetAffinity(key))
    }

    /// Opens the native executable file picker on the slow lane, returning a oneshot reply receiver.
    #[allow(dead_code)] // Consumed by 0.0.3 Run New Task dialog (Issue #10).
    pub(crate) fn browse_executable_async(
        store: &Entity<UiStore>,
        cx: &App,
    ) -> Result<Receiver<CommandEvent>, EngineCommandError> {
        store
            .read(cx)
            .dispatch_command_with_reply(EngineCommand::BrowseExecutable)
    }

    /// Opens the OS file explorer and selects `path`.
    #[allow(dead_code)] // Consumed by 0.0.3 Processes & Details context menus (Issues #10 & #11).
    pub(crate) fn open_file_location(store: &Entity<UiStore>, path: PathBuf, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::OpenFileLocation(path));
    }

    /// Opens the native OS properties dialog for `path`.
    #[allow(dead_code)] // Consumed by 0.0.3 Processes & Details context menus (Issues #10 & #11).
    pub(crate) fn show_file_properties(store: &Entity<UiStore>, path: PathBuf, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::ShowFileProperties(path));
    }

    /// Opens the user's default browser searching online for `query`.
    #[allow(dead_code)] // Consumed by 0.0.3 Processes & Details context menus (Issues #10 & #11).
    pub(crate) fn search_online(store: &Entity<UiStore>, query: String, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::SearchOnline(query));
    }

    /// Updates the sort column and direction of the Processes table.
    #[allow(dead_code)] // Consumed by 0.0.3 Processes table sorting (Issue #10).
    pub(crate) fn set_process_sort(
        store: &Entity<UiStore>,
        column: ProcessSortColumn,
        direction: SortDirection,
        cx: &mut App,
    ) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::SetProcessSort { column, direction });
    }

    /// Updates the sort column and direction of the Details table.
    #[allow(dead_code)] // Consumed by 0.0.3 Details table sorting (Issue #11).
    pub(crate) fn set_details_sort(
        store: &Entity<UiStore>,
        column: DetailsSortColumn,
        direction: SortDirection,
        cx: &mut App,
    ) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::SetDetailsSort { column, direction });
    }

    /// Switches the Details view between Flat and Process Tree modes.
    #[allow(dead_code)] // Consumed by 0.0.3 Details toolbar (Issue #11).
    pub(crate) fn set_details_view_mode(
        store: &Entity<UiStore>,
        mode: DetailsViewMode,
        cx: &mut App,
    ) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::SetDetailsViewMode(mode));
    }

    /// Toggles expansion of a parent process row in the Details tree view.
    #[allow(dead_code)] // Consumed by 0.0.3 Details tree view (Issue #11).
    pub(crate) fn toggle_details_expanded(store: &Entity<UiStore>, key: ProcessKey, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::ToggleDetailsExpanded(key));
    }

    /// Updates and atomically persists per-column resource display modes (`Values` vs. `Percent`).
    #[allow(dead_code)] // Consumed by 0.0.3 Processes Resource Values submenu (Issue #10).
    pub(crate) fn set_resource_display_modes(
        store: &Entity<UiStore>,
        memory: ResourceDisplayMode,
        disk: ResourceDisplayMode,
        network: ResourceDisplayMode,
        cx: &mut App,
    ) {
        store.update(cx, |store, cx| {
            let _ = store.update_settings(
                |s| {
                    s.memory_display_mode = memory;
                    s.disk_display_mode = disk;
                    s.network_display_mode = network;
                },
                cx,
            );
        });
    }

    /// Appends a command to the MRU "Run new task" history and persists it atomically.
    #[allow(dead_code)] // Consumed by 0.0.3 Run New Task dialog (Issue #10).
    pub(crate) fn record_run_task_history(store: &Entity<UiStore>, command: String, cx: &mut App) {
        store.update(cx, |store, cx| {
            let _ = store.update_settings(|s| s.push_run_task_history(command), cx);
        });
    }
}
