//! Shell-scoped dialogs shared across multiple pages.
//!
//! Feature-specific dialogs (such as `Details`'s `SetAffinityDialog` or wait-chain tree)
//! MUST live inside their own `features/*` slice and mount via `gpui-kit`'s `Root` dialog
//! layer. Only truly cross-page shell dialogs (such as `RunNewTaskDialog` and high-risk
//! process termination confirmation) belong in this module.

use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, IntoElement, ParentElement as _, Render,
    Styled as _, Subscription, Task, Window, div, prelude::FluentBuilder as _, px,
};
use loom_engine::{CommandEvent, CommandOutcome, CommandSuccess};
use loom_types::{ProcessKey, RunTaskRequest};

use crate::bridge::{UiCommands, UiStore};
use crate::i18n;

/// Maximum number of recent-command pills displayed inside the Run New Task dialog.
const MAX_VISIBLE_HISTORY_PILLS: usize = 8;

/// Formats a path returned by the native file picker for insertion into the command box.
///
/// Paths containing whitespace are wrapped in double quotes so `ShellExecuteExW` argument
/// splitting keeps the executable path intact.
fn format_picked_executable_path(path: &Path) -> String {
    let raw = path.display().to_string();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.chars().any(char::is_whitespace)
        && !(trimmed.starts_with('"') && trimmed.ends_with('"'))
    {
        format!("\"{trimmed}\"")
    } else {
        trimmed.to_owned()
    }
}

/// Normalizes and validates a command entered in the Run New Task dialog.
fn normalize_run_task_command(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// Stateful form body mounted inside the shell `Run new task` modal dialog.
struct RunNewTaskForm {
    store: Entity<UiStore>,
    input: Entity<InputState>,
    run_as_admin: bool,
    submitted: bool,
    validation_error: Option<String>,
    browsing: bool,
    browse_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl RunNewTaskForm {
    fn new(store: Entity<UiStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(i18n::tr("run_task.placeholder")));

        let input_sub = cx.subscribe_in(
            &input,
            window,
            |this, _state, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    if this.validation_error.is_some() {
                        this.validation_error = None;
                        cx.notify();
                    }
                }
                InputEvent::PressEnter { .. } => {
                    this.submit(window, cx);
                }
                InputEvent::Focus | InputEvent::Blur => {}
            },
        );

        Self {
            store,
            input,
            run_as_admin: false,
            submitted: false,
            validation_error: None,
            browsing: false,
            browse_task: None,
            _subscriptions: vec![input_sub],
        }
    }

    fn focus_input(&self, window: &mut Window, cx: &mut App) {
        self.input.focus_handle(cx).focus(window, cx);
    }

    fn set_command_text(&mut self, command: String, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |state, cx| {
            state.set_value(command, window, cx);
        });
        self.validation_error = None;
        self.focus_input(window, cx);
        cx.notify();
    }

    fn start_browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.browsing {
            return;
        }

        let Ok(rx) = UiCommands::browse_executable_async(&self.store, cx) else {
            return;
        };

        self.browsing = true;
        cx.notify();

        self.browse_task = Some(cx.spawn_in(window, async move |weak, cx| {
            let received: Option<CommandEvent> = cx
                .background_executor()
                .spawn(async move { rx.recv().ok() })
                .await;

            let _ = weak.update_in(cx, |this, window, cx| {
                this.browsing = false;
                this.browse_task = None;

                if let Some(CommandEvent {
                    outcome: CommandOutcome::Ok(CommandSuccess::FilePicked(Some(path))),
                    ..
                }) = received
                {
                    let formatted = format_picked_executable_path(&path);
                    if !formatted.is_empty() {
                        this.set_command_text(formatted, window, cx);
                        return;
                    }
                }
                cx.notify();
            });
        }));
    }

    fn try_submit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.submitted {
            return true;
        }
        let raw = self.input.read(cx).value();
        let Some(command) = normalize_run_task_command(raw.as_ref()) else {
            self.validation_error = Some(i18n::tr("run_task.empty_error"));
            self.focus_input(window, cx);
            cx.notify();
            return false;
        };

        self.submitted = true;
        UiCommands::record_run_task_history(&self.store, command.clone(), cx);
        UiCommands::run_new_task(
            &self.store,
            RunTaskRequest {
                command,
                run_as_admin: self.run_as_admin,
            },
            cx,
        );
        true
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.try_submit(window, cx) {
            window.close_dialog(cx);
        }
    }
}

impl Render for RunNewTaskForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let history = self.store.read(cx).settings().run_task_history.clone();

        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let danger = theme.danger;
        let border = theme.border;
        let radius = theme.radius;

        v_flex()
            .w_full()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(muted_fg)
                    .child(i18n::tr("run_task.description")),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap_1p5()
                    .child(
                        div()
                            .text_xs()
                            .font_medium()
                            .text_color(fg)
                            .child(i18n::tr("run_task.open_label")),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(Input::new(&self.input).cleanable(true)),
                            )
                            .child(
                                Button::new("run-task-browse")
                                    .outline()
                                    .icon(IconName::FolderOpen)
                                    .label(i18n::tr("run_task.browse"))
                                    .disabled(self.browsing)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.start_browse(window, cx);
                                    })),
                            ),
                    )
                    .when_some(self.validation_error.clone(), |this, err| {
                        this.child(
                            h_flex()
                                .items_center()
                                .gap_1p5()
                                .text_xs()
                                .text_color(danger)
                                .child(Icon::new(IconName::CircleAlert).xsmall().text_color(danger))
                                .child(err),
                        )
                    }),
            )
            .child(
                Checkbox::new("run-task-admin")
                    .checked(self.run_as_admin)
                    .label(i18n::tr("run_task.run_as_admin"))
                    .on_click(cx.listener(|this, checked, _, cx| {
                        this.run_as_admin = *checked;
                        cx.notify();
                    })),
            )
            .when(!history.is_empty(), |this| {
                this.child(
                    v_flex()
                        .w_full()
                        .gap_1p5()
                        .pt_2()
                        .border_t_1()
                        .border_color(border.opacity(0.6))
                        .child(
                            div()
                                .text_xs()
                                .font_medium()
                                .text_color(muted_fg)
                                .child(i18n::tr("run_task.recent_commands")),
                        )
                        .child(
                            h_flex().w_full().flex_wrap().gap_1p5().children(
                                history
                                    .into_iter()
                                    .take(MAX_VISIBLE_HISTORY_PILLS)
                                    .enumerate()
                                    .map(|(ix, cmd)| {
                                        let cmd_for_click = cmd.clone();
                                        Button::new(("run-task-history-pill", ix))
                                            .xsmall()
                                            .outline()
                                            .rounded(radius)
                                            .max_w(px(220.))
                                            .label(cmd)
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.set_command_text(
                                                    cmd_for_click.clone(),
                                                    window,
                                                    cx,
                                                );
                                            }))
                                    }),
                            ),
                        ),
                )
            })
    }
}

/// Opens the shell-scoped **Run new task** modal dialog on `gpui-kit`'s `Root` dialog layer.
pub(crate) fn open_run_new_task_dialog(store: Entity<UiStore>, window: &mut Window, cx: &mut App) {
    let form = cx.new(|cx| RunNewTaskForm::new(store, window, cx));
    let form_for_dialog = form.clone();
    let form_for_focus = form.clone();

    window.open_dialog(cx, move |dialog, _window, _cx| {
        let form_for_ok = form_for_dialog.clone();
        let form_for_run = form_for_dialog.clone();

        dialog
            .w(px(500.))
            .centered()
            .title(i18n::tr("run_task.title"))
            .child(form_for_dialog.clone())
            .on_ok(move |_, window, cx| form_for_ok.update(cx, |f, cx| f.try_submit(window, cx)))
            .footer(
                DialogFooter::new()
                    .gap_2()
                    .child(
                        Button::new("run-task-cancel")
                            .outline()
                            .label(i18n::tr("common.cancel"))
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("run-task-submit")
                            .primary()
                            .icon(IconName::Play)
                            .label(i18n::tr("run_task.run"))
                            .on_click(move |_, window, cx| {
                                form_for_run.update(cx, |f, cx| f.submit(window, cx));
                            }),
                    ),
            )
    });

    // Focus the command Input after `open_dialog` finishes claiming initial dialog focus.
    window.defer(cx, move |window, cx| {
        let input = form_for_focus.read(cx).input.clone();
        input.focus_handle(cx).focus(window, cx);
    });
}

/// Opens a high-risk confirmation dialog before terminating a Windows or critical process.
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{format_picked_executable_path, normalize_run_task_command};

    #[test]
    fn format_picked_executable_path_quotes_whitespace_only() {
        assert_eq!(
            format_picked_executable_path(Path::new(r"C:\Windows\System32\notepad.exe")),
            r"C:\Windows\System32\notepad.exe"
        );
        assert_eq!(
            format_picked_executable_path(Path::new(r"C:\Program Files\Loom\loom.exe")),
            r#""C:\Program Files\Loom\loom.exe""#
        );
        assert_eq!(format_picked_executable_path(Path::new("")), "");
    }

    #[test]
    fn normalize_run_task_command_rejects_blank_input() {
        assert_eq!(normalize_run_task_command(""), None);
        assert_eq!(normalize_run_task_command("   \t\n "), None);
        assert_eq!(
            normalize_run_task_command("  wt.exe -p PowerShell  "),
            Some("wt.exe -p PowerShell".to_owned())
        );
    }
}
