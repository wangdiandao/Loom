//! Shell-scoped **Create memory dump file** progress & cancellation dialog.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _,
    Task, Window, div, prelude::FluentBuilder as _, px,
};
use loom_engine::CommandSuccess;
use loom_pal::PalError;
use loom_types::{CapabilityFlags, ProcessKey};

use crate::bootstrap;
use crate::bridge::{UiCommands, UiStore};
use crate::i18n;

/// Computes the default memory dump file path (`%TEMP%\<sanitized_stem>_<pid>.DMP`).
fn default_minidump_path(name: &str, pid: u32) -> PathBuf {
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
    dest: PathBuf,
    cancel: Arc<AtomicBool>,
    state: MiniDumpState,
    _dump_task: Option<Task<()>>,
}

impl MiniDumpDialogView {
    fn new(
        store: Entity<UiStore>,
        key: ProcessKey,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let dest = default_minidump_path(name, key.pid);
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
            dest,
            cancel,
            state,
            _dump_task: dump_task,
        }
    }

    fn request_cancel(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, MiniDumpState::Creating) {
            self.cancel.store(true, Ordering::Release);
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
            self.cancel.store(true, Ordering::Release);
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
                let restore_route = bootstrap::active_route(cx)
                    .map(|r| r.as_str())
                    .unwrap_or("details");
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
                                    UiCommands::relaunch_elevated(&store, restore_route, cx);
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
    let dialog_title = format!(
        "{} — {} (PID {})",
        i18n::tr("minidump.title"),
        name,
        key.pid
    );
    let dialog_view = cx.new(|cx| MiniDumpDialogView::new(store, key, &name, window, cx));
    window.open_dialog(cx, move |dialog, _window, _cx| {
        dialog
            .w(px(520.))
            .centered()
            .title(dialog_title.clone())
            .child(dialog_view.clone())
    });
}

#[cfg(test)]
mod tests {
    use super::default_minidump_path;

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
