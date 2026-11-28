//! Confirmation dialogs and OS account actions for the Users feature slice.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{App, Entity, ParentElement as _, Styled as _, Window, div, px};
use loom_types::{RunTaskRequest, UserSessionProcess};

use crate::bridge::{UiCommands, UiStore};
use crate::i18n;

use super::state::is_critical_or_system_process;

/// Opens a confirmation dialog before disconnecting a user session.
pub(super) fn open_disconnect_session_dialog(
    store: &Entity<UiStore>,
    session_id: u32,
    user_name: String,
    window: &mut Window,
    cx: &mut App,
) {
    let store = store.clone();
    window.open_dialog(cx, move |dialog, _window, cx| {
        let store_confirm = store.clone();
        let muted_fg = cx.theme().muted_foreground;
        let warning = cx.theme().warning;
        let danger = cx.theme().danger;
        let radius = cx.theme().radius;
        let message = rust_i18n::t!(
            "users.confirm_disconnect_message",
            user = user_name.as_str(),
            session_id = session_id
        )
        .into_owned();

        dialog
            .w(px(440.))
            .title(i18n::tr("users.confirm_disconnect_title"))
            .child(
                h_flex()
                    .w_full()
                    .items_start()
                    .gap_2p5()
                    .p_3()
                    .rounded(radius)
                    .bg(warning.opacity(0.12))
                    .border_1()
                    .border_color(warning.opacity(0.35))
                    .child(
                        Icon::new(IconName::TriangleAlert)
                            .small()
                            .text_color(danger),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(muted_fg)
                            .child(message),
                    ),
            )
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("users-confirm-disconnect-cancel")
                            .outline()
                            .label(i18n::tr("common.cancel"))
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("users-confirm-disconnect-ok")
                            .danger()
                            .label(i18n::tr("users.confirm_disconnect_action"))
                            .on_click(move |_, window, cx| {
                                UiCommands::disconnect_session(&store_confirm, session_id, cx);
                                window.close_dialog(cx);
                            }),
                    ),
            )
    });
}

/// Terminates a child process directly, or opens a confirmation dialog for critical processes.
pub(super) fn request_end_user_process(
    store: &Entity<UiStore>,
    process: &UserSessionProcess,
    window: &mut Window,
    cx: &mut App,
) {
    if !is_critical_or_system_process(process.status) {
        UiCommands::terminate_process(store, process.key, cx);
        return;
    }

    let store = store.clone();
    let key = process.key;
    let name = process.name.to_string();
    window.open_dialog(cx, move |dialog, _window, cx| {
        let store_confirm = store.clone();
        let warning = cx.theme().warning;
        let danger = cx.theme().danger;
        let muted_fg = cx.theme().muted_foreground;
        let radius = cx.theme().radius;
        let message = rust_i18n::t!(
            "processes.confirm_end_message",
            name = name.as_str(),
            pid = key.pid
        )
        .into_owned();

        dialog
            .w(px(440.))
            .title(i18n::tr("processes.confirm_end_title"))
            .child(
                v_flex()
                    .w_full()
                    .gap_3()
                    .child(div().text_sm().text_color(muted_fg).child(message))
                    .child(
                        h_flex()
                            .w_full()
                            .items_start()
                            .gap_2()
                            .p_2p5()
                            .rounded(radius)
                            .bg(warning.opacity(0.14))
                            .border_1()
                            .border_color(warning.opacity(0.4))
                            .child(
                                Icon::new(IconName::TriangleAlert)
                                    .small()
                                    .text_color(danger),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_xs()
                                    .text_color(danger)
                                    .child(i18n::tr("processes.confirm_end_critical")),
                            ),
                    ),
            )
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("users-confirm-end-cancel")
                            .outline()
                            .label(i18n::tr("common.cancel"))
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("users-confirm-end-ok")
                            .danger()
                            .label(i18n::tr("processes.confirm_end_action"))
                            .on_click(move |_, window, cx| {
                                UiCommands::terminate_process(&store_confirm, key, cx);
                                window.close_dialog(cx);
                            }),
                    ),
            )
    });
}

/// Launches the native Windows user accounts management applet (`netplwiz`).
pub(super) fn launch_manage_user_accounts(store: &Entity<UiStore>, cx: &mut App) {
    UiCommands::run_new_task(
        store,
        RunTaskRequest {
            command: "netplwiz".to_owned(),
            run_as_admin: false,
        },
        cx,
    );
}
