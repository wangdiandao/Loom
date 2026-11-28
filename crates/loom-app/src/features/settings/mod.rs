//! Complete inline Settings vertical feature slice (zero modals required).
//!
//! All controls apply live to the running engine, native window, theme, and locale
//! and persist atomically to `settings.toml` via [`UiCommands`] / [`UiStore`].

mod options;
mod sections;
#[cfg(test)]
mod tests;

use gpui_kit::component::color_picker::{ColorPickerEvent, ColorPickerState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::{SelectEvent, SelectState};
use gpui_kit::component::{StyledExt as _, WindowExt as _, v_flex};
use gpui_kit::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, Styled as _, Subscription, Task, Window, div, px,
};
use loom_engine::{CommandOutcome, CommandSuccess, EngineCommand};
use loom_types::{
    AccentColorPreference, AdminStartupStatus, CapabilityFlags, LanguagePreference, RefreshSpeed,
    StartupPage, TaskManagerReplacementState, TaskManagerReplacementStatus, ThemePreference,
};
use loom_ui_kit::{hsla_to_rgb_u8, rgb_to_hsla};

use crate::bootstrap;
use crate::bridge::{UiCommands, UiStore, UiStoreEvent, resolve_accent_rgb, system_accent_rgb};
use crate::i18n;
use crate::shell::Route;

use options::{
    LANGUAGE_PREFS, REFRESH_SPEEDS, STARTUP_PAGES, SelectOption, THEME_PREFS,
    global_hotkey_options, hotkey_index_of, index_of, language_options, refresh_speed_options,
    startup_page_options, theme_options,
};
use sections::{
    SettingsThemePalette, render_appearance_section, render_diagnostics_section,
    render_general_section, render_privileges_section, render_window_section,
};

/// Complete inline Settings page view.
pub(crate) struct SettingsView {
    store: Entity<UiStore>,
    last_locale: LanguagePreference,
    startup_page_select: Entity<SelectState<Vec<SelectOption<StartupPage>>>>,
    refresh_speed_select: Entity<SelectState<Vec<SelectOption<RefreshSpeed>>>>,
    global_hotkey_select: Entity<SelectState<Vec<SelectOption<Option<String>>>>>,
    theme_select: Entity<SelectState<Vec<SelectOption<ThemePreference>>>>,
    accent_color_picker: Entity<ColorPickerState>,
    last_custom_accent: Option<(u8, u8, u8)>,
    language_select: Entity<SelectState<Vec<SelectOption<LanguagePreference>>>>,
    taskmgr_status: Option<TaskManagerReplacementStatus>,
    taskmgr_busy: bool,
    _taskmgr_task: Option<Task<()>>,
    admin_status: Option<AdminStartupStatus>,
    admin_busy: bool,
    _admin_task: Option<Task<()>>,
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
        let hotkey_opts = global_hotkey_options(settings.global_hotkey.as_deref());
        let hotkey_idx = hotkey_index_of(&hotkey_opts, settings.global_hotkey.as_deref());
        let global_hotkey_select =
            cx.new(|cx| SelectState::new(hotkey_opts, hotkey_idx, window, cx));
        let theme_select = cx.new(|cx| {
            SelectState::new(
                theme_options(),
                index_of(&THEME_PREFS, &settings.theme),
                window,
                cx,
            )
        });
        let initial_accent_rgb = resolve_accent_rgb(settings.accent_color);
        let accent_color_picker = cx.new(|cx| {
            ColorPickerState::new(window, cx).default_value(rgb_to_hsla(initial_accent_rgb))
        });
        let language_select = cx.new(|cx| {
            SelectState::new(
                language_options(),
                index_of(&LANGUAGE_PREFS, &settings.language),
                window,
                cx,
            )
        });

        let mut subscriptions = Vec::with_capacity(7);

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
            &global_hotkey_select,
            window,
            |this, state, event: &SelectEvent<Vec<SelectOption<Option<String>>>>, window, cx| {
                let SelectEvent::Confirm(Some(selected_hotkey)) = event else {
                    return;
                };
                if let Err(err) =
                    UiCommands::set_global_hotkey(&this.store, selected_hotkey.clone(), cx)
                {
                    let msg = rust_i18n::t!(
                        "settings.global_hotkey_error",
                        error = err.to_string().as_str()
                    )
                    .into_owned();
                    window.push_notification(Notification::error(msg), cx);
                    let previous = this.store.read(cx).settings().global_hotkey.clone();
                    let opts = global_hotkey_options(previous.as_deref());
                    let prev_idx = hotkey_index_of(&opts, previous.as_deref());
                    state.update(cx, |s, cx| {
                        s.set_selected_index(prev_idx, window, cx);
                    });
                }
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
            &accent_color_picker,
            window,
            |this, _state, event: &ColorPickerEvent, window, cx| {
                let ColorPickerEvent::Change(Some(color)) = event else {
                    return;
                };
                let (r, g, b) = hsla_to_rgb_u8(*color);
                this.last_custom_accent = Some((r, g, b));
                UiCommands::set_accent_color(
                    &this.store,
                    AccentColorPreference::custom(r, g, b),
                    Some(window),
                    cx,
                );
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
                let (next_locale, next_accent) = {
                    let s = store.read(cx).settings();
                    (s.language, s.accent_color)
                };
                if this.last_locale != next_locale {
                    this.last_locale = next_locale;
                    this.refresh_select_labels(window, cx);
                }
                let resolved_rgb = resolve_accent_rgb(next_accent);
                let picker_rgb = this
                    .accent_color_picker
                    .read(cx)
                    .value()
                    .map(hsla_to_rgb_u8);
                if picker_rgb != Some(resolved_rgb) {
                    this.accent_color_picker.update(cx, |state, cx| {
                        state.set_value(rgb_to_hsla(resolved_rgb), window, cx);
                    });
                }
                cx.notify();
            }),
        );

        let mut view = Self {
            store,
            last_locale: settings.language,
            startup_page_select,
            refresh_speed_select,
            global_hotkey_select,
            theme_select,
            accent_color_picker,
            last_custom_accent: settings.accent_color.rgb(),
            language_select,
            taskmgr_status: None,
            taskmgr_busy: false,
            _taskmgr_task: None,
            admin_status: None,
            admin_busy: false,
            _admin_task: None,
            _subscriptions: subscriptions,
        };
        let caps = view.store.read(cx).snapshot().capabilities;
        if caps.contains(CapabilityFlags::TASK_MANAGER_REPLACEMENT) {
            view.refresh_task_manager_status(window, cx);
        }
        if caps.contains(CapabilityFlags::ADMIN_STARTUP) {
            view.refresh_admin_startup_status(window, cx);
        }
        view
    }

    /// Queries the read-only OS Task Manager replacement status once on the slow lane.
    pub(crate) fn refresh_task_manager_status(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Ok(rx) = UiCommands::query_task_manager_replacement_async(&self.store, cx) else {
            return;
        };
        self._taskmgr_task = Some(cx.spawn_in(window, async move |weak, cx| {
            let reply = cx
                .background_executor()
                .spawn(async move { rx.recv().ok() })
                .await;
            let _ =
                weak.update_in(cx, |this, _window, cx| {
                    this.taskmgr_busy = false;
                    if let Some(event) = reply
                        && let CommandOutcome::Ok(CommandSuccess::TaskManagerReplacementStatus(
                            status,
                        )) = event.outcome
                    {
                        this.taskmgr_status = Some(status);
                    }
                    cx.notify();
                });
        }));
    }

    /// Toggles OS-level Task Manager replacement on the slow lane (triggering UAC if needed).
    pub(crate) fn set_task_manager_replacement(
        &mut self,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.taskmgr_busy {
            return;
        }
        let Ok(rx) = UiCommands::set_task_manager_replacement_async(&self.store, enabled, cx)
        else {
            return;
        };
        self.taskmgr_busy = true;
        cx.notify();

        self._taskmgr_task = Some(cx.spawn_in(window, async move |weak, cx| {
            let reply = cx
                .background_executor()
                .spawn(async move { rx.recv().ok() })
                .await;
            let _ = weak.update_in(cx, |this, window, cx| {
                this.taskmgr_busy = false;
                if let Some(event) = reply {
                    match event.outcome {
                        CommandOutcome::Ok(CommandSuccess::TaskManagerReplacementStatus(
                            status,
                        )) => {
                            let msg = if status.state == TaskManagerReplacementState::Enabled {
                                i18n::tr("settings.taskmgr_replace_enabled_toast")
                            } else {
                                i18n::tr("settings.taskmgr_replace_restored_toast")
                            };
                            this.taskmgr_status = Some(status);
                            window.push_notification(Notification::success(msg), cx);
                        }
                        CommandOutcome::Err(_) => {
                            // Refresh authoritative OS status so the toggle stays accurate
                            // (including when the user cancels UAC).
                            this.refresh_task_manager_status(window, cx);
                        }
                        CommandOutcome::Ok(_) => {}
                    }
                } else {
                    this.refresh_task_manager_status(window, cx);
                }
                cx.notify();
            });
        }));
    }

    /// Queries the read-only persistent administrator startup status once on the slow lane.
    pub(crate) fn refresh_admin_startup_status(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Ok(rx) = UiCommands::query_admin_startup_async(&self.store, cx) else {
            return;
        };
        self._admin_task = Some(cx.spawn_in(window, async move |weak, cx| {
            let reply = cx
                .background_executor()
                .spawn(async move { rx.recv().ok() })
                .await;
            let _ = weak.update_in(cx, |this, _window, cx| {
                this.admin_busy = false;
                if let Some(event) = reply
                    && let CommandOutcome::Ok(CommandSuccess::AdminStartupStatus(status)) =
                        event.outcome
                {
                    this.admin_status = Some(status);
                }
                cx.notify();
            });
        }));
    }

    /// Enables or disables persistent administrator startup on the slow lane and seamlessly
    /// transitions privilege level when needed.
    pub(crate) fn set_admin_startup(
        &mut self,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.admin_busy {
            return;
        }
        let Ok(rx) = UiCommands::set_admin_startup_async(&self.store, enabled, cx) else {
            return;
        };
        self.admin_busy = true;
        cx.notify();

        self._admin_task = Some(cx.spawn_in(window, async move |weak, cx| {
            let reply = cx
                .background_executor()
                .spawn(async move { rx.recv().ok() })
                .await;
            let Some(event) = reply else {
                let _ = weak.update_in(cx, |this, window, cx| {
                    this.admin_busy = false;
                    this.refresh_admin_startup_status(window, cx);
                    cx.notify();
                });
                return;
            };

            let status = match event.outcome {
                CommandOutcome::Ok(CommandSuccess::AdminStartupStatus(status)) => status,
                CommandOutcome::Err(_) => {
                    let _ = weak.update_in(cx, |this, window, cx| {
                        this.admin_busy = false;
                        this.refresh_admin_startup_status(window, cx);
                        cx.notify();
                    });
                    return;
                }
                CommandOutcome::Ok(_) => {
                    let _ = weak.update_in(cx, |this, _window, cx| {
                        this.admin_busy = false;
                        cx.notify();
                    });
                    return;
                }
            };

            // Decide whether to perform a seamless privilege switch after updating the task.
            let switch_target = if status.enabled && status.ready && !status.elevated {
                Some(true)
            } else if !status.enabled && status.elevated && status.can_drop_elevation {
                Some(false)
            } else {
                None
            };

            if let Some(target_elevated) = switch_target {
                let prepared = cx
                    .background_executor()
                    .spawn(async move {
                        loom_pal::admin::prepare_privilege_switch(
                            Route::Settings.as_str(),
                            target_elevated,
                        )
                    })
                    .await;
                if let Ok(switch) = prepared
                    && switch.commit().is_ok()
                {
                    let _ = cx.update(|_window, cx| {
                        bootstrap::quit_for_privilege_switch(cx);
                    });
                    return;
                }
            }

            let _ = weak.update_in(cx, |this, window, cx| {
                this.admin_busy = false;
                let msg = if status.enabled {
                    i18n::tr("settings.admin_startup_enabled_toast")
                } else {
                    i18n::tr("settings.admin_startup_disabled_toast")
                };
                this.admin_status = Some(status);
                window.push_notification(Notification::success(msg), cx);
                cx.notify();
            });
        }));
    }

    /// Toggles grouping `msedgewebview2.exe` helpers under their host app,
    /// dispatching [`EngineCommand::SetWebView2HostGrouping`] and persisting to `settings.toml`.
    pub(crate) fn set_webview2_host_grouping(&self, enabled: bool, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            let _ = store.dispatch_command(EngineCommand::SetWebView2HostGrouping(enabled));
            let _ = store.update_settings(|s| s.webview2_host_grouping = enabled, cx);
        });
    }

    /// Toggles advisory resource leak warnings, dispatching
    /// [`EngineCommand::SetShowLeakWarnings`] and persisting to `settings.toml`.
    pub(crate) fn set_show_leak_warnings(&self, enabled: bool, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            let _ = store.dispatch_command(EngineCommand::SetShowLeakWarnings(enabled));
            let _ = store.update_settings(|s| s.show_leak_warnings = enabled, cx);
        });
    }

    /// Toggles hiding core Windows system processes by default, dispatching
    /// [`EngineCommand::SetHideSystemProcesses`] and persisting to `settings.toml`.
    pub(crate) fn set_hide_system_processes(&self, hidden: bool, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            let _ = store.dispatch_command(EngineCommand::SetHideSystemProcesses(hidden));
            let _ = store.update_settings(|s| s.hide_windows_system_processes = hidden, cx);
        });
    }

    /// Updates and live-applies the theme accent color (`System` or custom hex),
    /// keeping the inline color picker synchronized and persisting to `settings.toml`.
    pub(crate) fn set_accent_color(
        &mut self,
        accent: AccentColorPreference,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(rgb) = accent.rgb() {
            self.last_custom_accent = Some(rgb);
        }
        UiCommands::set_accent_color(&self.store, accent, Some(window), cx);
        let resolved = resolve_accent_rgb(accent);
        self.accent_color_picker.update(cx, |state, cx| {
            state.set_value(rgb_to_hsla(resolved), window, cx);
        });
        cx.notify();
    }

    /// Validates and applies a custom hex accent color string (`#RRGGBB` / `#RGB`).
    ///
    /// Returns `true` if `raw` was a valid hex color and was applied, or `false` if invalid.
    #[cfg(test)]
    pub(crate) fn apply_custom_accent_hex(
        &mut self,
        raw: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(accent) = options::parse_custom_accent_hex(raw) else {
            return false;
        };
        self.set_accent_color(accent, window, cx);
        true
    }

    /// Toggles whether Loom follows the operating system's active accent color.
    pub(crate) fn set_follow_system_accent(
        &mut self,
        follow: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let next = if follow {
            AccentColorPreference::System
        } else {
            let (r, g, b) = self.last_custom_accent.unwrap_or_else(system_accent_rgb);
            AccentColorPreference::custom(r, g, b)
        };
        self.set_accent_color(next, window, cx);
    }

    fn refresh_select_labels(&self, window: &mut Window, cx: &mut Context<Self>) {
        let current_hotkey = self.store.read(cx).settings().global_hotkey.clone();
        self.startup_page_select.update(cx, |state, cx| {
            state.set_items(startup_page_options(), window, cx);
        });
        self.refresh_speed_select.update(cx, |state, cx| {
            state.set_items(refresh_speed_options(), window, cx);
        });
        self.global_hotkey_select.update(cx, |state, cx| {
            state.set_items(global_hotkey_options(current_hotkey.as_deref()), window, cx);
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
        let (settings, is_elevated, capabilities, config_path) = {
            let store_ref = self.store.read(cx);
            let snapshot = store_ref.snapshot();
            (
                store_ref.settings().clone(),
                snapshot.is_elevated,
                snapshot.capabilities,
                store_ref.config_path().display().to_string(),
            )
        };
        let palette = SettingsThemePalette::from_cx(cx);

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
                        div()
                            .text_xl()
                            .font_semibold()
                            .text_color(palette.fg)
                            .child(i18n::tr("settings.title")),
                    )
                    .child(render_general_section(self, &palette))
                    .child(render_diagnostics_section(&settings, &palette, cx))
                    .child(render_window_section(
                        self,
                        &settings,
                        capabilities,
                        &palette,
                    ))
                    .child(render_appearance_section(self, &settings, &palette, cx))
                    .child(render_privileges_section(
                        self,
                        is_elevated,
                        capabilities,
                        config_path,
                        &palette,
                        cx,
                    )),
            )
    }
}
