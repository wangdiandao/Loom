//! Complete inline Settings vertical feature slice (zero modals required).
//!
//! All controls apply live to the running engine, native window, theme, and locale
//! and persist atomically to `settings.toml` via [`UiCommands`].

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::{Select, SelectEvent, SelectItem, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, IndexPath, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use loom_types::{CapabilityFlags, LanguagePreference, RefreshSpeed, StartupPage, ThemePreference};

use crate::bridge::{UiCommands, UiStore, UiStoreEvent};
use crate::i18n;
use crate::shell::Route;

const STARTUP_PAGES: [StartupPage; 7] = [
    StartupPage::Processes,
    StartupPage::Performance,
    StartupPage::AppHistory,
    StartupPage::Startup,
    StartupPage::Users,
    StartupPage::Details,
    StartupPage::Services,
];

const REFRESH_SPEEDS: [RefreshSpeed; 4] = [
    RefreshSpeed::High500Ms,
    RefreshSpeed::Normal1S,
    RefreshSpeed::Low2S,
    RefreshSpeed::Paused,
];

const THEME_PREFS: [ThemePreference; 3] = [
    ThemePreference::System,
    ThemePreference::Light,
    ThemePreference::Dark,
];

const LANGUAGE_PREFS: [LanguagePreference; 2] =
    [LanguagePreference::ZhCn, LanguagePreference::EnUs];

/// Strongly-typed option entry for [`SelectState`].
#[derive(Debug, Clone)]
struct SelectOption<V: Clone + PartialEq + 'static> {
    value: V,
    label: SharedString,
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

fn startup_page_options() -> Vec<SelectOption<StartupPage>> {
    STARTUP_PAGES
        .into_iter()
        .map(|value| SelectOption {
            value,
            label: i18n::startup_page_label(value).into(),
        })
        .collect()
}

fn refresh_speed_options() -> Vec<SelectOption<RefreshSpeed>> {
    REFRESH_SPEEDS
        .into_iter()
        .map(|value| SelectOption {
            value,
            label: i18n::refresh_speed_label(value).into(),
        })
        .collect()
}

fn theme_options() -> Vec<SelectOption<ThemePreference>> {
    THEME_PREFS
        .into_iter()
        .map(|value| SelectOption {
            value,
            label: i18n::theme_preference_label(value).into(),
        })
        .collect()
}

fn language_options() -> Vec<SelectOption<LanguagePreference>> {
    LANGUAGE_PREFS
        .into_iter()
        .map(|value| SelectOption {
            value,
            label: i18n::language_preference_label(value).into(),
        })
        .collect()
}

fn index_of<T: PartialEq>(slice: &[T], target: &T) -> Option<IndexPath> {
    slice
        .iter()
        .position(|item| item == target)
        .map(|row| IndexPath::default().row(row))
}

/// Complete inline Settings page view.
pub(crate) struct SettingsView {
    store: Entity<UiStore>,
    last_locale: LanguagePreference,
    startup_page_select: Entity<SelectState<Vec<SelectOption<StartupPage>>>>,
    refresh_speed_select: Entity<SelectState<Vec<SelectOption<RefreshSpeed>>>>,
    theme_select: Entity<SelectState<Vec<SelectOption<ThemePreference>>>>,
    language_select: Entity<SelectState<Vec<SelectOption<LanguagePreference>>>>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    /// Creates a new [`SettingsView`] initialized from current [`UiStore`] settings.
    pub(crate) fn new(store: Entity<UiStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = store.read(cx).settings().clone();

        let startup_page_select = cx.new(|cx| {
            SelectState::new(
                startup_page_options(),
                index_of(&STARTUP_PAGES, &settings.startup_page),
                window,
                cx,
            )
        });
        let refresh_speed_select = cx.new(|cx| {
            SelectState::new(
                refresh_speed_options(),
                index_of(&REFRESH_SPEEDS, &settings.refresh_speed),
                window,
                cx,
            )
        });
        let theme_select = cx.new(|cx| {
            SelectState::new(
                theme_options(),
                index_of(&THEME_PREFS, &settings.theme),
                window,
                cx,
            )
        });
        let language_select = cx.new(|cx| {
            SelectState::new(
                language_options(),
                index_of(&LANGUAGE_PREFS, &settings.language),
                window,
                cx,
            )
        });

        let mut subscriptions = Vec::with_capacity(5);

        subscriptions.push(cx.subscribe_in(
            &startup_page_select,
            window,
            |this, _state, event: &SelectEvent<Vec<SelectOption<StartupPage>>>, _window, cx| {
                let SelectEvent::Confirm(Some(page)) = event else {
                    return;
                };
                UiCommands::set_startup_page(&this.store, *page, cx);
            },
        ));

        subscriptions.push(cx.subscribe_in(
            &refresh_speed_select,
            window,
            |this, _state, event: &SelectEvent<Vec<SelectOption<RefreshSpeed>>>, _window, cx| {
                let SelectEvent::Confirm(Some(speed)) = event else {
                    return;
                };
                UiCommands::set_refresh_speed(&this.store, *speed, cx);
            },
        ));

        subscriptions.push(cx.subscribe_in(
            &theme_select,
            window,
            |this, _state, event: &SelectEvent<Vec<SelectOption<ThemePreference>>>, window, cx| {
                let SelectEvent::Confirm(Some(theme)) = event else {
                    return;
                };
                UiCommands::set_theme(&this.store, *theme, Some(window), cx);
            },
        ));

        subscriptions.push(cx.subscribe_in(
            &language_select,
            window,
            |this,
             _state,
             event: &SelectEvent<Vec<SelectOption<LanguagePreference>>>,
             _window,
             cx| {
                let SelectEvent::Confirm(Some(lang)) = event else {
                    return;
                };
                UiCommands::set_language(&this.store, *lang, cx);
            },
        ));

        subscriptions.push(
            cx.subscribe_in(&store, window, |this, store, event, window, cx| {
                if !matches!(event, UiStoreEvent::SettingsChanged) {
                    return;
                }
                let next_locale = store.read(cx).settings().language;
                if this.last_locale != next_locale {
                    this.last_locale = next_locale;
                    this.refresh_select_labels(window, cx);
                }
                cx.notify();
            }),
        );

        Self {
            store,
            last_locale: settings.language,
            startup_page_select,
            refresh_speed_select,
            theme_select,
            language_select,
            _subscriptions: subscriptions,
        }
    }

    fn refresh_select_labels(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.startup_page_select.update(cx, |state, cx| {
            state.set_items(startup_page_options(), window, cx);
        });
        self.refresh_speed_select.update(cx, |state, cx| {
            state.set_items(refresh_speed_options(), window, cx);
        });
        self.theme_select.update(cx, |state, cx| {
            state.set_items(theme_options(), window, cx);
        });
        self.language_select.update(cx, |state, cx| {
            state.set_items(language_options(), window, cx);
        });
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store_ref = self.store.read(cx);
        let settings = store_ref.settings().clone();
        let snapshot = store_ref.snapshot();
        let is_elevated = snapshot.is_elevated;
        let capabilities = snapshot.capabilities;
        let supports_always_on_top = capabilities.contains(CapabilityFlags::ALWAYS_ON_TOP);
        let supports_elevation_relaunch =
            capabilities.contains(CapabilityFlags::ELEVATION_RELAUNCH);
        let config_path = store_ref.config_path().display().to_string();

        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let border = theme.border;
        let card_bg = theme.secondary.opacity(0.28);
        let badge_bg = if is_elevated {
            theme.success.opacity(0.15)
        } else {
            theme.muted.opacity(0.7)
        };
        let badge_fg = if is_elevated {
            theme.success
        } else {
            theme.foreground
        };
        let radius = theme.radius;
        let radius_lg = theme.radius_lg;
        let mono_font = theme.mono_font_family.clone();

        let store_top = self.store.clone();
        let store_min = self.store.clone();
        let store_close = self.store.clone();
        let store_elevate = self.store.clone();

        let startup_label = i18n::tr("settings.startup_page_label");
        let speed_label = i18n::tr("settings.refresh_speed_label");
        let always_on_top_label = i18n::tr("settings.always_on_top_label");
        let min_tray_label = i18n::tr("settings.minimize_to_tray_label");
        let close_tray_label = i18n::tr("settings.close_to_tray_label");
        let theme_label = i18n::tr("settings.theme_label");
        let language_label = i18n::tr("settings.language_label");

        v_flex()
            .id("settings-scroll-container")
            .size_full()
            .overflow_y_scrollbar()
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(780.))
                    .gap_5()
                    .p_6()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_xl()
                                    .font_semibold()
                                    .text_color(fg)
                                    .child(i18n::tr("settings.title")),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(muted_fg)
                                    .child(i18n::tr("settings.subtitle")),
                            ),
                    )
                    // 1. General
                    .child(
                        settings_section(
                            i18n::tr("settings.section_general"),
                            IconName::Settings,
                            card_bg,
                            border,
                            fg,
                            radius_lg,
                        )
                        .child(setting_row(
                            startup_label.clone(),
                            i18n::tr("settings.startup_page_desc"),
                            fg,
                            muted_fg,
                            border,
                            false,
                            div().w(px(220.)).child(
                                Select::new(&self.startup_page_select)
                                    .small()
                                    .accessibility_label(startup_label),
                            ),
                        ))
                        .child(setting_row(
                            speed_label.clone(),
                            i18n::tr("settings.refresh_speed_desc"),
                            fg,
                            muted_fg,
                            border,
                            true,
                            div().w(px(220.)).child(
                                Select::new(&self.refresh_speed_select)
                                    .small()
                                    .accessibility_label(speed_label),
                            ),
                        )),
                    )
                    // 2. Window Management
                    .child(
                        settings_section(
                            i18n::tr("settings.section_window"),
                            IconName::LayoutDashboard,
                            card_bg,
                            border,
                            fg,
                            radius_lg,
                        )
                        .when(supports_always_on_top, |section| {
                            section.child(setting_row(
                                always_on_top_label.clone(),
                                i18n::tr("settings.always_on_top_desc"),
                                fg,
                                muted_fg,
                                border,
                                false,
                                Switch::new("setting-always-on-top")
                                    .checked(settings.always_on_top)
                                    .accessibility_label(always_on_top_label.clone())
                                    .on_change(move |checked, _, cx| {
                                        UiCommands::set_always_on_top(&store_top, *checked, cx);
                                    }),
                            ))
                        })
                        .child(setting_row(
                            min_tray_label.clone(),
                            i18n::tr("settings.minimize_to_tray_desc"),
                            fg,
                            muted_fg,
                            border,
                            supports_always_on_top,
                            Switch::new("setting-minimize-to-tray")
                                .checked(settings.minimize_to_tray)
                                .accessibility_label(min_tray_label)
                                .on_change(move |checked, _, cx| {
                                    UiCommands::set_minimize_to_tray(&store_min, *checked, cx);
                                }),
                        ))
                        .child(setting_row(
                            close_tray_label.clone(),
                            i18n::tr("settings.close_to_tray_desc"),
                            fg,
                            muted_fg,
                            border,
                            true,
                            Switch::new("setting-close-to-tray")
                                .checked(settings.close_to_tray)
                                .accessibility_label(close_tray_label)
                                .on_change(move |checked, _, cx| {
                                    UiCommands::set_close_to_tray(&store_close, *checked, cx);
                                }),
                        )),
                    )
                    // 3. Appearance & Language
                    .child(
                        settings_section(
                            i18n::tr("settings.section_appearance"),
                            IconName::Palette,
                            card_bg,
                            border,
                            fg,
                            radius_lg,
                        )
                        .child(setting_row(
                            theme_label.clone(),
                            i18n::tr("settings.theme_desc"),
                            fg,
                            muted_fg,
                            border,
                            false,
                            div().w(px(220.)).child(
                                Select::new(&self.theme_select)
                                    .small()
                                    .accessibility_label(theme_label),
                            ),
                        ))
                        .child(setting_row(
                            language_label.clone(),
                            i18n::tr("settings.language_desc"),
                            fg,
                            muted_fg,
                            border,
                            true,
                            div().w(px(220.)).child(
                                Select::new(&self.language_select)
                                    .small()
                                    .accessibility_label(language_label),
                            ),
                        )),
                    )
                    // 4. Privileges & Storage
                    .child(
                        settings_section(
                            i18n::tr("settings.section_privileges"),
                            IconName::Info,
                            card_bg,
                            border,
                            fg,
                            radius_lg,
                        )
                        .child(setting_row(
                            i18n::tr("settings.privilege_label"),
                            i18n::tr("settings.privilege_desc"),
                            fg,
                            muted_fg,
                            border,
                            false,
                            h_flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .px_2p5()
                                        .py_1()
                                        .rounded(radius)
                                        .bg(badge_bg)
                                        .text_xs()
                                        .font_medium()
                                        .text_color(badge_fg)
                                        .child(if is_elevated {
                                            i18n::tr("settings.privilege_admin")
                                        } else {
                                            i18n::tr("settings.privilege_standard")
                                        }),
                                )
                                .when(!is_elevated && supports_elevation_relaunch, |this| {
                                    this.child(
                                        Button::new("settings-restart-as-admin")
                                            .small()
                                            .primary()
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
                        .child(setting_row(
                            i18n::tr("settings.config_path_label"),
                            i18n::tr("settings.config_path_desc"),
                            fg,
                            muted_fg,
                            border,
                            true,
                            div()
                                .max_w(px(340.))
                                .px_2p5()
                                .py_1()
                                .rounded(radius)
                                .border_1()
                                .border_color(border)
                                .text_xs()
                                .font_family(mono_font)
                                .text_color(muted_fg)
                                .truncate()
                                .child(config_path),
                        )),
                    ),
            )
    }
}

fn settings_section(
    title: String,
    icon: IconName,
    bg: gpui_kit::Hsla,
    border: gpui_kit::Hsla,
    fg: gpui_kit::Hsla,
    radius: gpui_kit::Pixels,
) -> gpui_kit::Div {
    v_flex()
        .w_full()
        .rounded(radius)
        .border_1()
        .border_color(border)
        .bg(bg)
        .child(
            h_flex()
                .items_center()
                .gap_2()
                .px_4()
                .py_3()
                .border_b_1()
                .border_color(border)
                .child(Icon::new(icon).small().text_color(fg))
                .child(div().text_sm().font_semibold().text_color(fg).child(title)),
        )
}

fn setting_row(
    title: String,
    description: String,
    fg: gpui_kit::Hsla,
    muted_fg: gpui_kit::Hsla,
    border: gpui_kit::Hsla,
    top_divider: bool,
    control: impl IntoElement,
) -> impl IntoElement {
    h_flex()
        .w_full()
        .items_center()
        .justify_between()
        .gap_4()
        .px_4()
        .py_3p5()
        .when(top_divider, |this| this.border_t_1().border_color(border))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().text_sm().font_medium().text_color(fg).child(title))
                .child(div().text_xs().text_color(muted_fg).child(description)),
        )
        .child(div().flex_shrink_0().child(control))
}
