//! Confirmation dialogs and OS account actions for the Users feature slice.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex};
use gpui_kit::{App, Entity, ParentElement as _, Styled as _, Window, div, px};
use loom_types::{RunTaskRequest, UserSessionProcess};

use crate::bridge::{UiCommands, UiStore};
use crate::i18n;
use crate::shell::dialogs::open_confirm_end_task_dialog;

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
    let confirmed = Rc::new(Cell::new(false));
    window.open_dialog(cx, move |dialog, _window, cx| {
        let store_for_ok = store.clone();
        let store_confirm = store.clone();
        let confirmed_ok = Rc::clone(&confirmed);
        let confirmed_btn = Rc::clone(&confirmed);
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
            .centered()
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
            .on_ok(move |_, _, cx| {
                if !confirmed_ok.replace(true) {
                    UiCommands::disconnect_session(&store_for_ok, session_id, cx);
                }
                true
            })
            .footer(
                DialogFooter::new()
                    .gap_2()
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
                                if !confirmed_btn.replace(true) {
                                    UiCommands::disconnect_session(&store_confirm, session_id, cx);
                                }
                                window.close_dialog(cx);
                            }),
                    ),
            )
    });
}

/// Terminates a child process directly, or opens the shared critical-process confirmation dialog.
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

    open_confirm_end_task_dialog(
        store.clone(),
        process.key,
        process.name.to_string(),
        true,
        window,
        cx,
    );
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
