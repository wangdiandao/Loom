//! Shell-scoped dialogs shared across multiple pages.
//!
//! Feature-specific dialogs (such as `Details`'s `SetAffinityDialog` or wait-chain tree)
//! MUST live inside their own `features/*` slice and mount via `gpui-kit`'s `Root` dialog
//! layer. Only truly cross-page shell dialogs (such as `RunNewTaskDialog` and high-risk
//! process termination confirmation) belong in this module.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, IntoElement, ParentElement as _, Render,
    Styled as _, Subscription, Task, Window, div, prelude::FluentBuilder as _, px,
};
use loom_engine::{CommandEvent, CommandOutcome, CommandSuccess};
use loom_pal::PalError;
use loom_types::{CapabilityFlags, ProcessKey, RunTaskRequest};

use crate::bridge::{UiCommands, UiStore};
use crate::i18n;

/// Maximum number of recent-command pills displayed inside the Run New Task dialog.
const MAX_VISIBLE_HISTORY_PILLS: usize = 8;

/// Formats a path returned by the native file picker for insertion into the command box.
///
/// Paths containing whitespace are wrapped in double quotes so OS command argument
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

        let supports_elevation = self
            .store
            .read(cx)
            .snapshot()
            .capabilities
            .contains(CapabilityFlags::ELEVATION_RELAUNCH);

        self.submitted = true;
        UiCommands::record_run_task_history(&self.store, command.clone(), cx);
        UiCommands::run_new_task(
            &self.store,
            RunTaskRequest {
                command,
                run_as_admin: supports_elevation && self.run_as_admin,
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
        let store_ref = self.store.read(cx);
        let history = store_ref.settings().run_task_history.clone();
        let supports_elevation = store_ref
            .snapshot()
            .capabilities
            .contains(CapabilityFlags::ELEVATION_RELAUNCH);

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
            .when(supports_elevation, |this| {
                this.child(
                    Checkbox::new("run-task-admin")
                        .checked(self.run_as_admin)
                        .label(i18n::tr("run_task.run_as_admin"))
                        .on_click(cx.listener(|this, checked, _, cx| {
                            this.run_as_admin = *checked;
                            cx.notify();
                        })),
                )
            })
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

/// Computes the default memory dump file path (`%TEMP%\<sanitized_stem>_<pid>.DMP`).
pub(crate) fn default_minidump_path(name: &str, pid: u32) -> PathBuf {
    let raw_stem = Path::new(name.trim())
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("process");
    let sanitized: String = raw_stem
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let stem = sanitized.trim_matches(|c| c == '.' || c == '_');
    let final_stem = if stem.is_empty() { "process" } else { stem };
    std::env::temp_dir().join(format!("{final_stem}_{pid}.DMP"))
}

enum MiniDumpState {
    Creating,
    Cancelling,
    Completed(PathBuf),
    Cancelled,
    ElevationRequired,
    Error(String),
}

struct MiniDumpDialogView {
    store: Entity<UiStore>,
    key: ProcessKey,
    name: String,
    dest: PathBuf,
    cancel: Arc<AtomicBool>,
    state: MiniDumpState,
    _dump_task: Option<Task<()>>,
}

impl MiniDumpDialogView {
    fn new(
        store: Entity<UiStore>,
        key: ProcessKey,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let dest = default_minidump_path(&name, key.pid);
        let cancel = Arc::new(AtomicBool::new(false));

        let (state, dump_task) = match UiCommands::write_minidump_async(
            &store,
            key,
            dest.clone(),
            Arc::clone(&cancel),
            cx,
        ) {
            Ok(reply_rx) => {
                let task = cx.spawn_in(window, async move |weak, cx| {
                    let recv = cx
                        .background_executor()
                        .spawn(async move { reply_rx.recv() })
                        .await;
                    let next_state = match recv {
                        Ok(event) => {
                            if event.is_elevation_required() {
                                MiniDumpState::ElevationRequired
                            } else if let Some(CommandSuccess::MiniDumpWritten(path)) =
                                event.outcome.as_ok()
                            {
                                MiniDumpState::Completed(path.clone())
                            } else if let Some(err) = event.error() {
                                if matches!(err, PalError::Cancelled) {
                                    MiniDumpState::Cancelled
                                } else {
                                    MiniDumpState::Error(err.to_string())
                                }
                            } else {
                                MiniDumpState::Error("Unexpected memory dump response".to_owned())
                            }
                        }
                        Err(_) => MiniDumpState::Error("Memory dump channel closed".to_owned()),
                    };

                    let _ = weak.update(cx, |this, cx| {
                        this.state = next_state;
                        cx.notify();
                    });
                });
                (MiniDumpState::Creating, Some(task))
            }
            Err(err) => (MiniDumpState::Error(err.to_string()), None),
        };

        Self {
            store,
            key,
            name,
            dest,
            cancel,
            state,
            _dump_task: dump_task,
        }
    }

    fn request_cancel(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, MiniDumpState::Creating) {
            self.cancel.store(true, Ordering::Relaxed);
            self.state = MiniDumpState::Cancelling;
            cx.notify();
        }
    }
}

impl Drop for MiniDumpDialogView {
    fn drop(&mut self) {
        if matches!(
            self.state,
            MiniDumpState::Creating | MiniDumpState::Cancelling
        ) {
            self.cancel.store(true, Ordering::Relaxed);
        }
    }
}

impl Render for MiniDumpDialogView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let muted_bg = theme.muted;
        let border = theme.border;
        let success = theme.success;
        let warning = theme.warning;
        let danger = theme.danger;
        let radius = theme.radius;
        let mono_font = theme.mono_font_family.clone();

        let subtitle = rust_i18n::t!(
            "minidump.subtitle",
            name = self.name.as_str(),
            pid = self.key.pid
        )
        .into_owned();
        let path_display = self.dest.display().to_string();

        let (status_body, footer) = match &self.state {
            MiniDumpState::Creating | MiniDumpState::Cancelling => {
                let is_cancelling = matches!(self.state, MiniDumpState::Cancelling);
                let label = if is_cancelling {
                    i18n::tr("minidump.cancelling")
                } else {
                    i18n::tr("minidump.creating")
                };
                let body = h_flex()
                    .w_full()
                    .items_center()
                    .gap_2p5()
                    .p_3()
                    .rounded(radius)
                    .bg(muted_bg.opacity(0.6))
                    .border_1()
                    .border_color(border)
                    .child(Spinner::new().small().color(muted_fg))
                    .child(div().text_sm().text_color(fg).child(label))
                    .into_any_element();
                let footer = DialogFooter::new().child(
                    Button::new("minidump-cancel")
                        .outline()
                        .label(i18n::tr("common.cancel"))
                        .disabled(is_cancelling)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.request_cancel(cx);
                        })),
                );
                (body, footer)
            }
            MiniDumpState::Completed(path) => {
                let store = self.store.clone();
                let open_path = path.clone();
                let body = h_flex()
                    .w_full()
                    .items_start()
                    .gap_2p5()
                    .p_3()
                    .rounded(radius)
                    .bg(success.opacity(0.12))
                    .border_1()
                    .border_color(success.opacity(0.35))
                    .child(Icon::new(IconName::CircleCheck).small().text_color(success))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(fg)
                            .child(i18n::tr("minidump.completed")),
                    )
                    .into_any_element();
                let footer = DialogFooter::new()
                    .gap_2()
                    .child(
                        Button::new("minidump-open-location")
                            .outline()
                            .icon(IconName::FolderOpen)
                            .label(i18n::tr("minidump.open_location"))
                            .on_click(move |_, _, cx| {
                                UiCommands::open_file_location(&store, open_path.clone(), cx);
                            }),
                    )
                    .child(
                        Button::new("minidump-done-close")
                            .primary()
                            .label(i18n::tr("common.close"))
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    );
                (body, footer)
            }
            MiniDumpState::Cancelled => {
                let body = h_flex()
                    .w_full()
                    .items_center()
                    .gap_2p5()
                    .p_3()
                    .rounded(radius)
                    .bg(muted_bg.opacity(0.6))
                    .border_1()
                    .border_color(border)
                    .child(Icon::new(IconName::Info).small().text_color(muted_fg))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(muted_fg)
                            .child(i18n::tr("minidump.cancelled")),
                    )
                    .into_any_element();
                let footer = DialogFooter::new().child(
                    Button::new("minidump-cancelled-close")
                        .outline()
                        .label(i18n::tr("common.close"))
                        .on_click(|_, window, cx| {
                            window.close_dialog(cx);
                        }),
                );
                (body, footer)
            }
            MiniDumpState::ElevationRequired => {
                let store = self.store.clone();
                let supports_relaunch = self
                    .store
                    .read(cx)
                    .snapshot()
                    .capabilities
                    .contains(CapabilityFlags::ELEVATION_RELAUNCH);
                let body = h_flex()
                    .w_full()
                    .items_start()
                    .gap_2p5()
                    .p_3()
                    .rounded(radius)
                    .bg(warning.opacity(0.14))
                    .border_1()
                    .border_color(warning.opacity(0.35))
                    .child(
                        Icon::new(IconName::TriangleAlert)
                            .small()
                            .text_color(warning),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .text_color(fg)
                                    .child(i18n::tr("uac.title")),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted_fg)
                                    .child(i18n::tr("uac.message")),
                            ),
                    )
                    .into_any_element();
                let footer = DialogFooter::new()
                    .gap_2()
                    .child(
                        Button::new("minidump-uac-close")
                            .outline()
                            .label(i18n::tr("common.close"))
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .when(supports_relaunch, |f| {
                        f.child(
                            Button::new("minidump-uac-relaunch")
                                .primary()
                                .label(i18n::tr("uac.action"))
                                .on_click(move |_, window, cx| {
                                    UiCommands::relaunch_elevated(&store, "details", cx);
                                    window.close_dialog(cx);
                                }),
                        )
                    });
                (body, footer)
            }
            MiniDumpState::Error(message) => {
                let body = h_flex()
                    .w_full()
                    .items_start()
                    .gap_2p5()
                    .p_3()
                    .rounded(radius)
                    .bg(danger.opacity(0.12))
                    .border_1()
                    .border_color(danger.opacity(0.35))
                    .child(Icon::new(IconName::CircleAlert).small().text_color(danger))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(danger)
                            .child(message.clone()),
                    )
                    .into_any_element();
                let footer = DialogFooter::new().child(
                    Button::new("minidump-error-close")
                        .outline()
                        .label(i18n::tr("common.close"))
                        .on_click(|_, window, cx| {
                            window.close_dialog(cx);
                        }),
                );
                (body, footer)
            }
        };

        v_flex()
            .w_full()
            .gap_3()
            .child(div().text_xs().text_color(muted_fg).child(subtitle))
            .child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .font_medium()
                            .text_color(muted_fg)
                            .child(i18n::tr("minidump.output_path_label")),
                    )
                    .child(
                        div()
                            .w_full()
                            .px_2p5()
                            .py_1p5()
                            .rounded(radius)
                            .border_1()
                            .border_color(border)
                            .bg(muted_bg.opacity(0.35))
                            .text_xs()
                            .font_family(mono_font)
                            .text_color(fg)
                            .truncate()
                            .child(path_display),
                    ),
            )
            .child(status_body)
            .child(footer)
    }
}

/// Opens the shell-scoped **Create memory dump file** progress & cancellation dialog
/// shared by both the Processes and Details views.
pub(crate) fn open_minidump_dialog(
    store: Entity<UiStore>,
    key: ProcessKey,
    name: String,
    window: &mut Window,
    cx: &mut App,
) {
    let dialog_view = cx.new(|cx| MiniDumpDialogView::new(store, key, name, window, cx));
    window.open_dialog(cx, move |dialog, _window, _cx| {
        dialog
            .w(px(520.))
            .centered()
            .title(i18n::tr("minidump.title"))
            .child(dialog_view.clone())
    });
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{default_minidump_path, format_picked_executable_path, normalize_run_task_command};

    #[test]
    fn format_picked_executable_path_quotes_whitespace_only() {
        // POSIX paths
        assert_eq!(
            format_picked_executable_path(Path::new("/usr/bin/loom")),
            "/usr/bin/loom"
        );
        assert_eq!(
            format_picked_executable_path(Path::new("/opt/My Apps/loom")),
            "\"/opt/My Apps/loom\""
        );
        // Windows paths
        assert_eq!(
            format_picked_executable_path(Path::new(r"C:\Tools\loom.exe")),
            r"C:\Tools\loom.exe"
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
            normalize_run_task_command("  /usr/bin/env sh -c true  "),
            Some("/usr/bin/env sh -c true".to_owned())
        );
    }

    #[test]
    fn default_minidump_path_sanitizes_name_and_targets_temp_dir() {
        let p1 = default_minidump_path("notepad.exe", 1234);
        assert_eq!(
            p1.file_name().and_then(|n| n.to_str()),
            Some("notepad_1234.DMP")
        );
        assert_eq!(p1.parent(), Some(std::env::temp_dir().as_path()));

        let p2 = default_minidump_path("My App / v2:test.exe", 99);
        assert_eq!(
            p2.file_name().and_then(|n| n.to_str()),
            Some("v2_test_99.DMP")
        );

        let p2b = default_minidump_path("My App:v2*.exe", 99);
        assert_eq!(
            p2b.file_name().and_then(|n| n.to_str()),
            Some("My_App_v2_99.DMP")
        );

        let p3 = default_minidump_path("   ", 7);
        assert_eq!(
            p3.file_name().and_then(|n| n.to_str()),
            Some("process_7.DMP")
        );
    }
}
