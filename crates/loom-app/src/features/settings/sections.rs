//! Modular section renderers and shared card/row layout primitives for [`super::SettingsView`].

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::color_picker::ColorSelect;
use gpui_kit::component::select::Select;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};
use gpui_kit::{
    Context, Div, Hsla, IntoElement, ParentElement as _, Pixels, SharedString, Styled as _, div,
    prelude::FluentBuilder as _, px,
};
use loom_config::AppSettings;
use loom_types::{AccentColorPreference, CapabilityFlags};
use loom_ui_kit::{ACCENT_COLOR_PRESETS, rgb_to_hsla};

use crate::bridge::{UiCommands, system_accent_rgb};
use crate::i18n;
use crate::shell::Route;

use super::SettingsView;
use super::options::{AdminStartupPresentation, StatusTone, TaskMgrReplacementPresentation};

/// Theme tokens resolved once per `SettingsView::render` pass for consistent visual hierarchy.
pub(super) struct SettingsThemePalette {
    pub(super) fg: Hsla,
    pub(super) muted_fg: Hsla,
    pub(super) warn_fg: Hsla,
    pub(super) warn_bg: Hsla,
    pub(super) danger_fg: Hsla,
    pub(super) danger_bg: Hsla,
    pub(super) success_fg: Hsla,
    pub(super) success_bg: Hsla,
    pub(super) neutral_badge_bg: Hsla,
    pub(super) border: Hsla,
    pub(super) card_bg: Hsla,
    pub(super) radius: Pixels,
    pub(super) radius_lg: Pixels,
    pub(super) mono_font: SharedString,
}

impl SettingsThemePalette {
    pub(super) fn from_cx(cx: &Context<SettingsView>) -> Self {
        let theme = cx.theme();
        Self {
            fg: theme.foreground,
            muted_fg: theme.muted_foreground,
            warn_fg: theme.warning,
            warn_bg: theme.warning.opacity(0.16),
            danger_fg: theme.danger,
            danger_bg: theme.danger.opacity(0.16),
            success_fg: theme.success,
            success_bg: theme.success.opacity(0.15),
            neutral_badge_bg: theme.muted.opacity(0.7),
            border: theme.border,
            card_bg: theme.secondary.opacity(0.28),
            radius: theme.radius,
            radius_lg: theme.radius_lg,
            mono_font: theme.mono_font_family.clone(),
        }
    }

    fn tone_colors(&self, tone: StatusTone) -> (Hsla, Hsla) {
        match tone {
            StatusTone::Muted => (self.neutral_badge_bg, self.muted_fg),
            StatusTone::Warning => (self.warn_bg, self.warn_fg),
            StatusTone::Danger => (self.danger_bg, self.danger_fg),
        }
    }
}

/// 1. General section: default start page & real-time update speed.
pub(super) fn render_general_section(view: &SettingsView, palette: &SettingsThemePalette) -> Div {
    let startup_label = i18n::tr("settings.startup_page_label");
    let speed_label = i18n::tr("settings.refresh_speed_label");

    settings_section(
        i18n::tr("settings.section_general"),
        IconName::Settings,
        palette,
    )
    .child(setting_row(
        startup_label.clone(),
        palette,
        false,
        div().w(px(220.)).child(
            Select::new(&view.startup_page_select)
                .small()
                .accessibility_label(startup_label),
        ),
    ))
    .child(setting_row(
        speed_label.clone(),
        palette,
        true,
        div().w(px(220.)).child(
            Select::new(&view.refresh_speed_select)
                .small()
                .accessibility_label(speed_label),
        ),
    ))
}

/// 2. Processes & Diagnostics section: WebView2 host grouping, leak warnings, system process hiding.
pub(super) fn render_diagnostics_section(
    settings: &AppSettings,
    palette: &SettingsThemePalette,
    cx: &mut Context<SettingsView>,
) -> Div {
    let wv2_label = i18n::tr("settings.settings_diag_webview2_grouping");
    let leak_label = i18n::tr("settings.settings_diag_leak_warnings");
    let hide_sys_label = i18n::tr("settings.settings_diag_hide_system");

    settings_section(
        i18n::tr("settings.settings_diag_section"),
        IconName::Settings2,
        palette,
    )
    .child(setting_row(
        wv2_label.clone(),
        palette,
        false,
        Switch::new("setting-webview2-host-grouping")
            .checked(settings.webview2_host_grouping)
            .accessibility_label(wv2_label)
            .on_change(cx.listener(|view, checked, _, cx| {
                view.set_webview2_host_grouping(*checked, cx);
            })),
    ))
    .child(setting_row(
        leak_label.clone(),
        palette,
        true,
        Switch::new("setting-show-leak-warnings")
            .checked(settings.show_leak_warnings)
            .accessibility_label(leak_label)
            .on_change(cx.listener(|view, checked, _, cx| {
                view.set_show_leak_warnings(*checked, cx);
            })),
    ))
    .child(setting_row(
        hide_sys_label.clone(),
        palette,
        true,
        Switch::new("setting-hide-system-processes")
            .checked(settings.hide_windows_system_processes)
            .accessibility_label(hide_sys_label)
            .on_change(cx.listener(|view, checked, _, cx| {
                view.set_hide_system_processes(*checked, cx);
            })),
    ))
}

/// 3. Window Management section: tray behavior & global wakeup hotkey.
pub(super) fn render_window_section(
    view: &SettingsView,
    settings: &AppSettings,
    capabilities: CapabilityFlags,
    palette: &SettingsThemePalette,
) -> Div {
    let supports_global_hotkey = capabilities.contains(CapabilityFlags::GLOBAL_HOTKEY);
    let min_tray_label = i18n::tr("settings.minimize_to_tray_label");
    let close_tray_label = i18n::tr("settings.close_to_tray_label");
    let global_hotkey_label = i18n::tr("settings.global_hotkey_label");
    let store_min = view.store.clone();
    let store_close = view.store.clone();

    settings_section(
        i18n::tr("settings.section_window"),
        IconName::LayoutDashboard,
        palette,
    )
    .child(setting_row(
        min_tray_label.clone(),
        palette,
        false,
        Switch::new("setting-minimize-to-tray")
            .checked(settings.minimize_to_tray)
            .accessibility_label(min_tray_label)
            .on_change(move |checked, _, cx| {
                UiCommands::set_minimize_to_tray(&store_min, *checked, cx);
            }),
    ))
    .child(setting_row(
        close_tray_label.clone(),
        palette,
        true,
        Switch::new("setting-close-to-tray")
            .checked(settings.close_to_tray)
            .accessibility_label(close_tray_label)
            .on_change(move |checked, _, cx| {
                UiCommands::set_close_to_tray(&store_close, *checked, cx);
            }),
    ))
    .when(supports_global_hotkey, |section| {
        section.child(setting_row(
            global_hotkey_label.clone(),
            palette,
            true,
            div().w(px(220.)).child(
                Select::new(&view.global_hotkey_select)
                    .small()
                    .accessibility_label(global_hotkey_label),
            ),
        ))
    })
}

/// 4. Appearance & Language section: theme, system/custom accent color swatches + hex picker, language.
pub(super) fn render_appearance_section(
    view: &SettingsView,
    settings: &AppSettings,
    palette: &SettingsThemePalette,
    cx: &mut Context<SettingsView>,
) -> Div {
    let theme_label = i18n::tr("settings.theme_label");
    let accent_follow_label = i18n::tr("settings.accent_follow_system_label");
    let accent_custom_label = i18n::tr("settings.accent_custom_label");
    let accent_picker_placeholder = i18n::tr("settings.accent_picker_placeholder");
    let language_label = i18n::tr("settings.language_label");

    let sys_rgb = system_accent_rgb();
    let sys_hex = AccentColorPreference::format_hex(sys_rgb.0, sys_rgb.1, sys_rgb.2);
    let follow_system_accent = settings.accent_color.is_system();
    let active_custom_rgb = settings.accent_color.rgb();

    let featured_colors: Vec<Hsla> = ACCENT_COLOR_PRESETS
        .iter()
        .map(|(_, rgb)| rgb_to_hsla(*rgb))
        .collect();

    let preset_swatches = render_accent_preset_swatches(active_custom_rgb, palette, cx);

    settings_section(
        i18n::tr("settings.section_appearance"),
        IconName::Palette,
        palette,
    )
    .child(setting_row(
        theme_label.clone(),
        palette,
        false,
        div().w(px(220.)).child(
            Select::new(&view.theme_select)
                .small()
                .accessibility_label(theme_label),
        ),
    ))
    .child(setting_row(
        accent_follow_label.clone(),
        palette,
        true,
        h_flex()
            .items_center()
            .gap_3()
            .child(
                h_flex()
                    .items_center()
                    .gap_1p5()
                    .px_2()
                    .py_0p5()
                    .rounded(palette.radius)
                    .border_1()
                    .border_color(palette.border)
                    .child(
                        div()
                            .size(px(12.))
                            .rounded_full()
                            .bg(rgb_to_hsla(sys_rgb))
                            .border_1()
                            .border_color(palette.border),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_family(palette.mono_font.clone())
                            .text_color(palette.muted_fg)
                            .child(sys_hex),
                    ),
            )
            .child(
                Switch::new("setting-accent-follow-system")
                    .checked(follow_system_accent)
                    .accessibility_label(accent_follow_label)
                    .on_change(cx.listener(|view, checked, window, cx| {
                        view.set_follow_system_accent(*checked, window, cx);
                    })),
            ),
    ))
    .child(setting_row(
        accent_custom_label.clone(),
        palette,
        true,
        h_flex()
            .flex_wrap()
            .items_center()
            .gap_2p5()
            .child(preset_swatches)
            .child(
                div().w(px(136.)).child(
                    ColorSelect::new(&view.accent_color_picker)
                        .small()
                        .featured_colors(featured_colors)
                        .placeholder(accent_picker_placeholder)
                        .accessibility_label(accent_custom_label),
                ),
            ),
    ))
    .child(setting_row(
        language_label.clone(),
        palette,
        true,
        div().w(px(220.)).child(
            Select::new(&view.language_select)
                .small()
                .accessibility_label(language_label),
        ),
    ))
}

fn render_accent_preset_swatches(
    active_custom_rgb: Option<(u8, u8, u8)>,
    palette: &SettingsThemePalette,
    cx: &mut Context<SettingsView>,
) -> Div {
    let fg = palette.fg;
    let border = palette.border;

    h_flex()
        .items_center()
        .gap_1()
        .children(
            ACCENT_COLOR_PRESETS
                .iter()
                .enumerate()
                .map(|(idx, (_slug, rgb))| {
                    let (r, g, b) = *rgb;
                    let hex = AccentColorPreference::format_hex(r, g, b);
                    let is_selected = active_custom_rgb == Some((r, g, b));
                    let swatch_color = rgb_to_hsla((r, g, b));
                    let ring_color = if is_selected {
                        fg
                    } else {
                        gpui_kit::transparent_black()
                    };
                    Button::new(("setting-accent-preset", idx))
                        .ghost()
                        .xsmall()
                        .w(px(24.))
                        .h(px(24.))
                        .p_0()
                        .rounded_full()
                        .accessibility_label(hex.clone())
                        .tooltip(hex)
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.set_accent_color(
                                AccentColorPreference::custom(r, g, b),
                                window,
                                cx,
                            );
                        }))
                        .child(
                            div()
                                .size(px(18.))
                                .rounded_full()
                                .border_2()
                                .border_color(ring_color)
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(
                                    div()
                                        .size(px(12.))
                                        .rounded_full()
                                        .bg(swatch_color)
                                        .border_1()
                                        .border_color(border),
                                ),
                        )
                }),
        )
}

/// Privileges & Storage section: privilege badge, admin startup task, IFEO Task Manager
/// replacement, native fallback launcher, and config file path.
pub(super) fn render_privileges_section(
    view: &SettingsView,
    is_elevated: bool,
    capabilities: CapabilityFlags,
    config_path: String,
    palette: &SettingsThemePalette,
    cx: &mut Context<SettingsView>,
) -> Div {
    let supports_elevation_relaunch = capabilities.contains(CapabilityFlags::ELEVATION_RELAUNCH);
    let supports_admin_startup = capabilities.contains(CapabilityFlags::ADMIN_STARTUP);
    let supports_taskmgr_replacement =
        capabilities.contains(CapabilityFlags::TASK_MANAGER_REPLACEMENT);

    let store_elevate = view.store.clone();
    let store_native_taskmgr = view.store.clone();

    let (priv_badge_bg, priv_badge_fg) = if is_elevated {
        (palette.success_bg, palette.success_fg)
    } else {
        (palette.neutral_badge_bg, palette.fg)
    };

    let admin_pres =
        AdminStartupPresentation::from_status(view.admin_status.as_ref(), view.admin_busy);
    let (admin_badge_bg, admin_desc_color) = palette.tone_colors(admin_pres.tone);
    let admin_startup_label = i18n::tr("settings.admin_startup_label");
    let admin_busy = view.admin_busy;

    let taskmgr_pres = TaskMgrReplacementPresentation::from_status(
        view.taskmgr_status.as_ref(),
        view.taskmgr_busy,
    );
    let (taskmgr_badge_bg, taskmgr_desc_color) = palette.tone_colors(taskmgr_pres.tone);
    let taskmgr_replace_label = i18n::tr("settings.taskmgr_replace_label");
    let taskmgr_open_native_label = i18n::tr("settings.taskmgr_open_native_label");
    let taskmgr_busy = view.taskmgr_busy;

    settings_section(
        i18n::tr("settings.section_privileges"),
        IconName::Info,
        palette,
    )
    .child(setting_row(
        i18n::tr("settings.privilege_label"),
        palette,
        false,
        h_flex()
            .items_center()
            .gap_2()
            .child(status_badge(
                if is_elevated {
                    i18n::tr("settings.privilege_admin")
                } else {
                    i18n::tr("settings.privilege_standard")
                },
                priv_badge_bg,
                priv_badge_fg,
                palette.radius,
            ))
            .when(!is_elevated && supports_elevation_relaunch, |this| {
                this.child(
                    Button::new("settings-restart-as-admin")
                        .small()
                        .outline()
                        .label(i18n::tr("settings.restart_as_admin"))
                        .on_click(move |_, _, cx| {
                            UiCommands::relaunch_elevated(
                                &store_elevate,
                                Route::Settings.as_str(),
                                cx,
                            );
                        }),
                )
            }),
    ))
    .when(supports_admin_startup, |section| {
        section.child(setting_row_with_description(
            admin_startup_label.clone(),
            admin_pres.description,
            admin_desc_color,
            palette,
            true,
            h_flex()
                .items_center()
                .gap_2()
                .when_some(admin_pres.badge_label, |this, label| {
                    this.child(status_badge(
                        label,
                        admin_badge_bg,
                        admin_desc_color,
                        palette.radius,
                    ))
                })
                .when(admin_pres.needs_repair, |this| {
                    this.child(
                        Button::new("settings-repair-admin-startup")
                            .small()
                            .outline()
                            .disabled(admin_busy)
                            .label(i18n::tr("settings.admin_startup_repair_btn"))
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.set_admin_startup(true, window, cx);
                            })),
                    )
                })
                .child(
                    Switch::new("setting-admin-startup")
                        .checked(admin_pres.checked)
                        .disabled(admin_pres.switch_disabled)
                        .accessibility_label(admin_startup_label.clone())
                        .on_change(cx.listener(|view, checked, window, cx| {
                            view.set_admin_startup(*checked, window, cx);
                        })),
                ),
        ))
    })
    .when(supports_taskmgr_replacement, |section| {
        section
            .child(setting_row_with_description(
                taskmgr_replace_label.clone(),
                taskmgr_pres.description,
                taskmgr_desc_color,
                palette,
                true,
                h_flex()
                    .items_center()
                    .gap_2()
                    .when_some(taskmgr_pres.badge_label, |this, label| {
                        this.child(status_badge(
                            label,
                            taskmgr_badge_bg,
                            taskmgr_desc_color,
                            palette.radius,
                        ))
                    })
                    .when(taskmgr_pres.show_restore, |this| {
                        this.child(
                            Button::new("settings-restore-task-manager")
                                .small()
                                .outline()
                                .disabled(taskmgr_busy)
                                .label(i18n::tr("settings.taskmgr_replace_restore_btn"))
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.set_task_manager_replacement(false, window, cx);
                                })),
                        )
                    })
                    .child(
                        Switch::new("setting-replace-task-manager")
                            .checked(taskmgr_pres.checked)
                            .disabled(taskmgr_pres.switch_disabled)
                            .accessibility_label(taskmgr_replace_label.clone())
                            .on_change(cx.listener(|view, checked, window, cx| {
                                view.set_task_manager_replacement(*checked, window, cx);
                            })),
                    ),
            ))
            .child(setting_row(
                taskmgr_open_native_label,
                palette,
                true,
                Button::new("settings-open-native-taskmgr")
                    .small()
                    .outline()
                    .label(i18n::tr("settings.taskmgr_open_native_btn"))
                    .on_click(move |_, _, cx| {
                        UiCommands::open_native_task_manager(&store_native_taskmgr, cx);
                    }),
            ))
    })
    .child(setting_row(
        i18n::tr("settings.config_path_label"),
        palette,
        true,
        div()
            .max_w(px(340.))
            .px_2p5()
            .py_1()
            .rounded(palette.radius)
            .border_1()
            .border_color(palette.border)
            .text_xs()
            .font_family(palette.mono_font.clone())
            .text_color(palette.muted_fg)
            .truncate()
            .child(config_path),
    ))
}

fn status_badge(label: String, bg: Hsla, fg: Hsla, radius: Pixels) -> Div {
    div()
        .px_2()
        .py_0p5()
        .rounded(radius)
        .bg(bg)
        .text_xs()
        .font_medium()
        .text_color(fg)
        .child(label)
}

fn settings_section(title: String, icon: IconName, palette: &SettingsThemePalette) -> Div {
    v_flex()
        .w_full()
        .rounded(palette.radius_lg)
        .border_1()
        .border_color(palette.border)
        .bg(palette.card_bg)
        .child(
            h_flex()
                .items_center()
                .gap_2()
                .px_4()
                .py_3()
                .border_b_1()
                .border_color(palette.border)
                .child(Icon::new(icon).small().text_color(palette.fg))
                .child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .text_color(palette.fg)
                        .child(title),
                ),
        )
}

fn setting_row(
    title: String,
    palette: &SettingsThemePalette,
    top_divider: bool,
    control: impl IntoElement,
) -> impl IntoElement {
    h_flex()
        .w_full()
        .items_center()
        .justify_between()
        .gap_4()
        .px_4()
        .py_3()
        .when(top_divider, |this| {
            this.border_t_1().border_color(palette.border)
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .font_medium()
                .text_color(palette.fg)
                .child(title),
        )
        .child(div().flex_shrink_0().child(control))
}

fn setting_row_with_description(
    title: String,
    description: Option<String>,
    desc_fg: Hsla,
    palette: &SettingsThemePalette,
    top_divider: bool,
    control: impl IntoElement,
) -> impl IntoElement {
    h_flex()
        .w_full()
        .items_center()
        .justify_between()
        .gap_4()
        .px_4()
        .py_3()
        .when(top_divider, |this| {
            this.border_t_1().border_color(palette.border)
        })
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .font_medium()
                        .text_color(palette.fg)
                        .child(title),
                )
                .when_some(description, |this, text| {
                    this.child(div().text_xs().text_color(desc_fg).child(text))
                }),
        )
        .child(div().flex_shrink_0().child(control))
}
