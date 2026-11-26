//! Main desktop window shell (`AppShell`) with TitleBar search, collapsible Sidebar,
//! route outlet, on-demand elevation notifications, and window visibility/close hooks.

use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::sidebar::{
    Sidebar, SidebarFooter, SidebarMenu, SidebarMenuItem, SidebarToggleButton,
};
use gpui_kit::component::{
    ActiveTheme as _, Collapsible as _, Icon, IconName, Selectable as _, Sizable as _,
    StyledExt as _, TitleBar, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _,
    Subscription, Task, Window, WindowVisibility, div, prelude::FluentBuilder as _, px,
};
use loom_types::{CapabilityFlags, LanguagePreference, ProcessKey, ThemePreference};

use crate::bootstrap;
use crate::bridge::{UiCommands, UiStore, UiStoreEvent, apply_theme};
use crate::features::{DetailsView, ProcessesView, SettingsView, render_upcoming_milestone};
use crate::i18n;
use crate::shell::nav::{NavigationIntent, Route, ShellHandle};

/// Debounce interval for typing in the global TitleBar search box.
const SEARCH_DEBOUNCE_DURATION: Duration = Duration::from_millis(150);

/// Unique notification type tag for deduplicating on-demand elevation prompts.
struct ElevationNotice;

/// Mounted page entity owned by [`AppShell`].
///
/// Dropping [`ActivePage::Details`] when switching away immediately releases its
/// RAII [`crate::bridge::ScopedMonitorLease`] for `MonitorDomain::Details`.
enum ActivePage {
    Processes(Entity<ProcessesView>),
    Details(Entity<DetailsView>),
    Settings(Entity<SettingsView>),
    Upcoming(Route),
}

/// Top-level application view wrapped by `gpui-kit`'s `Root` layer.
pub(crate) struct AppShell {
    store: Entity<UiStore>,
    route: Route,
    active_page: ActivePage,
    search_input: Entity<InputState>,
    search_debounce_task: Option<Task<()>>,
    last_locale: LanguagePreference,
    last_elevated: bool,
    _subscriptions: Vec<Subscription>,
}

impl AppShell {
    /// Constructs a new [`AppShell`] positioned on `initial_route`.
    pub(crate) fn new(
        store: Entity<UiStore>,
        initial_route: Route,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial_settings = store.read(cx).settings().clone();
        let initial_elevated = store.read(cx).snapshot().is_elevated;

        window.set_window_title(&i18n::tr("app.window_title"));

        let search_input = cx
            .new(|cx| InputState::new(window, cx).placeholder(i18n::tr("app.search_placeholder")));

        let mut subscriptions = Vec::with_capacity(4);

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
            },
        ));

        // Follow OS light/dark changes automatically when ThemePreference::System is active.
        let store_for_appearance = store.clone();
        subscriptions.push(window.observe_window_appearance(move |window, cx| {
            if store_for_appearance.read(cx).settings().theme == ThemePreference::System {
                apply_theme(ThemePreference::System, Some(window), cx);
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
                    } else {
                        store_for_visibility.update(cx, |store, cx| {
                            store.set_window_dormant(true, cx);
                        });
                    }
                }
                WindowVisibility::Visible => {
                    store_for_visibility.update(cx, |store, cx| {
                        store.set_window_dormant(false, cx);
                    });
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
        let active_page = Self::mount_route(&store, initial_route, None, shell_handle, window, cx);
        bootstrap::record_active_route(initial_route, cx);

        Self {
            store,
            route: initial_route,
            active_page,
            search_input,
            search_debounce_task: None,
            last_locale: initial_settings.language,
            last_elevated: initial_elevated,
            _subscriptions: subscriptions,
        }
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
        shell_handle: ShellHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ActivePage {
        match route {
            Route::Processes => {
                let store = store.clone();
                ActivePage::Processes(cx.new(|cx| ProcessesView::new(store, shell_handle, cx)))
            }
            Route::Details => {
                let store = store.clone();
                ActivePage::Details(cx.new(|cx| DetailsView::new(store, highlight_details, cx)))
            }
            Route::Settings => {
                let store = store.clone();
                ActivePage::Settings(cx.new(|cx| SettingsView::new(store, window, cx)))
            }
            upcoming => ActivePage::Upcoming(upcoming),
        }
    }

    fn navigate_to(
        &mut self,
        route: Route,
        highlight_details: Option<ProcessKey>,
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
            return;
        }

        let shell_handle = Self::make_shell_handle(cx);
        self.route = route;
        self.active_page = Self::mount_route(
            &self.store,
            route,
            highlight_details,
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
                self.navigate_to(route, None, window, cx);
            }
            NavigationIntent::JumpToDetails(key) => {
                self.navigate_to(Route::Details, Some(key), window, cx);
            }
            NavigationIntent::JumpToServices(_pid) => {
                self.navigate_to(Route::Services, None, window, cx);
            }
        }
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store_ref = self.store.read(cx);
        let collapsed = store_ref.settings().sidebar_collapsed;
        let is_elevated = store_ref.snapshot().is_elevated;
        let active_route = self.route;

        let theme = cx.theme();
        let bg = theme.background;
        let fg = theme.foreground;
        let primary = theme.primary;
        let radius = theme.radius;

        let store_toggle = self.store.clone();
        let shell_for_nav = Self::make_shell_handle(cx);
        let shell_for_settings = shell_for_nav.clone();

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
                    h_flex().flex_1().justify_center().px_2().child(
                        div().w(px(340.)).max_w_full().child(
                            Input::new(&self.search_input)
                                .small()
                                .cleanable(true)
                                .prefix(Icon::new(IconName::Search).small()),
                        ),
                    ),
                )
                .child(div().w(px(72.)).flex_shrink_0()),
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

        let settings_label = Route::Settings.label();
        let settings_footer = SidebarFooter::new()
            .selected(active_route == Route::Settings)
            .collapsed(collapsed)
            .p_0()
            .child(
                Button::new("nav-settings")
                    .ghost()
                    .w_full()
                    .h_8()
                    .accessibility_label(settings_label.clone())
                    .when(collapsed, |b| {
                        b.icon(Icon::new(IconName::Settings).small())
                            .tooltip(settings_label.clone())
                    })
                    .when(!collapsed, |b| {
                        b.child(
                            h_flex()
                                .w_full()
                                .items_center()
                                .gap_2()
                                .px_1()
                                .child(Icon::new(IconName::Settings).small())
                                .child(div().text_sm().child(settings_label.clone())),
                        )
                    })
                    .on_click(move |_, window, cx| {
                        shell_for_settings.navigate(Route::Settings, window, cx);
                    }),
            );

        let sidebar = Sidebar::new("loom-sidebar")
            .collapsed(collapsed)
            .collapsible(true)
            .w(px(212.))
            .child(SidebarMenu::new().children(nav_items))
            .footer(settings_footer);

        let outlet = match &self.active_page {
            ActivePage::Processes(view) => view.clone().into_any_element(),
            ActivePage::Details(view) => view.clone().into_any_element(),
            ActivePage::Settings(view) => view.clone().into_any_element(),
            ActivePage::Upcoming(route) => render_upcoming_milestone(*route, cx).into_any_element(),
        };

        v_flex()
            .size_full()
            .bg(bg)
            .text_color(fg)
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
