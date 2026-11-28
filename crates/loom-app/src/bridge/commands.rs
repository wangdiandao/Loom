//! Typed UI command helpers dispatching [`EngineCommand`]s and persisting [`loom_config::AppSettings`].

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crossbeam_channel::Receiver;
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Entity, Window};
use loom_config::TableColumnSetting;
use loom_engine::{CommandEvent, EngineCommand, EngineCommandError};
use loom_pal::PalError;
use loom_types::{
    AccentColorPreference, AppHistorySortColumn, CpuAffinityMask, DetailsSortColumn,
    DetailsViewMode, LanguagePreference, ProcessKey, ProcessPriority, ProcessSortColumn,
    ProcessViewMode, RefreshSpeed, ResourceDisplayMode, RunTaskRequest, ServicesSortColumn,
    SortDirection, StartupPage, StartupSortColumn, ThemePreference, UsersSortColumn,
};
use loom_ui_kit::{DEFAULT_ACCENT_RGB, apply_accent_to_theme};

use crate::bootstrap;
use crate::bridge::UiStore;
use crate::i18n;

/// Returns the operating system's current accent color, falling back to [`DEFAULT_ACCENT_RGB`].
pub(crate) fn system_accent_rgb() -> (u8, u8, u8) {
    loom_pal::window::system_accent_color().unwrap_or(DEFAULT_ACCENT_RGB)
}

/// Resolves an [`AccentColorPreference`] into a concrete sRGB `(r, g, b)` accent color.
pub(crate) fn resolve_accent_rgb(accent: AccentColorPreference) -> (u8, u8, u8) {
    accent.rgb().unwrap_or_else(system_accent_rgb)
}

/// Applies a [`ThemePreference`] (`System` / `Light` / `Dark`) and [`AccentColorPreference`]
/// to GPUI Kit immediately, returning the queried OS system accent color when `accent` tracks
/// the system accent so callers do not repeat registry/DWM queries on startup.
pub(crate) fn apply_theme(
    pref: ThemePreference,
    accent: AccentColorPreference,
    window: Option<&mut Window>,
    cx: &mut App,
) -> Option<(u8, u8, u8)> {
    match pref {
        ThemePreference::System => Theme::sync_system_appearance(window, cx),
        ThemePreference::Light => Theme::change(ThemeMode::Light, window, cx),
        ThemePreference::Dark => Theme::change(ThemeMode::Dark, window, cx),
    }
    let (accent_rgb, queried_sys) = match accent.rgb() {
        Some(custom) => (custom, None),
        None => {
            let sys = system_accent_rgb();
            (sys, Some(sys))
        }
    };
    Theme::update(cx, |theme| {
        apply_accent_to_theme(theme, accent_rgb);
    });
    queried_sys
}

/// Ergonomic namespace of typed UI command & preference mutation helpers.
pub(crate) struct UiCommands;

impl UiCommands {
    /// Updates the fast-lane refresh speed in the engine and persists it to `settings.toml`.
    pub(crate) fn set_refresh_speed(store: &Entity<UiStore>, speed: RefreshSpeed, cx: &mut App) {
        store.update(cx, |store, cx| {
            store.record_active_refresh_speed(speed);
            let _ = store.dispatch_command(EngineCommand::SetRefreshSpeed(speed));
            let _ = store.update_settings(|s| s.refresh_speed = speed, cx);
        });
    }

    /// Toggles between [`RefreshSpeed::Paused`] and the user's previously active refresh speed.
    ///
    /// Returns the newly active [`RefreshSpeed`].
    pub(crate) fn toggle_pause_refresh(store: &Entity<UiStore>, cx: &mut App) -> RefreshSpeed {
        let next = store.update(cx, |store, _| store.next_pause_toggle_speed());
        Self::set_refresh_speed(store, next, cx);
        next
    }

    /// Triggers an immediate sampling and projection pass even while paused.
    pub(crate) fn force_refresh(store: &Entity<UiStore>, cx: &mut App) {
        let _ = store.read(cx).dispatch_command(EngineCommand::ForceRefresh);
    }

    /// Temporarily freezes or unfreezes live snapshot updates (e.g., Hold `Ctrl`)
    /// without altering the user's persisted [`RefreshSpeed`].
    pub(crate) fn set_temp_freeze(store: &Entity<UiStore>, frozen: bool, cx: &mut App) {
        store.update(cx, |store, cx| {
            store.set_temp_frozen(frozen, cx);
        });
    }

    /// Freezes or unfreezes displayed row order in the Processes view while live metrics keep updating.
    #[allow(dead_code)]
    pub(crate) fn set_process_order_freeze(store: &Entity<UiStore>, frozen: bool, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::SetProcessOrderFreeze(frozen));
    }

    /// Freezes or unfreezes displayed row order in the Details view while live metrics keep updating.
    #[allow(dead_code)]
    pub(crate) fn set_details_order_freeze(store: &Entity<UiStore>, frozen: bool, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::SetDetailsOrderFreeze(frozen));
    }

    /// Updates and immediately applies the application theme preference (`System` / `Light` / `Dark`).
    pub(crate) fn set_theme(
        store: &Entity<UiStore>,
        theme: ThemePreference,
        window: Option<&mut Window>,
        cx: &mut App,
    ) {
        let accent = store.update(cx, |store, cx| {
            let _ = store.update_settings(|s| s.theme = theme, cx);
            store.settings().accent_color
        });
        apply_theme(theme, accent, window, cx);
    }

    /// Updates and immediately applies the application theme accent color (`System` or custom hex).
    pub(crate) fn set_accent_color(
        store: &Entity<UiStore>,
        accent: AccentColorPreference,
        window: Option<&mut Window>,
        cx: &mut App,
    ) {
        let theme = store.update(cx, |store, cx| {
            let _ = store.update_settings(|s| s.accent_color = accent, cx);
            store.settings().theme
        });
        apply_theme(theme, accent, window, cx);
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
        if store.read(cx).settings().startup_page == page {
            return;
        }
        store.update(cx, |store, cx| {
            let _ = store.update_settings(|s| s.startup_page = page, cx);
        });
    }

    /// Updates, persists, and live-applies the native window Always-on-Top state.
    #[allow(dead_code)]
    pub(crate) fn set_always_on_top(store: &Entity<UiStore>, enabled: bool, cx: &mut App) {
        if store.read(cx).settings().always_on_top == enabled {
            return;
        }
        store.update(cx, |store, cx| {
            let _ = store.update_settings(|s| s.always_on_top = enabled, cx);
        });
        bootstrap::apply_always_on_top(enabled);
    }

    /// Updates and persists whether minimizing hides Loom to the system tray.
    pub(crate) fn set_minimize_to_tray(store: &Entity<UiStore>, enabled: bool, cx: &mut App) {
        if store.read(cx).settings().minimize_to_tray == enabled {
            return;
        }
        store.update(cx, |store, cx| {
            let _ = store.update_settings(|s| s.minimize_to_tray = enabled, cx);
        });
    }

    /// Updates and persists whether closing the window keeps Loom resident in the system tray.
    pub(crate) fn set_close_to_tray(store: &Entity<UiStore>, enabled: bool, cx: &mut App) {
        if store.read(cx).settings().close_to_tray == enabled {
            return;
        }
        store.update(cx, |store, cx| {
            let _ = store.update_settings(|s| s.close_to_tray = enabled, cx);
        });
    }

    /// Updates and persists whether the left navigation sidebar is collapsed.
    pub(crate) fn set_sidebar_collapsed(store: &Entity<UiStore>, collapsed: bool, cx: &mut App) {
        if store.read(cx).settings().sidebar_collapsed == collapsed {
            return;
        }
        store.update(cx, |store, cx| {
            let _ = store.update_settings(|s| s.sidebar_collapsed = collapsed, cx);
        });
    }

    /// Updates the global case-insensitive search query in [`UiStore`] and the engine.
    pub(crate) fn set_search_query(store: &Entity<UiStore>, query: String, cx: &mut App) {
        store.update(cx, |store, cx| {
            store.set_search_query(query, cx);
        });
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

    /// Enables or disables attributing `msedgewebview2.exe` processes under their hosting app group.
    #[allow(dead_code)]
    pub(crate) fn set_webview2_host_grouping(store: &Entity<UiStore>, enabled: bool, cx: &mut App) {
        store.update(cx, |store, cx| {
            let _ = store.dispatch_command(EngineCommand::SetWebView2HostGrouping(enabled));
            let _ = store.update_settings(|s| s.webview2_host_grouping = enabled, cx);
        });
    }

    /// Enables or disables advisory in-memory resource leak warnings on process rows.
    #[allow(dead_code)]
    pub(crate) fn set_show_leak_warnings(store: &Entity<UiStore>, enabled: bool, cx: &mut App) {
        store.update(cx, |store, cx| {
            let _ = store.dispatch_command(EngineCommand::SetShowLeakWarnings(enabled));
            let _ = store.update_settings(|s| s.show_leak_warnings = enabled, cx);
        });
    }

    /// Adds an executable path or image name to the resource-leak ignore list and persists it.
    #[allow(dead_code)]
    pub(crate) fn ignore_leak_path(store: &Entity<UiStore>, path: String, cx: &mut App) {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return;
        }
        let normalized = trimmed.to_owned();
        store.update(cx, |store, cx| {
            let _ = store.dispatch_command(EngineCommand::IgnoreLeakPath(normalized.clone()));
            let _ = store.update_settings(
                |s| {
                    if !s.leak_ignored_paths.iter().any(|p| p == &normalized) {
                        s.leak_ignored_paths.push(normalized);
                    }
                },
                cx,
            );
        });
    }

    /// Hides or shows core Windows system processes in process lists and persists the preference.
    #[allow(dead_code)]
    pub(crate) fn set_hide_system_processes(store: &Entity<UiStore>, hide: bool, cx: &mut App) {
        store.update(cx, |store, cx| {
            let _ = store.dispatch_command(EngineCommand::SetHideSystemProcesses(hide));
            let _ = store.update_settings(|s| s.hide_windows_system_processes = hide, cx);
        });
    }

    /// Toggles expansion of a parent process row in the Processes view.
    pub(crate) fn toggle_process_expanded(store: &Entity<UiStore>, key: ProcessKey, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::ToggleProcessExpanded(key));
    }

    /// Requests an on-demand elevated relaunch preserving the given route identifier.
    pub(crate) fn relaunch_elevated(store: &Entity<UiStore>, route: &str, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::RelaunchElevated(route.to_owned()));
    }

    /// Launches a new task via the slow-lane operator.
    pub(crate) fn run_new_task(store: &Entity<UiStore>, req: RunTaskRequest, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::RunNewTask(req));
    }

    /// Terminates a single process verified by [`ProcessKey`].
    pub(crate) fn terminate_process(store: &Entity<UiStore>, key: ProcessKey, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::TerminateProcess(key));
    }

    /// Terminates a verified process tree in bottom-up (leaves-to-root) order.
    pub(crate) fn terminate_process_tree(store: &Entity<UiStore>, key: ProcessKey, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::TerminateProcessTree(key));
    }

    /// Suspends all threads of a non-critical process verified by [`ProcessKey`].
    #[allow(dead_code)]
    pub(crate) fn suspend_process(store: &Entity<UiStore>, key: ProcessKey, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::SuspendProcess(key));
    }

    /// Resumes all threads of a previously suspended process verified by [`ProcessKey`].
    #[allow(dead_code)]
    pub(crate) fn resume_process(store: &Entity<UiStore>, key: ProcessKey, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::ResumeProcess(key));
    }

    /// Safely restarts the desktop shell / Windows Explorer on the slow lane.
    pub(crate) fn restart_explorer(store: &Entity<UiStore>, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::RestartExplorer);
    }

    /// Queries processes holding open file or directory locks on `path` via the slow lane,
    /// returning a oneshot reply receiver with [`loom_engine::CommandSuccess::FileLocks`].
    pub(crate) fn find_file_locks(
        store: &Entity<UiStore>,
        path: PathBuf,
        cx: &App,
    ) -> Result<Receiver<CommandEvent>, EngineCommandError> {
        store
            .read(cx)
            .dispatch_command_with_reply(EngineCommand::FindFileLocks { path })
    }

    /// Enables or disables Efficiency Mode (`EcoQoS`) for `key`.
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
    pub(crate) fn get_affinity_async(
        store: &Entity<UiStore>,
        key: ProcessKey,
        cx: &App,
    ) -> Result<Receiver<CommandEvent>, EngineCommandError> {
        store
            .read(cx)
            .dispatch_command_with_reply(EngineCommand::GetAffinity(key))
    }

    /// Enables or disables UAC virtualization for `key`.
    pub(crate) fn set_uac_virtualization(
        store: &Entity<UiStore>,
        key: ProcessKey,
        enabled: bool,
        cx: &mut App,
    ) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::SetUacVirtualization { key, enabled });
    }

    /// Inspects the wait chain / deadlock graph for `key` on the slow lane, returning a oneshot reply receiver.
    pub(crate) fn analyze_wait_chain_async(
        store: &Entity<UiStore>,
        key: ProcessKey,
        cx: &App,
    ) -> Result<Receiver<CommandEvent>, EngineCommandError> {
        store
            .read(cx)
            .dispatch_command_with_reply(EngineCommand::AnalyzeWaitChain(key))
    }

    /// Streams a cancellable process memory minidump to `dest` on the slow lane, returning a oneshot reply receiver.
    pub(crate) fn write_minidump_async(
        store: &Entity<UiStore>,
        key: ProcessKey,
        dest: PathBuf,
        cancel: Arc<AtomicBool>,
        cx: &App,
    ) -> Result<Receiver<CommandEvent>, EngineCommandError> {
        store
            .read(cx)
            .dispatch_command_with_reply(EngineCommand::WriteMiniDump { key, dest, cancel })
    }

    /// Launches the OS-configured JIT debugger attached to `key` on the slow lane.
    pub(crate) fn attach_debugger(store: &Entity<UiStore>, key: ProcessKey, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::AttachDebugger(key));
    }

    /// Registers or clears the system-wide global wakeup hotkey live and persists it to `settings.toml`.
    pub(crate) fn set_global_hotkey(
        store: &Entity<UiStore>,
        hotkey: Option<String>,
        cx: &mut App,
    ) -> Result<(), PalError> {
        let normalized = hotkey.and_then(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        });
        bootstrap::apply_global_hotkey(normalized.as_deref(), cx)?;
        store.update(cx, |store, cx| {
            let _ = store.update_settings(|s| s.global_hotkey = normalized, cx);
        });
        Ok(())
    }

    /// Opens the native executable file picker on the slow lane, returning a oneshot reply receiver.
    pub(crate) fn browse_executable_async(
        store: &Entity<UiStore>,
        cx: &App,
    ) -> Result<Receiver<CommandEvent>, EngineCommandError> {
        store
            .read(cx)
            .dispatch_command_with_reply(EngineCommand::BrowseExecutable)
    }

    /// Opens the OS file explorer and selects `path`.
    pub(crate) fn open_file_location(store: &Entity<UiStore>, path: PathBuf, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::OpenFileLocation(path));
    }

    /// Opens the native OS properties dialog for `path`.
    pub(crate) fn show_file_properties(store: &Entity<UiStore>, path: PathBuf, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::ShowFileProperties(path));
    }

    /// Opens the user's default browser searching online for `query`.
    pub(crate) fn search_online(store: &Entity<UiStore>, query: String, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::SearchOnline(query));
    }

    /// Updates the sort column and direction of the Processes table.
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
    pub(crate) fn toggle_details_expanded(store: &Entity<UiStore>, key: ProcessKey, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::ToggleDetailsExpanded(key));
    }

    /// Updates and atomically persists per-column resource display modes (`Values` vs. `Percent`).
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
    pub(crate) fn record_run_task_history(store: &Entity<UiStore>, command: String, cx: &mut App) {
        store.update(cx, |store, cx| {
            let _ = store.update_settings(|s| s.push_run_task_history(command), cx);
        });
    }

    /// Persists the Processes table column layout (order, widths, visibility) to `settings.toml`.
    pub(crate) fn set_processes_columns(
        store: &Entity<UiStore>,
        columns: Vec<TableColumnSetting>,
        cx: &mut App,
    ) {
        store.update(cx, |store, cx| {
            let _ = store.update_settings(|s| s.processes_columns = columns, cx);
        });
    }

    /// Persists the Details table column layout (order, widths, visibility) to `settings.toml`.
    pub(crate) fn set_details_columns(
        store: &Entity<UiStore>,
        columns: Vec<TableColumnSetting>,
        cx: &mut App,
    ) {
        store.update(cx, |store, cx| {
            let _ = store.update_settings(|s| s.details_columns = columns, cx);
        });
    }

    /// Starts an OS system service identified by its internal name.
    pub(crate) fn start_service(store: &Entity<UiStore>, name: String, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::StartService(name));
    }

    /// Stops an OS system service identified by its internal name.
    pub(crate) fn stop_service(store: &Entity<UiStore>, name: String, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::StopService(name));
    }

    /// Restarts an OS system service identified by its internal name.
    pub(crate) fn restart_service(store: &Entity<UiStore>, name: String, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::RestartService(name));
    }

    /// Launches the native OS services management console (e.g., `services.msc`).
    pub(crate) fn open_services_console(store: &Entity<UiStore>, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::OpenServicesConsole);
    }

    /// Enables or disables a registered startup application entry by its stable ID.
    pub(crate) fn set_startup_enabled(
        store: &Entity<UiStore>,
        id: String,
        enabled: bool,
        cx: &mut App,
    ) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::SetStartupEnabled { id, enabled });
    }

    /// Disconnects the logged-in OS user session identified by `session_id`.
    pub(crate) fn disconnect_session(store: &Entity<UiStore>, session_id: u32, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::DisconnectSession(session_id));
    }

    /// Clears persisted and live in-memory cumulative application usage history.
    pub(crate) fn clear_app_history(store: &Entity<UiStore>, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::ClearAppHistory);
    }

    /// Updates the sort column and direction of the Services table on the backend engine.
    pub(crate) fn set_services_sort(
        store: &Entity<UiStore>,
        column: ServicesSortColumn,
        direction: SortDirection,
        cx: &mut App,
    ) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::SetServicesSort { column, direction });
    }

    /// Updates or clears the optional hosting PID filter of the Services table on the backend engine.
    pub(crate) fn set_services_filter_pid(store: &Entity<UiStore>, pid: Option<u32>, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::SetServicesFilterPid(pid));
    }

    /// Updates the sort column and direction of the Startup Applications table on the backend engine.
    pub(crate) fn set_startup_sort(
        store: &Entity<UiStore>,
        column: StartupSortColumn,
        direction: SortDirection,
        cx: &mut App,
    ) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::SetStartupSort { column, direction });
    }

    /// Updates the sort column and direction of the Users table on the backend engine.
    pub(crate) fn set_users_sort(
        store: &Entity<UiStore>,
        column: UsersSortColumn,
        direction: SortDirection,
        cx: &mut App,
    ) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::SetUsersSort { column, direction });
    }

    /// Toggles the expanded/collapsed state of a user session row in the Users table on the backend engine.
    pub(crate) fn toggle_user_session_expanded(
        store: &Entity<UiStore>,
        session_id: u32,
        cx: &mut App,
    ) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::ToggleUserSessionExpanded(session_id));
    }

    /// Updates the sort column and direction of the App History table on the backend engine.
    pub(crate) fn set_app_history_sort(
        store: &Entity<UiStore>,
        column: AppHistorySortColumn,
        direction: SortDirection,
        cx: &mut App,
    ) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::SetAppHistorySort { column, direction });
    }

    /// Queries the current OS Task Manager replacement status on the slow lane.
    pub(crate) fn query_task_manager_replacement_async(
        store: &Entity<UiStore>,
        cx: &App,
    ) -> Result<Receiver<CommandEvent>, EngineCommandError> {
        store
            .read(cx)
            .dispatch_command_with_reply(EngineCommand::QueryTaskManagerReplacement)
    }

    /// Enables or restores OS Task Manager replacement on the slow lane.
    pub(crate) fn set_task_manager_replacement_async(
        store: &Entity<UiStore>,
        enabled: bool,
        cx: &App,
    ) -> Result<Receiver<CommandEvent>, EngineCommandError> {
        store
            .read(cx)
            .dispatch_command_with_reply(EngineCommand::SetTaskManagerReplacement(enabled))
    }

    /// Launches the built-in OS Task Manager directly (bypassing any active IFEO entry).
    pub(crate) fn open_native_task_manager(store: &Entity<UiStore>, cx: &mut App) {
        let _ = store
            .read(cx)
            .dispatch_command(EngineCommand::OpenNativeTaskManager);
    }

    /// Queries the current persistent administrator startup status on the slow lane.
    pub(crate) fn query_admin_startup_async(
        store: &Entity<UiStore>,
        cx: &App,
    ) -> Result<Receiver<CommandEvent>, EngineCommandError> {
        store
            .read(cx)
            .dispatch_command_with_reply(EngineCommand::QueryAdminStartup)
    }

    /// Enables or disables persistent administrator startup on the slow lane.
    pub(crate) fn set_admin_startup_async(
        store: &Entity<UiStore>,
        enabled: bool,
        cx: &App,
    ) -> Result<Receiver<CommandEvent>, EngineCommandError> {
        store
            .read(cx)
            .dispatch_command_with_reply(EngineCommand::SetAdminStartup(enabled))
    }
}
