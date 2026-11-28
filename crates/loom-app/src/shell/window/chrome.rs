//! TitleBar, search syntax help popover, shell tools menu, and collapsible Sidebar rendering.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
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
    Anchor, Context, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _,
    SharedString, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use loom_types::{CapabilityFlags, PlatformKind, RefreshSpeed};

use crate::bridge::UiCommands;
use crate::i18n;
use crate::shell::dialogs::open_file_locks_dialog;
use crate::shell::nav::Route;

use super::AppShell;

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

impl AppShell {
    fn render_search_help_popover(
        &self,
        mono_font: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let weak_for_help = cx.entity().downgrade();
        let weak_for_help_toggle = cx.entity().downgrade();

        Popover::new("search-syntax-help-popover")
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
                        div()
                            .text_sm()
                            .font_semibold()
                            .text_color(fg)
                            .child(i18n::tr("shell_hotkeys.search_help_title")),
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
            })
    }

    fn render_tools_menu(
        &self,
        is_paused: bool,
        supports_file_locks: bool,
        is_windows: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let store_for_tools = self.store.clone();
        let shell_for_tools = Self::make_shell_handle(cx);
        let weak_for_diag = cx.entity().downgrade();

        div()
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
            )
    }

    pub(super) fn render_title_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let store_ref = self.store.read(cx);
        let collapsed = store_ref.settings().sidebar_collapsed;
        let is_paused = store_ref.settings().refresh_speed == RefreshSpeed::Paused;
        let is_temp_frozen = store_ref.is_temp_frozen();
        let snapshot = store_ref.snapshot().clone();
        let is_elevated = snapshot.is_elevated;
        let supports_file_locks = snapshot.capabilities.contains(CapabilityFlags::FILE_LOCKS);
        let is_windows = snapshot.platform.kind == PlatformKind::Windows;

        let (colors, fg, primary, accent_text, radius, mono_font) = {
            let theme = cx.theme();
            let colors = self.semantic_colors.get_copy(theme);
            (
                colors,
                theme.foreground,
                theme.primary,
                theme.list_highlight_foreground,
                theme.radius,
                theme.mono_font_family.clone(),
            )
        };

        let store_toggle = self.store.clone();
        let store_pause_badge = self.store.clone();
        let search_help_popover = self.render_search_help_popover(mono_font, cx);
        let tools_menu = self.render_tools_menu(is_paused, supports_file_locks, is_windows, cx);

        TitleBar::new().child(
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
                                    .text_color(accent_text)
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
        )
    }

    pub(super) fn render_sidebar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let collapsed = self.store.read(cx).settings().sidebar_collapsed;
        let active_route = self.route;
        let shell_for_nav = Self::make_shell_handle(cx);
        let shell_for_settings = shell_for_nav.clone();

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

        Sidebar::new("loom-sidebar")
            .collapsed(collapsed)
            .collapsible(true)
            .w(px(212.))
            .child(SidebarMenu::new().children(nav_items))
            .footer(settings_footer)
    }
}
