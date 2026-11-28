//! Complete inline Settings vertical feature slice (zero modals required).
//!
//! All controls apply live to the running engine, native window, theme, and locale
//! and persist atomically to `settings.toml` via [`UiCommands`] / [`UiStore`].

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::{Select, SelectEvent, SelectItem, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, IndexPath, Sizable as _, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Subscription, Task, Window, div,
    prelude::FluentBuilder as _, px,
};
use loom_engine::{CommandOutcome, CommandSuccess, EngineCommand};
use loom_types::{
    AdminStartupStatus, CapabilityFlags, LanguagePreference, RefreshSpeed, StartupPage,
    TaskManagerReplacementState, TaskManagerReplacementStatus, ThemePreference,
};

use crate::bootstrap;
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

const GLOBAL_HOTKEY_PRESETS: [Option<&str>; 5] = [
    None,
    Some("Ctrl+Shift+L"),
    Some("Ctrl+Alt+L"),
    Some("Ctrl+Shift+M"),
    Some("Alt+Shift+T"),
];

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

fn global_hotkey_options(current: Option<&str>) -> Vec<SelectOption<Option<String>>> {
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

fn hotkey_index_of(
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
    global_hotkey_select: Entity<SelectState<Vec<SelectOption<Option<String>>>>>,
    theme_select: Entity<SelectState<Vec<SelectOption<ThemePreference>>>>,
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
        let language_select = cx.new(|cx| {
            SelectState::new(
                language_options(),
                index_of(&LANGUAGE_PREFS, &settings.language),
                window,
                cx,
            )
        });

        let mut subscriptions = Vec::with_capacity(6);

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

        let mut view = Self {
            store,
            last_locale: settings.language,
            startup_page_select,
            refresh_speed_select,
            global_hotkey_select,
            theme_select,
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
                let _ = weak.update_in(cx, |this, _window, cx| {
                    this.admin_busy = false;
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
        let store_ref = self.store.read(cx);
        let settings = store_ref.settings().clone();
        let snapshot = store_ref.snapshot();
        let is_elevated = snapshot.is_elevated;
        let capabilities = snapshot.capabilities;
        let supports_global_hotkey = capabilities.contains(CapabilityFlags::GLOBAL_HOTKEY);
        let supports_elevation_relaunch =
            capabilities.contains(CapabilityFlags::ELEVATION_RELAUNCH);
        let supports_admin_startup = capabilities.contains(CapabilityFlags::ADMIN_STARTUP);
        let supports_taskmgr_replacement =
            capabilities.contains(CapabilityFlags::TASK_MANAGER_REPLACEMENT);
        let config_path = store_ref.config_path().display().to_string();

        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let warn_fg = theme.warning;
        let warn_bg = theme.warning.opacity(0.16);
        let danger_fg = theme.danger;
        let danger_bg = theme.danger.opacity(0.16);
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

        let store_min = self.store.clone();
        let store_close = self.store.clone();
        let store_elevate = self.store.clone();
        let store_native_taskmgr = self.store.clone();

        let startup_label = i18n::tr("settings.startup_page_label");
        let speed_label = i18n::tr("settings.refresh_speed_label");
        let wv2_label = i18n::tr("settings.settings_diag_webview2_grouping");
        let leak_label = i18n::tr("settings.settings_diag_leak_warnings");
        let hide_sys_label = i18n::tr("settings.settings_diag_hide_system");
        let min_tray_label = i18n::tr("settings.minimize_to_tray_label");
        let close_tray_label = i18n::tr("settings.close_to_tray_label");
        let global_hotkey_label = i18n::tr("settings.global_hotkey_label");
        let theme_label = i18n::tr("settings.theme_label");
        let language_label = i18n::tr("settings.language_label");
        let taskmgr_replace_label = i18n::tr("settings.taskmgr_replace_label");
        let taskmgr_open_native_label = i18n::tr("settings.taskmgr_open_native_label");
        let admin_startup_label = i18n::tr("settings.admin_startup_label");

        let admin_status = self.admin_status.clone();
        let admin_busy = self.admin_busy;
        let admin_checked = admin_status.as_ref().is_some_and(|s| s.enabled);
        let admin_needs_repair = admin_status.as_ref().is_some_and(|s| s.enabled && !s.ready);
        let admin_switch_disabled = admin_busy
            || match admin_status.as_ref() {
                None => true,
                Some(s) if s.enabled => false,
                Some(s) => !s.supported || !s.eligible,
            };
        let (admin_desc, admin_desc_color, admin_badge) = match admin_status.as_ref() {
            None => (None, muted_fg, None),
            Some(s) if !s.eligible => (
                Some(i18n::tr("settings.admin_startup_ineligible_desc")),
                muted_fg,
                None,
            ),
            Some(s) if s.enabled && !s.ready => (
                Some(i18n::tr("settings.admin_startup_repair_desc")),
                warn_fg,
                Some((
                    i18n::tr("settings.admin_startup_badge_repair"),
                    warn_bg,
                    warn_fg,
                )),
            ),
            Some(_) => (None, muted_fg, None),
        };

        let taskmgr_status = self.taskmgr_status.clone();
        let taskmgr_busy = self.taskmgr_busy;
        let taskmgr_checked = taskmgr_status
            .as_ref()
            .is_some_and(|s| s.state == TaskManagerReplacementState::Enabled);
        let taskmgr_switch_disabled = taskmgr_busy
            || match taskmgr_status.as_ref() {
                None => true,
                Some(s) if s.state == TaskManagerReplacementState::Enabled => !s.can_restore,
                Some(s) => !s.can_enable,
            };
        let taskmgr_show_restore = taskmgr_status.as_ref().is_some_and(|s| {
            s.can_restore
                && matches!(
                    s.state,
                    TaskManagerReplacementState::Revoked
                        | TaskManagerReplacementState::RecoveryNeeded
                )
        });
        let (taskmgr_desc, taskmgr_desc_color, taskmgr_badge) = match taskmgr_status.as_ref() {
            None => (None, muted_fg, None),
            Some(s) => match s.state {
                TaskManagerReplacementState::Disabled
                | TaskManagerReplacementState::Enabled
                | TaskManagerReplacementState::Unsupported => (None, muted_fg, None),
                TaskManagerReplacementState::Conflict => {
                    let detail = s
                        .conflict_detail
                        .as_deref()
                        .unwrap_or(&i18n::tr("settings.taskmgr_replace_conflict_unknown"))
                        .to_owned();
                    let msg = rust_i18n::t!(
                        "settings.taskmgr_replace_conflict_desc",
                        detail = detail.as_str()
                    )
                    .into_owned();
                    (
                        Some(msg),
                        danger_fg,
                        Some((
                            i18n::tr("settings.taskmgr_replace_badge_conflict"),
                            danger_bg,
                            danger_fg,
                        )),
                    )
                }
                TaskManagerReplacementState::Revoked => (
                    Some(i18n::tr("settings.taskmgr_replace_revoked_desc")),
                    warn_fg,
                    Some((
                        i18n::tr("settings.taskmgr_replace_badge_revoked"),
                        warn_bg,
                        warn_fg,
                    )),
                ),
                TaskManagerReplacementState::RecoveryNeeded => (
                    Some(i18n::tr("settings.taskmgr_replace_recovery_desc")),
                    warn_fg,
                    Some((
                        i18n::tr("settings.taskmgr_replace_badge_recovery"),
                        warn_bg,
                        warn_fg,
                    )),
                ),
            },
        };

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
                            .text_color(fg)
                            .child(i18n::tr("settings.title")),
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
                            fg,
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
                            fg,
                            border,
                            true,
                            div().w(px(220.)).child(
                                Select::new(&self.refresh_speed_select)
                                    .small()
                                    .accessibility_label(speed_label),
                            ),
                        )),
                    )
                    // 2. Processes & Diagnostics
                    .child(
                        settings_section(
                            i18n::tr("settings.settings_diag_section"),
                            IconName::Settings2,
                            card_bg,
                            border,
                            fg,
                            radius_lg,
                        )
                        .child(setting_row(
                            wv2_label.clone(),
                            fg,
                            border,
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
                            fg,
                            border,
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
                            fg,
                            border,
                            true,
                            Switch::new("setting-hide-system-processes")
                                .checked(settings.hide_windows_system_processes)
                                .accessibility_label(hide_sys_label)
                                .on_change(cx.listener(|view, checked, _, cx| {
                                    view.set_hide_system_processes(*checked, cx);
                                })),
                        )),
                    )
                    // 3. Window Management
                    .child(
                        settings_section(
                            i18n::tr("settings.section_window"),
                            IconName::LayoutDashboard,
                            card_bg,
                            border,
                            fg,
                            radius_lg,
                        )
                        .child(setting_row(
                            min_tray_label.clone(),
                            fg,
                            border,
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
                            fg,
                            border,
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
                                fg,
                                border,
                                true,
                                div().w(px(220.)).child(
                                    Select::new(&self.global_hotkey_select)
                                        .small()
                                        .accessibility_label(global_hotkey_label),
                                ),
                            ))
                        }),
                    )
                    // 4. Appearance & Language
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
                            fg,
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
                            fg,
                            border,
                            true,
                            div().w(px(220.)).child(
                                Select::new(&self.language_select)
                                    .small()
                                    .accessibility_label(language_label),
                            ),
                        )),
                    )
                    // 5. Privileges & Storage
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
                            fg,
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
                        .when(supports_admin_startup, |section| {
                            section.child(setting_row_with_description(
                                admin_startup_label.clone(),
                                admin_desc,
                                fg,
                                admin_desc_color,
                                border,
                                true,
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .when_some(admin_badge, |this, (label, bg, color)| {
                                        this.child(
                                            div()
                                                .px_2()
                                                .py_0p5()
                                                .rounded(radius)
                                                .bg(bg)
                                                .text_xs()
                                                .font_medium()
                                                .text_color(color)
                                                .child(label),
                                        )
                                    })
                                    .when(admin_needs_repair, |this| {
                                        this.child(
                                            Button::new("settings-repair-admin-startup")
                                                .small()
                                                .disabled(admin_busy)
                                                .label(i18n::tr(
                                                    "settings.admin_startup_repair_btn",
                                                ))
                                                .on_click(cx.listener(|view, _, window, cx| {
                                                    view.set_admin_startup(true, window, cx);
                                                })),
                                        )
                                    })
                                    .child(
                                        Switch::new("setting-admin-startup")
                                            .checked(admin_checked)
                                            .disabled(admin_switch_disabled)
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
                                    taskmgr_desc,
                                    fg,
                                    taskmgr_desc_color,
                                    border,
                                    true,
                                    h_flex()
                                        .items_center()
                                        .gap_2()
                                        .when_some(taskmgr_badge, |this, (label, bg, color)| {
                                            this.child(
                                                div()
                                                    .px_2()
                                                    .py_0p5()
                                                    .rounded(radius)
                                                    .bg(bg)
                                                    .text_xs()
                                                    .font_medium()
                                                    .text_color(color)
                                                    .child(label),
                                            )
                                        })
                                        .when(taskmgr_show_restore, |this| {
                                            this.child(
                                                Button::new("settings-restore-task-manager")
                                                    .small()
                                                    .disabled(taskmgr_busy)
                                                    .label(i18n::tr(
                                                        "settings.taskmgr_replace_restore_btn",
                                                    ))
                                                    .on_click(cx.listener(
                                                        |view, _, window, cx| {
                                                            view.set_task_manager_replacement(
                                                                false, window, cx,
                                                            );
                                                        },
                                                    )),
                                            )
                                        })
                                        .child(
                                            Switch::new("setting-replace-task-manager")
                                                .checked(taskmgr_checked)
                                                .disabled(taskmgr_switch_disabled)
                                                .accessibility_label(taskmgr_replace_label.clone())
                                                .on_change(cx.listener(
                                                    |view, checked, window, cx| {
                                                        view.set_task_manager_replacement(
                                                            *checked, window, cx,
                                                        );
                                                    },
                                                )),
                                        ),
                                ))
                                .child(setting_row(
                                    taskmgr_open_native_label,
                                    fg,
                                    border,
                                    true,
                                    Button::new("settings-open-native-taskmgr")
                                        .small()
                                        .label(i18n::tr("settings.taskmgr_open_native_btn"))
                                        .on_click(move |_, _, cx| {
                                            UiCommands::open_native_task_manager(
                                                &store_native_taskmgr,
                                                cx,
                                            );
                                        }),
                                ))
                        })
                        .child(setting_row(
                            i18n::tr("settings.config_path_label"),
                            fg,
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
    fg: gpui_kit::Hsla,
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
        .py_3()
        .when(top_divider, |this| this.border_t_1().border_color(border))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .font_medium()
                .text_color(fg)
                .child(title),
        )
        .child(div().flex_shrink_0().child(control))
}

fn setting_row_with_description(
    title: String,
    description: Option<String>,
    fg: gpui_kit::Hsla,
    desc_fg: gpui_kit::Hsla,
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
        .py_3()
        .when(top_divider, |this| this.border_t_1().border_color(border))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(div().text_sm().font_medium().text_color(fg).child(title))
                .when_some(description, |this, text| {
                    this.child(div().text_xs().text_color(desc_fg).child(text))
                }),
        )
        .child(div().flex_shrink_0().child(control))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui_kit::{AppContext as _, TestAppContext, VisualTestContext};
    use loom_config::{AppSettings, ConfigStore};
    use loom_engine::EngineCoordinator;
    use loom_pal::{NativeBackend, PlatformBackend};

    use crate::bridge::UiStore;

    use super::SettingsView;

    #[gpui_kit::test]
    fn settings_view_diagnostic_toggles_persist_and_reload_cleanly(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);

        let dir = tempfile::tempdir().expect("create temp config dir");
        let config_path = dir.path().join("settings.toml");
        let config_store = ConfigStore::new(config_path.clone());
        let settings = AppSettings::default();
        assert!(settings.webview2_host_grouping);
        assert!(settings.show_leak_warnings);
        assert!(!settings.hide_windows_system_processes);

        let backend = NativeBackend::initialize().expect("initialize native backend");
        let coordinator =
            Arc::new(EngineCoordinator::new(backend, settings.clone()).expect("start coordinator"));
        let store = cx.new(|cx| UiStore::new(coordinator.clone(), config_store, settings, cx));

        let mut settings_view_handle = None;
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                let view = cx.new(|cx| SettingsView::new(store.clone(), window, cx));
                settings_view_handle = Some(view.clone());
                cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
            })
            .expect("open settings test window")
        });
        let settings_view = settings_view_handle.expect("SettingsView constructed");

        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.update(|window, cx| window.draw(cx).clear(cx));

        // Flip all 3 additive v1 toggles via SettingsView.
        vcx.update(|_window, cx| {
            settings_view.update(cx, |view, cx| {
                view.set_webview2_host_grouping(false, cx);
                view.set_show_leak_warnings(false, cx);
                view.set_hide_system_processes(true, cx);
            });
        });

        // Verify in-memory UiStore state updated immediately.
        vcx.update(|_window, cx| {
            let s = store.read(cx).settings();
            assert!(!s.webview2_host_grouping);
            assert!(!s.show_leak_warnings);
            assert!(s.hide_windows_system_processes);
        });

        // Verify persisted settings.toml reloads identically.
        let reloaded = ConfigStore::new(config_path)
            .load()
            .expect("reload persisted settings.toml");
        assert!(!reloaded.webview2_host_grouping);
        assert!(!reloaded.show_leak_warnings);
        assert!(reloaded.hide_windows_system_processes);

        // On platforms advertising TASK_MANAGER_REPLACEMENT / ADMIN_STARTUP (Windows),
        // SettingsView must query read-only status asynchronously on mount without panicking.
        let caps = NativeBackend::CAPABILITIES;
        if caps.contains(loom_types::CapabilityFlags::TASK_MANAGER_REPLACEMENT)
            || caps.contains(loom_types::CapabilityFlags::ADMIN_STARTUP)
        {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while std::time::Instant::now() < deadline {
                vcx.run_until_parked();
                let ready = vcx.update(|_, cx| {
                    let v = settings_view.read(cx);
                    (!caps.contains(loom_types::CapabilityFlags::TASK_MANAGER_REPLACEMENT)
                        || v.taskmgr_status.is_some())
                        && (!caps.contains(loom_types::CapabilityFlags::ADMIN_STARTUP)
                            || v.admin_status.is_some())
                });
                if ready {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if caps.contains(loom_types::CapabilityFlags::TASK_MANAGER_REPLACEMENT) {
                let status = vcx.update(|_, cx| settings_view.read(cx).taskmgr_status.clone());
                assert!(
                    status.is_some(),
                    "SettingsView should resolve read-only TaskManagerReplacementStatus on mount"
                );
            }
            if caps.contains(loom_types::CapabilityFlags::ADMIN_STARTUP) {
                let status = vcx.update(|_, cx| settings_view.read(cx).admin_status.clone());
                assert!(
                    status.is_some(),
                    "SettingsView should resolve read-only AdminStartupStatus on mount"
                );
            }
            vcx.update(|window, cx| window.draw(cx).clear(cx));
        }

        drop(settings_view);
        vcx.update(|window, _| window.remove_window());
        vcx.run_until_parked();
        coordinator.shutdown();
    }
}
