//! Localization helpers wrapping `rust-i18n` for live `zh-CN` / `en-US` switching.

use loom_types::{
    LanguagePreference, PlatformKind, PlatformProfile, ProcessArch, ProcessCategory,
    ProcessPriority, RefreshSpeed, StartupPage, ThemePreference, UacVirtualization,
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

/// Returns the localized milestone badge string for a scheduled version (e.g., `"0.0.4"`).
#[inline]
pub(crate) fn tr_milestone_badge(version: &str) -> String {
    rust_i18n::t!("milestone.badge", version = version).into_owned()
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
        "run_task.title",
        "run_task.description",
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
        "table.select_columns_subtitle",
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
        "settings.theme_label",
        "settings.language_label",
        "settings.restart_as_admin",
        "uac.title",
        "uac.message",
        "uac.action",
        "tray.tooltip",
        "tray.show",
        "tray.exit",
        "milestone.notice",
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
        assert_eq!(tr_milestone_badge("0.0.4"), "Scheduled for v0.0.4");

        apply_locale(LanguagePreference::ZhCn);
        assert_eq!(tr("nav.processes"), "进程");
        assert_eq!(tr("nav.settings"), "设置");
        assert_eq!(tr("uac.action"), "以管理员身份重启");
        assert_eq!(tr("tray.show"), "显示 Loom");
        assert_eq!(tr_milestone_badge("0.0.4"), "计划于 v0.0.4 交付");
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
