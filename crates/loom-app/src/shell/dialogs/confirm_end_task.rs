//! High-risk confirmation dialog before terminating a system or critical process.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, Entity, ParentElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use loom_types::ProcessKey;

use crate::bridge::{UiCommands, UiStore};
use crate::i18n;

/// Opens a high-risk confirmation dialog before terminating a system or critical process.
pub(crate) fn open_confirm_end_task_dialog(
    store: Entity<UiStore>,
    key: ProcessKey,
    name: String,
    is_critical: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let confirmed = Rc::new(Cell::new(false));
    window.open_dialog(cx, move |dialog, _window, cx| {
        let theme = cx.theme();
        let fg = theme.foreground;
        let danger = theme.danger;
        let radius = theme.radius;

        let message = rust_i18n::t!(
            "processes.confirm_end_message",
            name = name.as_str(),
            pid = key.pid
        )
        .into_owned();

        let store_for_ok = store.clone();
        let store_for_confirm = store.clone();
        let confirmed_ok = Rc::clone(&confirmed);
        let confirmed_btn = Rc::clone(&confirmed);

        dialog
            .w(px(460.))
            .centered()
            .title(i18n::tr("processes.confirm_end_title"))
            .child(
                v_flex()
                    .w_full()
                    .gap_3()
                    .child(div().text_sm().text_color(fg).child(message))
                    .when(is_critical, |this| {
                        this.child(
                            h_flex()
                                .w_full()
                                .items_start()
                                .gap_2()
                                .p_3()
                                .rounded(radius)
                                .bg(danger.opacity(0.12))
                                .border_1()
                                .border_color(danger.opacity(0.35))
                                .child(
                                    Icon::new(IconName::TriangleAlert)
                                        .small()
                                        .text_color(danger),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .text_xs()
                                        .font_medium()
                                        .text_color(danger)
                                        .child(i18n::tr("processes.confirm_end_critical")),
                                ),
                        )
                    }),
            )
            .on_ok(move |_, _, cx| {
                if !confirmed_ok.replace(true) {
                    UiCommands::terminate_process(&store_for_ok, key, cx);
                }
                true
            })
            .footer(
                DialogFooter::new()
                    .gap_2()
                    .child(
                        Button::new("confirm-end-task-cancel")
                            .outline()
                            .label(i18n::tr("common.cancel"))
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("confirm-end-task-confirm")
                            .danger()
                            .label(i18n::tr("processes.confirm_end_action"))
                            .on_click(move |_, window, cx| {
                                if !confirmed_btn.replace(true) {
                                    UiCommands::terminate_process(&store_for_confirm, key, cx);
                                }
                                window.close_dialog(cx);
                            }),
                    ),
            )
    });
}
