//! Dropdown option catalogs, custom hex validation, and status presentation models
//! for the Settings feature slice.

use gpui_kit::SharedString;
use gpui_kit::component::IndexPath;
use gpui_kit::component::select::SelectItem;
use loom_types::{
    AdminStartupStatus, LanguagePreference, RefreshSpeed, StartupPage, TaskManagerReplacementState,
    TaskManagerReplacementStatus, ThemePreference,
};

use crate::i18n;

pub(super) const STARTUP_PAGES: [StartupPage; 7] = [
    StartupPage::Processes,
    StartupPage::Performance,
    StartupPage::AppHistory,
    StartupPage::Startup,
    StartupPage::Users,
    StartupPage::Details,
    StartupPage::Services,
];

pub(super) const REFRESH_SPEEDS: [RefreshSpeed; 4] = [
    RefreshSpeed::High500Ms,
    RefreshSpeed::Normal1S,
    RefreshSpeed::Low2S,
    RefreshSpeed::Paused,
];

pub(super) const THEME_PREFS: [ThemePreference; 3] = [
    ThemePreference::System,
    ThemePreference::Light,
    ThemePreference::Dark,
];

pub(super) const LANGUAGE_PREFS: [LanguagePreference; 2] =
    [LanguagePreference::ZhCn, LanguagePreference::EnUs];

pub(super) const GLOBAL_HOTKEY_PRESETS: [Option<&str>; 5] = [
    None,
    Some("Ctrl+Shift+L"),
    Some("Ctrl+Alt+L"),
    Some("Ctrl+Shift+M"),
    Some("Alt+Shift+T"),
];

/// Strongly-typed option entry for `gpui-kit` `SelectState`.
#[derive(Debug, Clone)]
pub(super) struct SelectOption<V: Clone + PartialEq + 'static> {
    pub(super) value: V,
    pub(super) label: SharedString,
}

impl<V: Clone + PartialEq + 'static> SelectItem for SelectOption<V> {
    type Value = V;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }
}

pub(super) fn startup_page_options() -> Vec<SelectOption<StartupPage>> {
    STARTUP_PAGES
        .into_iter()
        .map(|value| SelectOption {
            value,
            label: i18n::startup_page_label(value).into(),
        })
        .collect()
}

pub(super) fn refresh_speed_options() -> Vec<SelectOption<RefreshSpeed>> {
    REFRESH_SPEEDS
        .into_iter()
        .map(|value| SelectOption {
            value,
            label: i18n::refresh_speed_label(value).into(),
        })
        .collect()
}

pub(super) fn theme_options() -> Vec<SelectOption<ThemePreference>> {
    THEME_PREFS
        .into_iter()
        .map(|value| SelectOption {
            value,
            label: i18n::theme_preference_label(value).into(),
        })
        .collect()
}

pub(super) fn language_options() -> Vec<SelectOption<LanguagePreference>> {
    LANGUAGE_PREFS
        .into_iter()
        .map(|value| SelectOption {
            value,
            label: i18n::language_preference_label(value).into(),
        })
        .collect()
}

pub(super) fn global_hotkey_options(current: Option<&str>) -> Vec<SelectOption<Option<String>>> {
    let mut options: Vec<SelectOption<Option<String>>> = GLOBAL_HOTKEY_PRESETS
        .into_iter()
        .map(|preset| match preset {
            None => SelectOption {
                value: None,
                label: i18n::tr("settings.global_hotkey_disabled").into(),
            },
            Some(spec) => SelectOption {
                value: Some(spec.to_owned()),
                label: SharedString::new_static(spec),
            },
        })
        .collect();

    if let Some(custom) = current.map(str::trim).filter(|s| !s.is_empty())
        && !GLOBAL_HOTKEY_PRESETS
            .iter()
            .flatten()
            .any(|preset| preset.eq_ignore_ascii_case(custom))
    {
        options.push(SelectOption {
            value: Some(custom.to_owned()),
            label: custom.to_owned().into(),
        });
    }

    options
}

pub(super) fn hotkey_index_of(
    options: &[SelectOption<Option<String>>],
    target: Option<&str>,
) -> Option<IndexPath> {
    let normalized = target.map(str::trim).filter(|s| !s.is_empty());
    options
        .iter()
        .position(|opt| match (opt.value.as_deref(), normalized) {
            (None, None) => true,
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        })
        .map(|row| IndexPath::default().row(row))
}

pub(super) fn index_of<T: PartialEq>(slice: &[T], target: &T) -> Option<IndexPath> {
    slice
        .iter()
        .position(|item| item == target)
        .map(|row| IndexPath::default().row(row))
}

/// Validates and parses a custom `#RRGGBB` / `RRGGBB` / `#RGB` / `RGB` accent hex string.
#[cfg(test)]
pub(super) fn parse_custom_accent_hex(raw: &str) -> Option<loom_types::AccentColorPreference> {
    loom_types::AccentColorPreference::parse_hex(raw)
        .map(|(r, g, b)| loom_types::AccentColorPreference::custom(r, g, b))
}

/// Semantic status tone for inline Settings status messages and badges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StatusTone {
    Muted,
    Warning,
    Danger,
}

/// Pure presentation state derived for the persistent Administrator startup setting row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AdminStartupPresentation {
    pub(super) checked: bool,
    pub(super) needs_repair: bool,
    pub(super) switch_disabled: bool,
    pub(super) description: Option<String>,
    pub(super) tone: StatusTone,
    pub(super) badge_label: Option<String>,
}

impl AdminStartupPresentation {
    pub(super) fn from_status(status: Option<&AdminStartupStatus>, busy: bool) -> Self {
        let checked = status.is_some_and(|s| s.enabled);
        let needs_repair = status.is_some_and(|s| s.enabled && !s.ready && !s.unprotected_path);
        let switch_disabled = busy
            || match status {
                None => true,
                Some(s) if s.enabled => false,
                Some(s) => !s.supported || !s.eligible || s.unprotected_path,
            };

        let (description, tone, badge_label) = match status {
            None => (None, StatusTone::Muted, None),
            Some(s) if !s.eligible => (
                Some(i18n::tr("settings.admin_startup_ineligible_desc")),
                StatusTone::Muted,
                None,
            ),
            Some(s) if !s.enabled && s.unprotected_path => (
                Some(i18n::tr("settings.admin_startup_unprotected_desc")),
                StatusTone::Muted,
                None,
            ),
            Some(s) if s.enabled && !s.ready => (
                Some(i18n::tr("settings.admin_startup_repair_desc")),
                StatusTone::Warning,
                Some(i18n::tr("settings.admin_startup_badge_repair")),
            ),
            Some(_) => (None, StatusTone::Muted, None),
        };

        Self {
            checked,
            needs_repair,
            switch_disabled,
            description,
            tone,
            badge_label,
        }
    }
}

/// Pure presentation state derived for the IFEO Windows Task Manager replacement setting row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TaskMgrReplacementPresentation {
    pub(super) checked: bool,
    pub(super) switch_disabled: bool,
    pub(super) show_restore: bool,
    pub(super) description: Option<String>,
    pub(super) tone: StatusTone,
    pub(super) badge_label: Option<String>,
}

impl TaskMgrReplacementPresentation {
    pub(super) fn from_status(status: Option<&TaskManagerReplacementStatus>, busy: bool) -> Self {
        let checked = status.is_some_and(|s| s.state == TaskManagerReplacementState::Enabled);
        let switch_disabled = busy
            || match status {
                None => true,
                Some(s) if s.state == TaskManagerReplacementState::Enabled => !s.can_restore,
                Some(s) => !s.can_enable,
            };
        let show_restore = status.is_some_and(|s| {
            s.can_restore
                && matches!(
                    s.state,
                    TaskManagerReplacementState::Revoked
                        | TaskManagerReplacementState::RecoveryNeeded
                )
        });

        let (description, tone, badge_label) = match status {
            None => (None, StatusTone::Muted, None),
            Some(s) => match s.state {
                TaskManagerReplacementState::Disabled if s.unprotected_path => (
                    Some(i18n::tr("settings.taskmgr_replace_unprotected_desc")),
                    StatusTone::Muted,
                    None,
                ),
                TaskManagerReplacementState::Disabled
                | TaskManagerReplacementState::Enabled
                | TaskManagerReplacementState::Unsupported => (None, StatusTone::Muted, None),
                TaskManagerReplacementState::Conflict => {
                    let detail = s
                        .conflict_detail
                        .as_deref()
                        .filter(|d| !d.trim().is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(|| i18n::tr("settings.taskmgr_replace_conflict_unknown"));
                    let msg = rust_i18n::t!(
                        "settings.taskmgr_replace_conflict_desc",
                        detail = detail.as_str()
                    )
                    .into_owned();
                    (
                        Some(msg),
                        StatusTone::Danger,
                        Some(i18n::tr("settings.taskmgr_replace_badge_conflict")),
                    )
                }
                TaskManagerReplacementState::Revoked => (
                    Some(i18n::tr("settings.taskmgr_replace_revoked_desc")),
                    StatusTone::Warning,
                    Some(i18n::tr("settings.taskmgr_replace_badge_revoked")),
                ),
                TaskManagerReplacementState::RecoveryNeeded => (
                    Some(i18n::tr("settings.taskmgr_replace_recovery_desc")),
                    StatusTone::Warning,
                    Some(i18n::tr("settings.taskmgr_replace_badge_recovery")),
                ),
            },
        };

        Self {
            checked,
            switch_disabled,
            show_restore,
            description,
            tone,
            badge_label,
        }
    }
}
