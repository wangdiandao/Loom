//! Main desktop window shell (`AppShell`) with TitleBar search, structured query syntax help,
//! global keyboard shortcuts, Hold-`Ctrl` temporary freeze, shell tools menu, collapsible Sidebar,
//! route outlet, on-demand elevation notifications, and window visibility/close hooks.

use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::popover::Popover;
use gpui_kit::component::sidebar::{
    Sidebar, SidebarItem as _, SidebarMenu, SidebarMenuItem, SidebarToggleButton,
};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, TitleBar, WindowExt as _,
    h_flex, v_flex,
};
use gpui_kit::{
    Anchor, AppContext as _, ClipboardItem, Context, Entity, FocusHandle, Focusable as _,
    InteractiveElement as _, IntoElement, KeyDownEvent, Modifiers, ModifiersChangedEvent,
    MouseButton, ParentElement as _, Render, Styled as _, Subscription, Task, Window,
    WindowVisibility, div, prelude::FluentBuilder as _, px,
};
use loom_types::{
    CapabilityFlags, LanguagePreference, PlatformKind, ProcessKey, RefreshSpeed, ThemePreference,
};
use loom_ui_kit::LoomSemanticColors;

use crate::bootstrap;
use crate::bridge::{UiCommands, UiStore, UiStoreEvent, apply_theme};
use crate::features::{
    AppHistoryView, DetailsView, PerformanceView, ProcessesView, ServicesView, SettingsView,
    StartupView, UsersView,
};
use crate::i18n;
use crate::shell::diagnostic_summary::build_diagnostic_summary;
use crate::shell::dialogs::open_file_locks_dialog;
use crate::shell::nav::{NavigationIntent, Route, ShellHandle};

/// Debounce interval for typing in the global TitleBar search box.
const SEARCH_DEBOUNCE_DURATION: Duration = Duration::from_millis(150);

/// Unique notification type tag for deduplicating on-demand elevation prompts.
struct ElevationNotice;

/// Supported structured query syntax items shown in the `?` search syntax help popover.
const SEARCH_SYNTAX_EXAMPLES: [(&str, &str); 8] = [
    ("chrome, edge", "shell_hotkeys.syntax_or_desc"),
    ("!svchost", "shell_hotkeys.syntax_not_desc"),
    ("pid:1234", "shell_hotkeys.syntax_pid_desc"),
    ("user:system", "shell_hotkeys.syntax_user_desc"),
    ("cpu:>10", "shell_hotkeys.syntax_cpu_desc"),
    ("mem:>500m", "shell_hotkeys.syntax_mem_desc"),
    ("status:hung", "shell_hotkeys.syntax_status_desc"),
    (":3000", "shell_hotkeys.syntax_port_desc"),
];

/// Maps `Ctrl+1`..`Ctrl+8` digit keys to their corresponding [`Route`].
pub(crate) fn route_for_ctrl_digit(key: &str) -> Option<Route> {
    match key {
        "1" => Some(Route::Processes),
        "2" => Some(Route::Performance),
        "3" => Some(Route::AppHistory),
        "4" => Some(Route::Startup),
        "5" => Some(Route::Users),
        "6" => Some(Route::Details),
        "7" => Some(Route::Services),
        "8" => Some(Route::Settings),
        _ => None,
    }
}

/// Returns `true` when Hold-`Ctrl` temporary freeze should be active for `modifiers`.
#[inline]
pub(crate) const fn should_hold_ctrl_freeze(
    modifiers: Modifiers,
    has_focused_input: bool,
    has_modal_overlay: bool,
) -> bool {
    modifiers.control
        && !modifiers.alt
        && !modifiers.shift
        && !modifiers.platform
        && !has_focused_input
        && !has_modal_overlay
}

/// Mounted page entity owned by [`AppShell`].
///
/// Dropping on-demand views ([`ActivePage::Performance`], [`ActivePage::AppHistory`],
/// [`ActivePage::Startup`], [`ActivePage::Users`], [`ActivePage::Details`], or
/// [`ActivePage::Services`]) when switching away immediately releases their RAII
/// [`crate::bridge::ScopedMonitorLease`]s.
enum ActivePage {
    Processes(Entity<ProcessesView>),
    Performance(Entity<PerformanceView>),
    AppHistory(Entity<AppHistoryView>),
    Startup(Entity<StartupView>),
    Users(Entity<UsersView>),
    Details(Entity<DetailsView>),
    Services(Entity<ServicesView>),
    Settings(Entity<SettingsView>),
}

/// Top-level application view wrapped by `gpui-kit`'s `Root` layer.
pub(crate) struct AppShell {
    store: Entity<UiStore>,
    focus_handle: FocusHandle,
    route: Route,
    active_page: ActivePage,
    search_input: Entity<InputState>,
    search_debounce_task: Option<Task<()>>,
    syntax_help_open: bool,
    last_locale: LanguagePreference,
    last_elevated: bool,
    _subscriptions: Vec<Subscription>,
}

impl AppShell {
    /// Constructs a new [`AppShell`] positioned on `initial_route`.
    #[cfg(test)]
    pub(crate) fn new(
        store: Entity<UiStore>,
        initial_route: Route,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_highlight(store, initial_route, None, window, cx)
    }

    /// Constructs a new [`AppShell`] positioned on `initial_route`, optionally highlighting
    /// a [`ProcessKey`] when opening directly into [`Route::Details`].
    pub(crate) fn new_with_highlight(
        store: Entity<UiStore>,
        initial_route: Route,
        initial_details_highlight: Option<ProcessKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial_settings = store.read(cx).settings().clone();
        let initial_elevated = store.read(cx).snapshot().is_elevated;

        window.set_window_title(&i18n::tr("app.window_title"));

        let focus_handle = cx.focus_handle();
        let search_input = cx
            .new(|cx| InputState::new(window, cx).placeholder(i18n::tr("app.search_placeholder")));

        let mut subscriptions = Vec::with_capacity(5);

        // Subscribe to TitleBar search input changes with ~150 ms debounce (instant clear).
        subscriptions.push(cx.subscribe_in(
            &search_input,
            window,
            |this, state, event: &InputEvent, window, cx| {
                if !matches!(event, InputEvent::Change | InputEvent::PressEnter { .. }) {
                    return;
                }
                let query = state.read(cx).value().to_string();
                if query.is_empty() || matches!(event, InputEvent::PressEnter { .. }) {
                    this.search_debounce_task = None;
                    UiCommands::set_search_query(&this.store, query, cx);
                    return;
                }
                let store = this.store.clone();
                this.search_debounce_task = Some(cx.spawn_in(window, async move |_this, cx| {
                    cx.background_executor()
                        .timer(SEARCH_DEBOUNCE_DURATION)
                        .await;
                    let _ = cx.update(|_window, cx| {
                        UiCommands::set_search_query(&store, query, cx);
                    });
                }));
            },
        ));

        // Subscribe to UiStore domain events (elevation notices, locale/settings changes).
        subscriptions.push(cx.subscribe_in(
            &store,
            window,
            |this, store, event: &UiStoreEvent, window, cx| match event {
                UiStoreEvent::ElevationRequired { command } => {
                    tracing::info!(?command, "surfacing on-demand elevation notification");
                    this.push_elevation_notification(window, cx);
                }
                UiStoreEvent::SettingsChanged => {
                    let next_locale = store.read(cx).settings().language;
                    if this.last_locale != next_locale {
                        this.last_locale = next_locale;
                        let placeholder = i18n::tr("app.search_placeholder");
                        this.search_input.update(cx, |state, cx| {
                            state.set_placeholder(placeholder, window, cx);
                        });
                        window.set_window_title(&i18n::tr("app.window_title"));
                    }
                    cx.notify();
                }
                UiStoreEvent::SnapshotUpdated(_) => {
                    let elevated = store.read(cx).snapshot().is_elevated;
                    if this.last_elevated != elevated {
                        this.last_elevated = elevated;
                        cx.notify();
                    }
                }
                UiStoreEvent::CommandFailed { command, message } => {
                    tracing::warn!(?command, %message, "surfacing command error notification");
                    window.push_notification(Notification::error(message.clone()), cx);
                }
                UiStoreEvent::SearchQueryChanged => {}
            },
        ));

        // Follow OS light/dark changes automatically when ThemePreference::System is active.
        let store_for_appearance = store.clone();
        subscriptions.push(window.observe_window_appearance(move |window, cx| {
            if store_for_appearance.read(cx).settings().theme == ThemePreference::System {
                apply_theme(ThemePreference::System, Some(window), cx);
            }
        }));

        // Always release Hold-Ctrl temporary freeze when the window loses focus (e.g., Alt+Tab).
        subscriptions.push(cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() && this.store.read(cx).is_temp_frozen() {
                UiCommands::set_temp_freeze(&this.store, false, cx);
            }
        }));

        // Pause fast-lane ticks while hidden/minimized and optionally hide to tray on minimize.
        let store_for_visibility = store.clone();
        subscriptions.push(
            window.observe_window_visibility(move |visibility, window, cx| match visibility {
                WindowVisibility::Hidden => {
                    let minimize_to_tray =
                        store_for_visibility.read(cx).settings().minimize_to_tray;
                    if minimize_to_tray && bootstrap::is_main_window_minimized() {
                        bootstrap::mark_hiding_to_tray(cx);
                        window.remove_window();
                    } else if !bootstrap::set_main_window_visible(false, cx) {
                        store_for_visibility.update(cx, |store, cx| {
                            store.set_window_dormant(true, cx);
                        });
                    }
                }
                WindowVisibility::Visible => {
                    if !bootstrap::set_main_window_visible(true, cx) {
                        store_for_visibility.update(cx, |store, cx| {
                            store.set_window_dormant(false, cx);
                        });
                    }
                }
            }),
        );

        // Persist window geometry right before closing; actual quit vs tray residency is handled
        // cleanly by `bootstrap::AppRuntime`'s `on_window_closed` observer.
        let store_for_close = store.clone();
        window.on_window_should_close(cx, move |window, cx| {
            bootstrap::persist_window_geometry(&store_for_close, window, cx);
            true
        });

        // Apply initial Always-on-Top state once the native window is mapped.
        let initial_topmost = initial_settings.always_on_top
            && store
                .read(cx)
                .snapshot()
                .capabilities
                .contains(CapabilityFlags::ALWAYS_ON_TOP);
        if initial_topmost {
            window.defer(cx, move |_window, _cx| {
                bootstrap::apply_always_on_top(true);
            });
        }

        let shell_handle = Self::make_shell_handle(cx);
        let active_page = Self::mount_route(
            &store,
            initial_route,
            initial_details_highlight,
            None,
            shell_handle,
            window,
            cx,
        );
        bootstrap::record_active_route(initial_route, cx);
        focus_handle.focus(window, cx);

        Self {
            store,
            focus_handle,
            route: initial_route,
            active_page,
            search_input,
            search_debounce_task: None,
            syntax_help_open: false,
            last_locale: initial_settings.language,
            last_elevated: initial_elevated,
            _subscriptions: subscriptions,
        }
    }

    /// Handles a navigation intent initiated externally (e.g., from the tray Quick Overview popover).
    pub(crate) fn handle_external_intent(
        &mut self,
        intent: NavigationIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_intent(intent, window, cx);
    }

    #[cfg(test)]
    pub(crate) const fn current_route(&self) -> Route {
        self.route
    }

    fn make_shell_handle(cx: &Context<Self>) -> ShellHandle {
        let weak = cx.entity().downgrade();
        ShellHandle::new(move |intent, window, cx| {
            let _ = weak.update(cx, |shell, cx| {
                shell.handle_intent(intent, window, cx);
            });
        })
    }

    fn mount_route(
        store: &Entity<UiStore>,
        route: Route,
        highlight_details: Option<ProcessKey>,
        filter_services_pid: Option<u32>,
        shell_handle: ShellHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ActivePage {
        match route {
            Route::Processes => {
                let store = store.clone();
                ActivePage::Processes(
                    cx.new(|cx| ProcessesView::new(store, shell_handle, window, cx)),
                )
            }
            Route::Performance => {
                let store = store.clone();
                ActivePage::Performance(cx.new(|cx| PerformanceView::new(store, window, cx)))
            }
            Route::AppHistory => {
                let store = store.clone();
                ActivePage::AppHistory(cx.new(|cx| AppHistoryView::new(store, window, cx)))
            }
            Route::Startup => {
                let store = store.clone();
                ActivePage::Startup(cx.new(|cx| StartupView::new(store, window, cx)))
            }
            Route::Users => {
                let store = store.clone();
                ActivePage::Users(cx.new(|cx| UsersView::new(store, shell_handle, window, cx)))
            }
            Route::Details => {
                let store = store.clone();
                ActivePage::Details(
                    cx.new(|cx| {
                        DetailsView::new(store, highlight_details, shell_handle, window, cx)
                    }),
                )
            }
            Route::Services => {
                let store = store.clone();
                ActivePage::Services(cx.new(|cx| {
                    ServicesView::new(store, filter_services_pid, shell_handle, window, cx)
                }))
            }
            Route::Settings => {
                let store = store.clone();
                ActivePage::Settings(cx.new(|cx| SettingsView::new(store, window, cx)))
            }
        }
    }

    fn navigate_to(
        &mut self,
        route: Route,
        highlight_details: Option<ProcessKey>,
        filter_services_pid: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.route == route {
            if let (Route::Details, ActivePage::Details(details)) = (route, &self.active_page)
                && highlight_details.is_some()
            {
                details.update(cx, |view, cx| {
                    view.set_highlighted_key(highlight_details, cx);
                });
            }
            if let (Route::Services, ActivePage::Services(services)) = (route, &self.active_page)
                && filter_services_pid.is_some()
            {
                services.update(cx, |view, cx| {
                    view.set_filter_pid(filter_services_pid, cx);
                });
            }
            return;
        }

        let shell_handle = Self::make_shell_handle(cx);
        self.route = route;
        self.active_page = Self::mount_route(
            &self.store,
            route,
            highlight_details,
            filter_services_pid,
            shell_handle,
            window,
            cx,
        );
        bootstrap::record_active_route(route, cx);
        cx.notify();
    }

    fn handle_intent(
        &mut self,
        intent: NavigationIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match intent {
            NavigationIntent::Navigate(route) => {
                self.navigate_to(route, None, None, window, cx);
            }
            NavigationIntent::JumpToDetails(key) => {
                self.navigate_to(Route::Details, Some(key), None, window, cx);
            }
            NavigationIntent::JumpToServices(pid) => {
                self.navigate_to(Route::Services, None, Some(pid), window, cx);
            }
        }
    }

    fn focus_search_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_input.focus_handle(cx).focus(window, cx);
    }

    fn apply_search_example(&mut self, example: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.syntax_help_open = false;
        self.search_debounce_task = None;
        let query = example.to_owned();
        self.search_input.update(cx, |state, cx| {
            state.set_value(query.clone(), window, cx);
        });
        UiCommands::set_search_query(&self.store, query, cx);
        self.focus_search_input(window, cx);
        cx.notify();
    }

    fn clear_search_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_debounce_task = None;
        self.search_input.update(cx, |state, cx| {
            state.set_value(String::new(), window, cx);
        });
        UiCommands::set_search_query(&self.store, String::new(), cx);
        cx.notify();
    }

    fn copy_diagnostic_summary(&self, window: &mut Window, cx: &mut Context<Self>) {
        let store_ref = self.store.read(cx);
        let summary = build_diagnostic_summary(store_ref.snapshot(), store_ref.settings());
        cx.write_to_clipboard(ClipboardItem::new_string(summary));
        window.push_notification(
            Notification::success(i18n::tr("diag_summary.copied_message"))
                .title(i18n::tr("diag_summary.copied_title")),
            cx,
        );
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let has_modal = window.has_active_dialog(cx) || window.has_active_sheet(cx);
        if has_modal {
            return;
        }

        let mods = event.keystroke.modifiers;
        let key = event.keystroke.key.as_str();
        let ctrl_only = mods.control && !mods.alt && !mods.shift && !mods.platform;
        let no_mods = !mods.control && !mods.alt && !mods.shift && !mods.platform;
        let input_focused = window.has_focused_input(cx);

        // `Ctrl+F`: focus search box from anywhere in the main shell.
        if ctrl_only && key.eq_ignore_ascii_case("f") {
            self.focus_search_input(window, cx);
            cx.stop_propagation();
            return;
        }

        // `Ctrl+1`..`Ctrl+8`: switch directly to the 8 navigation routes.
        if ctrl_only && let Some(target_route) = route_for_ctrl_digit(key) {
            self.navigate_to(target_route, None, None, window, cx);
            cx.stop_propagation();
            return;
        }

        // `F5`: force immediate sampling refresh even when paused.
        if no_mods && key.eq_ignore_ascii_case("f5") {
            UiCommands::force_refresh(&self.store, cx);
            cx.stop_propagation();
            return;
        }

        // `Esc`: dismiss syntax popover, clear active search query, or blur search input.
        if no_mods && key == "escape" {
            if self.syntax_help_open {
                self.syntax_help_open = false;
                cx.notify();
                cx.stop_propagation();
                return;
            }
            let search_focused = self.search_input.focus_handle(cx).is_focused(window);
            let has_search_text = !self.search_input.read(cx).value().is_empty()
                || !self.store.read(cx).search_query().is_empty();
            if search_focused && has_search_text {
                self.clear_search_query(window, cx);
                cx.stop_propagation();
                return;
            }
            if search_focused {
                self.focus_handle.focus(window, cx);
                cx.stop_propagation();
                return;
            }
            return;
        }

        // Single-key shortcuts (`/`, `Space`, `P`) are suppressed whenever a text Input has focus.
        if input_focused || !no_mods {
            return;
        }

        match key {
            "/" => {
                self.focus_search_input(window, cx);
                cx.stop_propagation();
            }
            "space" | " " | "p" | "P" => {
                UiCommands::toggle_pause_refresh(&self.store, cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    fn handle_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let has_modal = window.has_active_dialog(cx) || window.has_active_sheet(cx);
        let input_focused = window.has_focused_input(cx);
        let freeze = should_hold_ctrl_freeze(event.modifiers, input_focused, has_modal);
        UiCommands::set_temp_freeze(&self.store, freeze, cx);
    }

    fn push_elevation_notification(&self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let supports_relaunch = store
            .read(cx)
            .snapshot()
            .capabilities
            .contains(CapabilityFlags::ELEVATION_RELAUNCH);
        let current_route = self.route.as_str();
        let action_label = i18n::tr("uac.action");

        let mut notice = Notification::warning(i18n::tr("uac.message"))
            .id::<ElevationNotice>()
            .title(i18n::tr("uac.title"))
            .autohide(false);

        if supports_relaunch {
            notice = notice.action(move |_, _, cx| {
                let store = store.clone();
                Button::new("elevation-restart-as-admin")
                    .primary()
                    .label(action_label.clone())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        UiCommands::relaunch_elevated(&store, current_route, cx);
                        this.dismiss(window, cx);
                    }))
            });
        }

        window.push_notification(notice, cx);
    }
}

impl Render for AppShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store_ref = self.store.read(cx);
        let collapsed = store_ref.settings().sidebar_collapsed;
        let is_paused = store_ref.settings().refresh_speed == RefreshSpeed::Paused;
        let is_temp_frozen = store_ref.is_temp_frozen();
        let snapshot = store_ref.snapshot().clone();
        let is_elevated = snapshot.is_elevated;
        let supports_file_locks = snapshot.capabilities.contains(CapabilityFlags::FILE_LOCKS);
        let is_windows = snapshot.platform.kind == PlatformKind::Windows;
        let active_route = self.route;

        let theme = cx.theme();
        let colors = LoomSemanticColors::from_theme(theme);
        let bg = theme.background;
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let primary = theme.primary;
        let border = theme.border;
        let radius = theme.radius;
        let mono_font = theme.mono_font_family.clone();

        let store_toggle = self.store.clone();
        let store_pause_badge = self.store.clone();
        let store_for_tools = self.store.clone();
        let shell_for_nav = Self::make_shell_handle(cx);
        let shell_for_settings = shell_for_nav.clone();
        let shell_for_tools = shell_for_nav.clone();
        let weak_for_help = cx.entity().downgrade();
        let weak_for_help_toggle = cx.entity().downgrade();
        let weak_for_diag = cx.entity().downgrade();

        let search_help_popover = Popover::new("search-syntax-help-popover")
            .anchor(Anchor::TopRight)
            .open(self.syntax_help_open)
            .on_open_change(move |open, _window, cx| {
                let open = *open;
                let _ = weak_for_help_toggle.update(cx, |this, cx| {
                    this.syntax_help_open = open;
                    cx.notify();
                });
            })
            .trigger(
                Button::new("search-syntax-help-btn")
                    .xsmall()
                    .ghost()
                    .icon(Icon::new(gpui_kit::assets::IconName::CircleQuestionMark).small())
                    .tooltip(i18n::tr("shell_hotkeys.search_help_tooltip")),
            )
            .content(move |_state, _window, cx| {
                let theme = cx.theme();
                let fg = theme.foreground;
                let muted_fg = theme.muted_foreground;
                let border = theme.border;
                let radius = theme.radius;
                let mono = mono_font.clone();

                v_flex()
                    .w(px(360.))
                    .gap_2()
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .text_color(fg)
                                    .child(i18n::tr("shell_hotkeys.search_help_title")),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted_fg)
                                    .child(i18n::tr("shell_hotkeys.search_help_subtitle")),
                            ),
                    )
                    .child(
                        v_flex()
                            .w_full()
                            .gap_1p5()
                            .pt_1()
                            .border_t_1()
                            .border_color(border.opacity(0.6))
                            .children(SEARCH_SYNTAX_EXAMPLES.into_iter().enumerate().map(
                                |(ix, (example, desc_key))| {
                                    let weak = weak_for_help.clone();
                                    let mono_inner = mono.clone();
                                    h_flex()
                                        .w_full()
                                        .items_center()
                                        .justify_between()
                                        .gap_2()
                                        .child(
                                            Button::new(("syntax-example-pill", ix))
                                                .xsmall()
                                                .outline()
                                                .rounded(radius)
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .font_family(mono_inner)
                                                        .child(example),
                                                )
                                                .on_click(move |_, window, cx| {
                                                    let _ = weak.update(cx, |shell, cx| {
                                                        shell.apply_search_example(
                                                            example, window, cx,
                                                        );
                                                    });
                                                }),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .text_right()
                                                .text_xs()
                                                .text_color(muted_fg)
                                                .child(i18n::tr(desc_key)),
                                        )
                                },
                            )),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_end()
                            .pt_1()
                            .child({
                                let weak = weak_for_help.clone();
                                Button::new("syntax-example-port-443")
                                    .xsmall()
                                    .outline()
                                    .rounded(radius)
                                    .child(div().text_xs().font_family(mono).child("port:443"))
                                    .on_click(move |_, window, cx| {
                                        let _ = weak.update(cx, |shell, cx| {
                                            shell.apply_search_example("port:443", window, cx);
                                        });
                                    })
                            }),
                    )
            });

        let tools_menu = div()
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .child(
                Button::new("shell-tools-menu")
                    .xsmall()
                    .ghost()
                    .icon(IconName::Ellipsis)
                    .tooltip(i18n::tr("shell_hotkeys.tools_tooltip"))
                    .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, _window, _cx| {
                        let store_locks = store_for_tools.clone();
                        let shell_locks = shell_for_tools.clone();
                        let weak_diag = weak_for_diag.clone();
                        let store_pause = store_for_tools.clone();
                        let store_refresh = store_for_tools.clone();
                        let store_explorer = store_for_tools.clone();

                        if supports_file_locks {
                            menu = menu.item(
                                PopupMenuItem::new(i18n::tr("shell_hotkeys.menu_find_file_locks"))
                                    .icon(gpui_kit::assets::IconName::FileLock)
                                    .on_click(move |_, window, cx| {
                                        open_file_locks_dialog(
                                            store_locks.clone(),
                                            shell_locks.clone(),
                                            window,
                                            cx,
                                        );
                                    }),
                            );
                        }

                        menu = menu
                            .item(
                                PopupMenuItem::new(i18n::tr(
                                    "shell_hotkeys.menu_copy_diagnostic_summary",
                                ))
                                .icon(IconName::Copy)
                                .on_click(move |_, window, cx| {
                                    let _ = weak_diag.update(cx, |shell, cx| {
                                        shell.copy_diagnostic_summary(window, cx);
                                    });
                                }),
                            )
                            .separator()
                            .item(
                                PopupMenuItem::new(if is_paused {
                                    i18n::tr("shell_hotkeys.menu_resume_updates")
                                } else {
                                    i18n::tr("shell_hotkeys.menu_pause_updates")
                                })
                                .icon(if is_paused {
                                    IconName::Play
                                } else {
                                    IconName::Pause
                                })
                                .on_click(
                                    move |_, _window, cx| {
                                        UiCommands::toggle_pause_refresh(&store_pause, cx);
                                    },
                                ),
                            )
                            .item(
                                PopupMenuItem::new(i18n::tr("shell_hotkeys.menu_force_refresh"))
                                    .icon(IconName::RefreshCw)
                                    .on_click(move |_, _window, cx| {
                                        UiCommands::force_refresh(&store_refresh, cx);
                                    }),
                            );

                        if is_windows {
                            menu = menu.separator().item(
                                PopupMenuItem::new(i18n::tr("shell_hotkeys.menu_restart_explorer"))
                                    .icon(IconName::RotateCw)
                                    .on_click(move |_, window, cx| {
                                        UiCommands::restart_explorer(&store_explorer, cx);
                                        window.push_notification(
                                            Notification::info(i18n::tr(
                                                "shell_hotkeys.explorer_restarting",
                                            )),
                                            cx,
                                        );
                                    }),
                            );
                        }

                        menu
                    }),
            );

        let title_bar = TitleBar::new().child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap_3()
                .pr_2()
                .child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .flex_shrink_0()
                        .child(
                            // Stop propagation on left mouse-down so Windows does not forward
                            // `WM_NCLBUTTONDOWN(HTCAPTION)` to `DefWindowProcW` and swallow the
                            // subsequent release needed for `SidebarToggleButton::on_click`.
                            div()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation();
                                })
                                .child(
                                    SidebarToggleButton::new()
                                        .collapsed(collapsed)
                                        .accessibility_label(i18n::tr("app.sidebar_toggle"))
                                        .on_click(move |_, _, cx| {
                                            UiCommands::set_sidebar_collapsed(
                                                &store_toggle,
                                                !collapsed,
                                                cx,
                                            );
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .text_sm()
                                .font_semibold()
                                .text_color(fg)
                                .child(i18n::tr("app.title")),
                        )
                        .when(is_elevated, |this| {
                            this.child(
                                div()
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded(radius)
                                    .bg(primary.opacity(0.16))
                                    .text_xs()
                                    .font_medium()
                                    .text_color(primary)
                                    .child(i18n::tr("app.admin_badge")),
                            )
                        }),
                )
                .child(
                    h_flex()
                        .flex_1()
                        .justify_center()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .child(
                            div()
                                .w(px(340.))
                                .max_w_full()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation();
                                })
                                .child(
                                    Input::new(&self.search_input)
                                        .small()
                                        .cleanable(true)
                                        .prefix(Icon::new(IconName::Search).small()),
                                ),
                        )
                        .child(
                            div()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation();
                                })
                                .child(search_help_popover),
                        ),
                )
                .child(
                    h_flex()
                        .items_center()
                        .justify_end()
                        .gap_1p5()
                        .flex_shrink_0()
                        .min_w(px(72.))
                        .when(is_temp_frozen, |this| {
                            this.child(
                                div()
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded(radius)
                                    .bg(colors.sort_paused.opacity(0.16))
                                    .border_1()
                                    .border_color(colors.sort_paused.opacity(0.35))
                                    .text_xs()
                                    .font_medium()
                                    .text_color(colors.sort_paused)
                                    .child(i18n::tr("shell_hotkeys.frozen_badge")),
                            )
                        })
                        .when(is_paused && !is_temp_frozen, |this| {
                            this.child(
                                div()
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation();
                                    })
                                    .child(
                                        Button::new("titlebar-paused-badge")
                                            .xsmall()
                                            .outline()
                                            .icon(IconName::Pause)
                                            .label(i18n::tr("shell_hotkeys.paused_badge"))
                                            .tooltip(i18n::tr("shell_hotkeys.paused_tooltip"))
                                            .on_click(move |_, _, cx| {
                                                UiCommands::toggle_pause_refresh(
                                                    &store_pause_badge,
                                                    cx,
                                                );
                                            }),
                                    ),
                            )
                        })
                        .child(tools_menu),
                ),
        );

        let nav_items = Route::PRIMARY_NAV.into_iter().map(|route| {
            let handle = shell_for_nav.clone();
            SidebarMenuItem::new(route.label())
                .icon(route.icon())
                .active(active_route == route)
                .on_click(move |_, window, cx| {
                    handle.navigate(route, window, cx);
                })
        });

        let settings_footer = SidebarMenuItem::new(Route::Settings.label())
            .icon(Route::Settings.icon())
            .active(active_route == Route::Settings)
            .collapsed(collapsed)
            .on_click(move |_, window, cx| {
                shell_for_settings.navigate(Route::Settings, window, cx);
            })
            .render("nav-settings", window, cx);

        let sidebar = Sidebar::new("loom-sidebar")
            .collapsed(collapsed)
            .collapsible(true)
            .w(px(212.))
            .child(SidebarMenu::new().children(nav_items))
            .footer(settings_footer);

        let outlet = match &self.active_page {
            ActivePage::Processes(view) => view.clone().into_any_element(),
            ActivePage::Performance(view) => view.clone().into_any_element(),
            ActivePage::AppHistory(view) => view.clone().into_any_element(),
            ActivePage::Startup(view) => view.clone().into_any_element(),
            ActivePage::Users(view) => view.clone().into_any_element(),
            ActivePage::Details(view) => view.clone().into_any_element(),
            ActivePage::Services(view) => view.clone().into_any_element(),
            ActivePage::Settings(view) => view.clone().into_any_element(),
        };

        let _ = (muted_fg, border);

        v_flex()
            .id("loom-app-shell-root")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(bg)
            .text_color(fg)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key_down(event, window, cx);
            }))
            .on_modifiers_changed(
                cx.listener(|this, event: &ModifiersChangedEvent, window, cx| {
                    this.handle_modifiers_changed(event, window, cx);
                }),
            )
            .child(title_bar)
            .child(
                h_flex().flex_1().min_h_0().w_full().child(sidebar).child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .overflow_hidden()
                        .child(outlet),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::Arc;

    use gpui_kit::component::TitleBar;
    use gpui_kit::component::sidebar::SidebarToggleButton;
    use gpui_kit::{
        AppContext as _, Capslock, Context, Focusable as _, InteractiveElement as _, IntoElement,
        Modifiers, ModifiersChangedEvent, MouseButton, MouseDownEvent, ParentElement as _,
        PlatformInput, Render, TestAppContext, VisualTestContext, Window, div, point, px,
    };
    use loom_config::{AppSettings, ConfigStore};
    use loom_engine::EngineCoordinator;
    use loom_pal::{NativeBackend, PlatformBackend as _};
    use loom_types::RefreshSpeed;

    use crate::bridge::UiStore;
    use crate::shell::Route;

    use super::{AppShell, route_for_ctrl_digit, should_hold_ctrl_freeze};

    struct TitleBarToggleHarness {
        collapsed: Rc<Cell<bool>>,
    }

    impl Render for TitleBarToggleHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let collapsed = self.collapsed.get();
            let flag = self.collapsed.clone();
            TitleBar::new().child(
                div()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(
                        SidebarToggleButton::new()
                            .collapsed(collapsed)
                            .accessibility_label("Toggle sidebar")
                            .on_click(move |_, _, _| {
                                flag.set(!collapsed);
                            }),
                    ),
            )
        }
    }

    #[test]
    fn ctrl_digit_mapping_and_hold_ctrl_freeze_guards() {
        assert_eq!(route_for_ctrl_digit("1"), Some(Route::Processes));
        assert_eq!(route_for_ctrl_digit("2"), Some(Route::Performance));
        assert_eq!(route_for_ctrl_digit("3"), Some(Route::AppHistory));
        assert_eq!(route_for_ctrl_digit("4"), Some(Route::Startup));
        assert_eq!(route_for_ctrl_digit("5"), Some(Route::Users));
        assert_eq!(route_for_ctrl_digit("6"), Some(Route::Details));
        assert_eq!(route_for_ctrl_digit("7"), Some(Route::Services));
        assert_eq!(route_for_ctrl_digit("8"), Some(Route::Settings));
        assert_eq!(route_for_ctrl_digit("9"), None);

        let ctrl = Modifiers {
            control: true,
            ..Modifiers::default()
        };
        let ctrl_shift = Modifiers {
            control: true,
            shift: true,
            ..Modifiers::default()
        };
        assert!(should_hold_ctrl_freeze(ctrl, false, false));
        assert!(!should_hold_ctrl_freeze(ctrl, true, false));
        assert!(!should_hold_ctrl_freeze(ctrl, false, true));
        assert!(!should_hold_ctrl_freeze(ctrl_shift, false, false));
        assert!(!should_hold_ctrl_freeze(Modifiers::default(), false, false));
    }

    #[gpui_kit::test]
    fn titlebar_sidebar_toggle_stops_mousedown_propagation_and_toggles_on_click(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let collapsed = Rc::new(Cell::new(false));
        let view = cx.new(|_| TitleBarToggleHarness {
            collapsed: collapsed.clone(),
        });
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                cx.new(|cx| gpui_kit::base::Root::new(view.clone(), window, cx))
            })
            .unwrap()
        });
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.update(|window, cx| window.draw(cx).clear(cx));

        // Center of SidebarToggleButton inside TitleBar (left padding 12px + half of 24px = 24px, y = 17px).
        let at = point(px(24.), px(17.));
        let down_result = vcx.update(|window, cx| {
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position: at,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                    first_mouse: false,
                }),
                cx,
            )
        });
        assert!(
            !down_result.propagate,
            "left mouse-down on TitleBar SidebarToggleButton must stop propagation so Windows HTCAPTION does not swallow release"
        );

        vcx.simulate_mouse_up(at, MouseButton::Left, Modifiers::default());
        assert!(
            collapsed.get(),
            "clicking SidebarToggleButton must toggle collapsed state"
        );
    }

    #[gpui_kit::test]
    fn shell_hotkeys_route_navigation_pause_search_esc_and_ctrl_freeze(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().expect("create temp dir");
        let config_store = ConfigStore::new(dir.path().join("settings.toml"));
        let settings = AppSettings {
            refresh_speed: RefreshSpeed::High500Ms,
            ..AppSettings::default()
        };
        let backend = NativeBackend::initialize().expect("init native backend");
        let coordinator =
            Arc::new(EngineCoordinator::new(backend, settings.clone()).expect("start coordinator"));
        let store = cx.new(|cx| UiStore::new(Arc::clone(&coordinator), config_store, settings, cx));

        let mut shell_entity = None;
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                let shell = cx.new(|cx| AppShell::new(store.clone(), Route::Processes, window, cx));
                shell_entity = Some(shell.clone());
                cx.new(|cx| gpui_kit::base::Root::new(shell, window, cx))
            })
            .unwrap()
        });
        let shell = shell_entity.expect("AppShell created");
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.update(|window, cx| window.draw(cx).clear(cx));

        // Ctrl+1..Ctrl+8 switches routes directly.
        for (stroke, expected) in [
            ("ctrl-2", Route::Performance),
            ("ctrl-3", Route::AppHistory),
            ("ctrl-4", Route::Startup),
            ("ctrl-5", Route::Users),
            ("ctrl-6", Route::Details),
            ("ctrl-7", Route::Services),
            ("ctrl-8", Route::Settings),
            ("ctrl-1", Route::Processes),
        ] {
            vcx.simulate_keystrokes(stroke);
            vcx.update(|window, cx| window.draw(cx).clear(cx));
            assert_eq!(
                shell.read_with(&vcx, |s, _| s.current_route()),
                expected,
                "shortcut `{stroke}` must navigate to {expected:?}"
            );
        }

        // `p` toggles pause -> Paused, then `p` again restores High500Ms.
        vcx.simulate_keystrokes("p");
        assert_eq!(
            store.read_with(&vcx, |s, _| s.settings().refresh_speed),
            RefreshSpeed::Paused
        );
        vcx.simulate_keystrokes("p");
        assert_eq!(
            store.read_with(&vcx, |s, _| s.settings().refresh_speed),
            RefreshSpeed::High500Ms
        );

        // `F5` dispatches ForceRefresh cleanly without changing speed or route.
        vcx.simulate_keystrokes("f5");
        assert_eq!(
            shell.read_with(&vcx, |s, _| s.current_route()),
            Route::Processes
        );

        // Hold `Ctrl` activates temp freeze; releasing clears it.
        vcx.update(|window, cx| {
            window.dispatch_event(
                PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                    modifiers: Modifiers {
                        control: true,
                        ..Modifiers::default()
                    },
                    capslock: Capslock::default(),
                }),
                cx,
            );
        });
        assert!(store.read_with(&vcx, |s, _| s.is_temp_frozen()));

        vcx.update(|window, cx| {
            window.dispatch_event(
                PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                    modifiers: Modifiers::default(),
                    capslock: Capslock::default(),
                }),
                cx,
            );
        });
        assert!(!store.read_with(&vcx, |s, _| s.is_temp_frozen()));

        // `/` focuses search input, applying an example populates query, and `Esc` clears it.
        vcx.simulate_keystrokes("/");
        vcx.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert!(
                shell
                    .read(cx)
                    .search_input
                    .focus_handle(cx)
                    .is_focused(window)
            );
            shell.update(cx, |s, cx| s.apply_search_example("pid:1234", window, cx));
            window.draw(cx).clear(cx);
        });
        assert_eq!(
            store.read_with(&vcx, |s, _| s.search_query().to_owned()),
            "pid:1234"
        );

        // Pressing `p` while search input is focused MUST NOT pause refresh.
        vcx.simulate_keystrokes("p");
        assert_eq!(
            store.read_with(&vcx, |s, _| s.settings().refresh_speed),
            RefreshSpeed::High500Ms
        );

        // Pressing `Esc` while search input is focused clears the search query.
        vcx.simulate_keystrokes("escape");
        assert_eq!(
            store.read_with(&vcx, |s, _| s.search_query().to_owned()),
            ""
        );

        coordinator.shutdown();
    }
}
