//! Localization helpers wrapping `rust-i18n` for live `zh-CN` / `en-US` switching.

use loom_types::{
    DiskMediaType, LanguagePreference, NetConnectionType, PlatformKind, PlatformProfile,
    ProcessArch, ProcessCategory, ProcessPriority, RefreshSpeed, ServiceState, StartupImpact,
    StartupPage, ThemePreference, UacVirtualization, UserSessionState,
};

/// Applies `preference` to `rust-i18n`'s active global locale immediately.
pub(crate) fn apply_locale(preference: LanguagePreference) {
    rust_i18n::set_locale(preference.locale_tag());
}

/// Returns the localized translation string for `key` using the currently active locale.
#[inline]
pub(crate) fn tr(key: &str) -> String {
    rust_i18n::t!(key).into_owned()
}

/// Returns the localized label for a [`StartupPage`].
pub(crate) fn startup_page_label(page: StartupPage) -> String {
    match page {
        StartupPage::Processes => tr("nav.processes"),
        StartupPage::Performance => tr("nav.performance"),
        StartupPage::AppHistory => tr("nav.app_history"),
        StartupPage::Startup => tr("nav.startup"),
        StartupPage::Users => tr("nav.users"),
        StartupPage::Details => tr("nav.details"),
        StartupPage::Services => tr("nav.services"),
    }
}

/// Returns the localized label for a [`RefreshSpeed`].
pub(crate) fn refresh_speed_label(speed: RefreshSpeed) -> String {
    match speed {
        RefreshSpeed::High500Ms => tr("settings.speed_high"),
        RefreshSpeed::Normal1S => tr("settings.speed_normal"),
        RefreshSpeed::Low2S => tr("settings.speed_low"),
        RefreshSpeed::Paused => tr("settings.speed_paused"),
    }
}

/// Returns the localized label for a [`ThemePreference`].
pub(crate) fn theme_preference_label(theme: ThemePreference) -> String {
    match theme {
        ThemePreference::System => tr("settings.theme_system"),
        ThemePreference::Light => tr("settings.theme_light"),
        ThemePreference::Dark => tr("settings.theme_dark"),
    }
}

/// Returns the localized label for a [`LanguagePreference`].
pub(crate) fn language_preference_label(lang: LanguagePreference) -> String {
    match lang {
        LanguagePreference::ZhCn => tr("settings.lang_zh_cn"),
        LanguagePreference::EnUs => tr("settings.lang_en_us"),
    }
}

/// Returns the localized label for a [`ProcessCategory`] adapted to `platform`.
pub(crate) fn process_category_label(
    category: ProcessCategory,
    platform: PlatformProfile,
) -> String {
    match category {
        ProcessCategory::App => tr("processes.cat_apps"),
        ProcessCategory::Background => tr("processes.cat_background"),
        ProcessCategory::System => match platform.kind {
            PlatformKind::Windows => tr("processes.cat_windows"),
            PlatformKind::Macos => tr("processes.cat_macos"),
            PlatformKind::Linux | PlatformKind::Unknown => tr("processes.cat_system"),
        },
    }
}

/// Returns the localized Details virtualization/sandbox column header adapted to `platform`.
pub(crate) fn virtualization_column_label(platform: PlatformProfile) -> String {
    match platform.kind {
        PlatformKind::Windows => tr("details.col_uac"),
        PlatformKind::Linux | PlatformKind::Macos | PlatformKind::Unknown => {
            tr("details.col_sandbox")
        }
    }
}

/// Returns the localized label for a [`ProcessPriority`].
pub(crate) fn process_priority_label(priority: ProcessPriority) -> String {
    match priority {
        ProcessPriority::Idle => tr("details.priority_idle"),
        ProcessPriority::BelowNormal => tr("details.priority_below_normal"),
        ProcessPriority::Normal => tr("details.priority_normal"),
        ProcessPriority::AboveNormal => tr("details.priority_above_normal"),
        ProcessPriority::High => tr("details.priority_high"),
        ProcessPriority::Realtime => tr("details.priority_realtime"),
    }
}

/// Returns the localized label for a [`ProcessArch`].
pub(crate) fn process_arch_label(arch: ProcessArch) -> String {
    match arch {
        ProcessArch::X64 => tr("details.arch_x64"),
        ProcessArch::X86 => tr("details.arch_x86"),
        ProcessArch::Arm64 => tr("details.arch_arm64"),
        ProcessArch::Unknown => tr("details.arch_unknown"),
    }
}

/// Returns the localized label for a [`UacVirtualization`] state.
pub(crate) fn uac_virtualization_label(uac: UacVirtualization) -> String {
    match uac {
        UacVirtualization::Enabled => tr("details.uac_enabled"),
        UacVirtualization::Disabled => tr("details.uac_disabled"),
        UacVirtualization::NotAllowed => tr("details.uac_not_allowed"),
        UacVirtualization::Unknown => tr("details.uac_unknown"),
    }
}

/// Returns the localized label for a [`DiskMediaType`].
pub(crate) fn disk_media_type_label(media_type: DiskMediaType) -> String {
    match media_type {
        DiskMediaType::Nvme => tr("perf.media_nvme"),
        DiskMediaType::Ssd => tr("perf.media_ssd"),
        DiskMediaType::Hdd => tr("perf.media_hdd"),
        DiskMediaType::Unknown => tr("perf.media_unknown"),
    }
}

/// Returns the localized label for a [`NetConnectionType`].
pub(crate) fn net_connection_type_label(conn_type: NetConnectionType) -> String {
    match conn_type {
        NetConnectionType::Ethernet => tr("perf.ethernet"),
        NetConnectionType::WiFi => tr("perf.wifi"),
        NetConnectionType::Other => tr("perf.network_other"),
    }
}

/// Formats a localized disk title (`"Disk 0 (C: D:)"` or `"Disk 0"`).
pub(crate) fn perf_disk_title(index: u32, drive_letters: &str) -> String {
    let trimmed = drive_letters.trim();
    if trimmed.is_empty() {
        rust_i18n::t!("perf.disk_numbered", index = index).into_owned()
    } else {
        rust_i18n::t!("perf.disk_with_drives", index = index, drives = trimmed).into_owned()
    }
}

/// Formats a localized GPU display title (`"GPU 0"`).
#[inline]
pub(crate) fn perf_gpu_title(index: u32) -> String {
    rust_i18n::t!("perf.gpu_numbered", index = index).into_owned()
}

/// Formats a localized GPU engine-pane count option (`"1 engine"` / `"4 engines"`).
#[inline]
pub(crate) fn perf_gpu_engine_count_label(count: usize) -> String {
    if count == 1 {
        tr("perf.gpu_engine_count_one")
    } else {
        rust_i18n::t!("perf.gpu_engine_count_many", count = count).into_owned()
    }
}

/// Formats a localized logical CPU cell label (`"CPU 0"`).
#[inline]
pub(crate) fn perf_cpu_core_label(index: usize) -> String {
    rust_i18n::t!("perf.cpu_core_label", index = index).into_owned()
}

/// Formats a localized relative time label for a 60-second chart hover tooltip.
#[inline]
pub(crate) fn perf_time_offset_label(secs_ago: usize) -> String {
    if secs_ago == 0 {
        tr("perf.now")
    } else {
        rust_i18n::t!("perf.seconds_ago", secs = secs_ago).into_owned()
    }
}

/// Returns the localized label for a [`ServiceState`].
pub(crate) fn service_state_label(state: ServiceState) -> String {
    match state {
        ServiceState::Running => tr("services.state_running"),
        ServiceState::Stopped => tr("services.state_stopped"),
        ServiceState::StartPending => tr("services.state_start_pending"),
        ServiceState::StopPending => tr("services.state_stop_pending"),
        ServiceState::Paused => tr("services.state_paused"),
        ServiceState::PausePending => tr("services.state_pause_pending"),
        ServiceState::ContinuePending => tr("services.state_continue_pending"),
        ServiceState::Unknown => tr("services.state_unknown"),
    }
}

/// Returns the localized label for a [`StartupImpact`].
pub(crate) fn startup_impact_label(impact: StartupImpact) -> String {
    match impact {
        StartupImpact::High => tr("startup.impact_high"),
        StartupImpact::Medium => tr("startup.impact_medium"),
        StartupImpact::Low => tr("startup.impact_low"),
        StartupImpact::NotMeasured => tr("startup.impact_not_measured"),
        StartupImpact::None => tr("startup.impact_none"),
    }
}

/// Returns the localized label for a [`UserSessionState`].
pub(crate) fn user_session_state_label(state: UserSessionState) -> String {
    match state {
        UserSessionState::Active => tr("users.state_active"),
        UserSessionState::Disconnected => tr("users.state_disconnected"),
        UserSessionState::Idle => tr("users.state_idle"),
        UserSessionState::Unknown => tr("users.state_unknown"),
    }
}

/// Formats the localized "Last BIOS time: X.X s" indicator for the Startup Apps toolbar.
#[inline]
pub(crate) fn startup_last_bios_time(seconds: &str) -> String {
    rust_i18n::t!("startup.last_bios_time", seconds = seconds).into_owned()
}

/// Formats the localized "Resource usage since YYYY-MM-DD HH:MM" indicator for App History.
#[inline]
pub(crate) fn app_history_since(datetime: &str) -> String {
    rust_i18n::t!("app_history.since", datetime = datetime).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CORE_KEYS: &[&str] = &[
        "app.title",
        "app.window_title",
        "app.admin_badge",
        "app.search_placeholder",
        "app.sidebar_toggle",
        "nav.processes",
        "nav.performance",
        "nav.app_history",
        "nav.startup",
        "nav.users",
        "nav.details",
        "nav.services",
        "nav.settings",
        "processes.title",
        "processes.view_grouped",
        "processes.view_tree",
        "processes.run_new_task",
        "processes.end_task",
        "processes.col_name",
        "processes.col_status",
        "processes.col_cpu",
        "processes.col_memory",
        "processes.col_disk",
        "processes.col_network",
        "processes.col_gpu",
        "processes.col_publisher",
        "processes.col_pid",
        "processes.col_type",
        "processes.col_process_name",
        "processes.col_command_line",
        "processes.cat_apps",
        "processes.cat_background",
        "processes.cat_windows",
        "processes.cat_macos",
        "processes.cat_system",
        "processes.status_elevated",
        "processes.menu_expand",
        "processes.menu_collapse",
        "processes.menu_end_task",
        "processes.menu_efficiency_mode",
        "processes.menu_resource_values",
        "processes.menu_resource_memory",
        "processes.menu_resource_disk",
        "processes.menu_resource_network",
        "processes.menu_mode_values",
        "processes.menu_mode_percent",
        "processes.menu_open_file_location",
        "processes.menu_search_online",
        "processes.menu_properties",
        "processes.menu_create_dump",
        "processes.menu_attach_debugger",
        "processes.confirm_end_title",
        "processes.confirm_end_critical",
        "processes.confirm_end_action",
        "details.title",
        "details.view_flat",
        "details.view_tree",
        "details.run_new_task",
        "details.end_task",
        "details.col_memory",
        "details.col_session_id",
        "details.col_cpu_time",
        "details.col_working_set",
        "details.col_working_set_delta",
        "details.col_commit",
        "details.col_threads",
        "details.col_handles",
        "details.col_uac",
        "details.col_sandbox",
        "details.col_elevated",
        "details.col_gpu_memory",
        "details.col_publisher",
        "details.col_description",
        "details.col_exe_path",
        "details.col_command_line",
        "details.elevated_yes",
        "details.elevated_no",
        "details.status_running",
        "details.status_suspended",
        "details.uac_enabled",
        "details.uac_disabled",
        "details.uac_not_allowed",
        "details.uac_unknown",
        "details.menu_end_task",
        "details.menu_end_process_tree",
        "details.menu_set_priority",
        "details.menu_set_affinity",
        "details.menu_efficiency_mode",
        "details.menu_analyze_wait_chain",
        "details.menu_create_dump",
        "details.menu_uac_virtualization",
        "details.menu_attach_debugger",
        "details.menu_open_file_location",
        "details.menu_properties",
        "details.menu_search_online",
        "details.menu_go_to_services",
        "details.confirm_tree_title",
        "details.confirm_tree_action",
        "details.affinity_title",
        "details.affinity_all_cpus",
        "details.affinity_empty_error",
        "details.affinity_loading",
        "details.affinity_apply",
        "details.wait_chain_title",
        "details.wait_chain_loading",
        "details.wait_chain_deadlock_banner",
        "details.wait_chain_deadlock_badge",
        "details.wait_chain_end_blocking",
        "details.wait_chain_refresh",
        "minidump.title",
        "minidump.output_path_label",
        "minidump.creating",
        "minidump.cancelling",
        "minidump.completed",
        "minidump.cancelled",
        "minidump.open_location",
        "run_task.title",
        "run_task.open_label",
        "run_task.placeholder",
        "run_task.browse",
        "run_task.run_as_admin",
        "run_task.recent_commands",
        "run_task.empty_error",
        "run_task.run",
        "common.ok",
        "common.cancel",
        "common.apply",
        "common.close",
        "common.confirm",
        "common.copy",
        "table.menu_hide_column",
        "table.menu_select_columns",
        "table.menu_reset_columns",
        "table.select_columns_title",
        "settings.title",
        "settings.section_general",
        "settings.section_window",
        "settings.section_appearance",
        "settings.section_privileges",
        "settings.startup_page_label",
        "settings.refresh_speed_label",
        "settings.always_on_top_label",
        "settings.minimize_to_tray_label",
        "settings.close_to_tray_label",
        "settings.global_hotkey_label",
        "settings.global_hotkey_disabled",
        "settings.theme_label",
        "settings.language_label",
        "settings.restart_as_admin",
        "uac.title",
        "uac.message",
        "uac.action",
        "tray.tooltip",
        "tray.show",
        "tray.exit",
        "perf.cpu",
        "perf.memory",
        "perf.ethernet",
        "perf.wifi",
        "perf.network_other",
        "perf.media_nvme",
        "perf.media_ssd",
        "perf.media_hdd",
        "perf.media_unknown",
        "perf.time_axis_start",
        "perf.time_axis_end",
        "perf.now",
        "perf.unavailable",
        "perf.enabled",
        "perf.disabled",
        "perf.send_short",
        "perf.recv_short",
        "perf.cpu_overall",
        "perf.cpu_logical",
        "perf.cpu_show_kernel",
        "perf.cpu_utilization",
        "perf.cpu_kernel_time",
        "perf.cpu_speed",
        "perf.cpu_processes",
        "perf.cpu_threads",
        "perf.cpu_handles",
        "perf.cpu_uptime",
        "perf.cpu_base_speed",
        "perf.cpu_sockets",
        "perf.cpu_cores",
        "perf.cpu_logical_processors",
        "perf.cpu_virtualization",
        "perf.cpu_l1_cache",
        "perf.cpu_l2_cache",
        "perf.cpu_l3_cache",
        "perf.cpu_sampling_cores",
        "perf.mem_usage_chart",
        "perf.mem_composition",
        "perf.mem_in_use",
        "perf.mem_modified",
        "perf.mem_standby",
        "perf.mem_free",
        "perf.mem_available",
        "perf.mem_committed",
        "perf.mem_cached",
        "perf.mem_paged_pool",
        "perf.mem_nonpaged_pool",
        "perf.mem_speed",
        "perf.mem_slots_used",
        "perf.mem_form_factor",
        "perf.mem_hardware_reserved",
        "perf.disk_active_chart",
        "perf.disk_transfer_chart",
        "perf.disk_read",
        "perf.disk_write",
        "perf.disk_active_time",
        "perf.disk_avg_response",
        "perf.disk_read_speed",
        "perf.disk_write_speed",
        "perf.disk_capacity",
        "perf.disk_drive_letters",
        "perf.disk_type",
        "perf.disk_circuit_open",
        "perf.net_throughput_chart",
        "perf.net_send",
        "perf.net_receive",
        "perf.net_adapter_name",
        "perf.net_connection_type",
        "perf.net_link_speed",
        "perf.net_ipv4",
        "perf.net_ipv6",
        "perf.net_dns_suffix",
        "perf.gpu_engine_count",
        "perf.gpu_engine_count_one",
        "perf.gpu_engine_count_many",
        "perf.gpu_utilization",
        "perf.gpu_dedicated_memory",
        "perf.gpu_shared_memory",
        "perf.gpu_temperature",
        "perf.gpu_driver_version",
        "perf.gpu_directx_version",
        "perf.gpu_pci_location",
        "perf.gpu_select_engine",
        "services.title",
        "services.start",
        "services.stop",
        "services.restart",
        "services.open_services",
        "services.filtered_pid",
        "services.clear_filter",
        "services.col_name",
        "services.col_pid",
        "services.col_description",
        "services.col_status",
        "services.col_group",
        "services.state_running",
        "services.state_stopped",
        "services.state_start_pending",
        "services.state_stop_pending",
        "services.state_paused",
        "services.state_pause_pending",
        "services.state_continue_pending",
        "services.state_unknown",
        "services.menu_start",
        "services.menu_stop",
        "services.menu_restart",
        "services.menu_open_services",
        "services.menu_go_to_details",
        "services.menu_search_online",
        "services.loading",
        "services.empty",
        "startup.title",
        "startup.enable",
        "startup.disable",
        "startup.col_name",
        "startup.col_publisher",
        "startup.col_status",
        "startup.col_impact",
        "startup.status_enabled",
        "startup.status_disabled",
        "startup.impact_high",
        "startup.impact_medium",
        "startup.impact_low",
        "startup.impact_not_measured",
        "startup.impact_none",
        "startup.menu_enable",
        "startup.menu_disable",
        "startup.menu_open_file_location",
        "startup.menu_properties",
        "startup.menu_search_online",
        "startup.loading",
        "startup.empty",
        "users.title",
        "users.disconnect",
        "users.manage_accounts",
        "users.col_user",
        "users.col_status",
        "users.col_cpu",
        "users.col_memory",
        "users.col_disk",
        "users.col_network",
        "users.state_active",
        "users.state_disconnected",
        "users.state_idle",
        "users.state_unknown",
        "users.menu_expand",
        "users.menu_collapse",
        "users.menu_disconnect",
        "users.menu_manage_accounts",
        "users.menu_end_task",
        "users.menu_go_to_details",
        "users.menu_search_online",
        "users.confirm_disconnect_title",
        "users.confirm_disconnect_action",
        "users.loading",
        "users.empty",
        "app_history.title",
        "app_history.delete_history",
        "app_history.col_name",
        "app_history.col_cpu_time",
        "app_history.col_network",
        "app_history.col_metered_network",
        "app_history.menu_search_online",
        "app_history.confirm_delete_title",
        "app_history.confirm_delete_message",
        "app_history.confirm_delete_action",
        "app_history.loading",
        "app_history.empty",
    ];

    use std::sync::Mutex;

    static LOCALE_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn zh_cn_and_en_us_cover_all_core_keys_and_switch_live() {
        let _guard = LOCALE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for &key in CORE_KEYS {
            apply_locale(LanguagePreference::EnUs);
            let en = tr(key);
            assert!(
                !en.is_empty() && en != key,
                "missing en-US translation for key '{key}'"
            );

            apply_locale(LanguagePreference::ZhCn);
            let zh = tr(key);
            assert!(
                !zh.is_empty() && zh != key,
                "missing zh-CN translation for key '{key}'"
            );
        }

        // Verify live switching visibly changes localized strings.
        apply_locale(LanguagePreference::EnUs);
        assert_eq!(tr("nav.processes"), "Processes");
        assert_eq!(tr("nav.settings"), "Settings");
        assert_eq!(tr("uac.action"), "Restart as Administrator");
        assert_eq!(tr("tray.show"), "Show Loom");
        assert_eq!(startup_last_bios_time("4.2"), "Last BIOS time: 4.2 s");
        assert_eq!(perf_gpu_engine_count_label(1), "1 engine");
        assert_eq!(perf_gpu_engine_count_label(4), "4 engines");
        assert_eq!(
            app_history_since("2026-11-27 10:30"),
            "Resource usage since 2026-11-27 10:30"
        );

        apply_locale(LanguagePreference::ZhCn);
        assert_eq!(tr("nav.processes"), "进程");
        assert_eq!(tr("nav.settings"), "设置");
        assert_eq!(tr("uac.action"), "以管理员身份重启");
        assert_eq!(tr("tray.show"), "显示 Loom");
        assert_eq!(startup_last_bios_time("4.2"), "上次 BIOS 时间: 4.2 秒");
        assert_eq!(perf_gpu_engine_count_label(1), "1 个引擎");
        assert_eq!(perf_gpu_engine_count_label(4), "4 个引擎");
        assert_eq!(
            app_history_since("2026-11-27 10:30"),
            "自 2026-11-27 10:30 起的资源使用情况"
        );
    }

    #[test]
    fn process_category_and_virtualization_labels_adapt_to_platform_profile() {
        let _guard = LOCALE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        apply_locale(LanguagePreference::EnUs);
        assert_eq!(
            process_category_label(ProcessCategory::System, PlatformProfile::WINDOWS),
            "Windows processes"
        );
        assert_eq!(
            process_category_label(ProcessCategory::System, PlatformProfile::MACOS),
            "macOS processes"
        );
        assert_eq!(
            process_category_label(ProcessCategory::System, PlatformProfile::LINUX),
            "System processes"
        );
        assert_eq!(
            process_category_label(ProcessCategory::System, PlatformProfile::UNKNOWN),
            "System processes"
        );
        assert_eq!(
            virtualization_column_label(PlatformProfile::WINDOWS),
            "UAC virtualization"
        );
        assert_eq!(
            virtualization_column_label(PlatformProfile::LINUX),
            "Sandbox"
        );
        assert_eq!(
            virtualization_column_label(PlatformProfile::MACOS),
            "Sandbox"
        );

        apply_locale(LanguagePreference::ZhCn);
        assert_eq!(
            process_category_label(ProcessCategory::System, PlatformProfile::WINDOWS),
            "Windows 进程"
        );
        assert_eq!(
            process_category_label(ProcessCategory::System, PlatformProfile::MACOS),
            "macOS 进程"
        );
        assert_eq!(
            process_category_label(ProcessCategory::System, PlatformProfile::LINUX),
            "系统进程"
        );
        assert_eq!(
            process_category_label(ProcessCategory::System, PlatformProfile::UNKNOWN),
            "系统进程"
        );
        assert_eq!(
            virtualization_column_label(PlatformProfile::WINDOWS),
            "UAC 虚拟化"
        );
        assert_eq!(virtualization_column_label(PlatformProfile::LINUX), "沙箱");
        assert_eq!(virtualization_column_label(PlatformProfile::MACOS), "沙箱");
    }
}
