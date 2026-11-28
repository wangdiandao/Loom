//! Main desktop window shell (`AppShell`) with TitleBar search, structured query syntax help,
//! global keyboard shortcuts, Hold-`Ctrl` temporary freeze, shell tools menu, collapsible Sidebar,
//! route outlet, on-demand elevation notifications, and window visibility/close hooks.

mod chrome;
mod shortcuts;
#[cfg(test)]
mod tests;

use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, ClipboardItem, Context, Entity, FocusHandle, Focusable as _,
    InteractiveElement as _, IntoElement, KeyDownEvent, ModifiersChangedEvent, ParentElement as _,
    Render, Styled as _, Subscription, Task, Window, WindowVisibility, div,
};
use loom_types::{CapabilityFlags, LanguagePreference, ProcessKey, ThemePreference};
use loom_ui_kit::CachedSemanticColors;

use crate::bootstrap;
use crate::bridge::{UiCommands, UiStore, UiStoreEvent, apply_theme, system_accent_rgb};
use crate::features::{
    AppHistoryView, DetailsView, PerformanceView, ProcessesView, ServicesView, SettingsView,
    StartupView, UsersView,
};
use crate::i18n;
use crate::shell::diagnostic_summary::build_diagnostic_summary;
use crate::shell::nav::{NavigationIntent, Route, ShellHandle};

/// Debounce interval for typing in the global TitleBar search box.
const SEARCH_DEBOUNCE_DURATION: Duration = Duration::from_millis(150);

/// Unique notification type tag for deduplicating on-demand elevation prompts.
struct ElevationNotice;

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
    last_system_accent: Option<(u8, u8, u8)>,
    semantic_colors: CachedSemanticColors,
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
        Self::new_with_highlight(store, initial_route, None, None, window, cx)
    }

    /// Constructs a new [`AppShell`] positioned on `initial_route`, optionally highlighting
    /// a [`ProcessKey`] when opening directly into [`Route::Details`].
    pub(crate) fn new_with_highlight(
        store: Entity<UiStore>,
        initial_route: Route,
        initial_details_highlight: Option<ProcessKey>,
        initial_system_accent: Option<(u8, u8, u8)>,
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

        // Follow OS light/dark and accent changes automatically when System preference is active.
        let store_for_appearance = store.clone();
        subscriptions.push(window.observe_window_appearance(move |window, cx| {
            let (theme_pref, accent_pref) = {
                let s = store_for_appearance.read(cx).settings();
                (s.theme, s.accent_color)
            };
            if theme_pref == ThemePreference::System || accent_pref.is_system() {
                apply_theme(theme_pref, accent_pref, Some(window), cx);
            }
        }));

        // Release Hold-Ctrl temporary freeze on focus loss and sync OS accent color on re-focus.
        subscriptions.push(cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                if this.store.read(cx).is_temp_frozen() {
                    UiCommands::set_temp_freeze(&this.store, false, cx);
                    cx.notify();
                }
            } else {
                let (theme_pref, accent_pref) = {
                    let s = this.store.read(cx).settings();
                    (s.theme, s.accent_color)
                };
                if accent_pref.is_system() {
                    let current_sys = system_accent_rgb();
                    if this.last_system_accent != Some(current_sys) {
                        this.last_system_accent = Some(current_sys);
                        apply_theme(theme_pref, accent_pref, Some(window), cx);
                    }
                }
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
            last_system_accent: initial_system_accent,
            semantic_colors: CachedSemanticColors::new(),
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
            Notification::success(i18n::tr("diag_summary.copied_title")),
            cx,
        );
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

    fn render_outlet(&self) -> gpui_kit::AnyElement {
        match &self.active_page {
            ActivePage::Processes(view) => view.clone().into_any_element(),
            ActivePage::Performance(view) => view.clone().into_any_element(),
            ActivePage::AppHistory(view) => view.clone().into_any_element(),
            ActivePage::Startup(view) => view.clone().into_any_element(),
            ActivePage::Users(view) => view.clone().into_any_element(),
            ActivePage::Details(view) => view.clone().into_any_element(),
            ActivePage::Services(view) => view.clone().into_any_element(),
            ActivePage::Settings(view) => view.clone().into_any_element(),
        }
    }
}

impl Render for AppShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (bg, fg) = {
            let theme = cx.theme();
            (theme.background, theme.foreground)
        };
        let title_bar = self.render_title_bar(cx);
        let sidebar = self.render_sidebar(window, cx);
        let outlet = self.render_outlet();

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
                h_flex()
                    .items_stretch()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(sidebar)
                    .child(
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
