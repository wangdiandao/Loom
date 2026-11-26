//! Independent vertical UI feature slices for Loom.
//!
//! Feature modules MUST NOT import each other's internals. Cross-page navigation
//! flows exclusively through [`crate::shell::nav::ShellHandle`] and
//! [`crate::shell::nav::NavigationIntent`].

pub(crate) mod details;
pub(crate) mod processes;
pub(crate) mod settings;

use gpui_kit::component::{ActiveTheme as _, Icon, StyledExt as _, h_flex, v_flex};
use gpui_kit::{App, IntoElement, ParentElement as _, Styled as _, div, px};

use crate::i18n;
use crate::shell::Route;

pub(crate) use details::DetailsView;
pub(crate) use processes::ProcessesView;
pub(crate) use settings::SettingsView;

/// Renders a clean, localized milestone notice for P1 views not yet implemented.
///
/// Intentionally renders zero placeholder/fake data and holds zero `MonitorLease`s.
pub(crate) fn render_upcoming_milestone(route: Route, cx: &App) -> impl IntoElement {
    let version = route.milestone_version().unwrap_or("0.0.5");
    let badge = i18n::tr_milestone_badge(version);
    let description = route.milestone_description().unwrap_or_default();
    let notice = i18n::tr("milestone.notice");
    let theme = cx.theme();

    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .p_8()
        .child(
            v_flex()
                .max_w(px(460.))
                .w_full()
                .gap_4()
                .p_6()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .bg(theme.secondary.opacity(0.3))
                .child(
                    h_flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .child(
                            h_flex()
                                .items_center()
                                .gap_2p5()
                                .child(
                                    Icon::new(route.icon())
                                        .size(px(20.))
                                        .text_color(theme.primary),
                                )
                                .child(
                                    div()
                                        .text_lg()
                                        .font_semibold()
                                        .text_color(theme.foreground)
                                        .child(route.label()),
                                ),
                        )
                        .child(
                            div()
                                .px_2p5()
                                .py_1()
                                .rounded(theme.radius)
                                .bg(theme.primary.opacity(0.14))
                                .text_xs()
                                .font_medium()
                                .text_color(theme.primary)
                                .child(badge),
                        ),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child(description),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(notice),
                ),
        )
}
