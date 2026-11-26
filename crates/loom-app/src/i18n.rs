//! Localization helpers wrapping `rust-i18n` for live `zh-CN` / `en-US` switching.

use loom_types::{
    LanguagePreference, ProcessArch, ProcessCategory, ProcessPriority, RefreshSpeed, StartupPage,
    ThemePreference,
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

/// Returns the localized label for a [`ProcessCategory`].
pub(crate) fn process_category_label(category: ProcessCategory) -> String {
    match category {
        ProcessCategory::App => tr("processes.cat_apps"),
        ProcessCategory::Background => tr("processes.cat_background"),
        ProcessCategory::Windows => tr("processes.cat_windows"),
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
        "processes.col_name",
        "processes.col_status",
        "processes.col_cpu",
        "processes.col_memory",
        "processes.col_disk",
        "processes.col_network",
        "processes.cat_apps",
        "processes.cat_background",
        "processes.cat_windows",
        "details.title",
        "details.lease_active",
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

    #[test]
    fn zh_cn_and_en_us_cover_all_core_keys_and_switch_live() {
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
}
