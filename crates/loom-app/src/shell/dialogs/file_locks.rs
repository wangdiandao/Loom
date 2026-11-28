//! Shell-scoped **Find file locks** (Restart Manager) modal dialog.

use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Subscription, Task,
    Window, div, prelude::FluentBuilder as _, px,
};
use loom_engine::{CommandEvent, CommandOutcome, CommandSuccess};
use loom_types::{AppSnapshot, CapabilityFlags, FileLockHolder, ProcessKey, ProcessRowKind};

use crate::bootstrap;
use crate::bridge::{UiCommands, UiStore};
use crate::i18n;
use crate::shell::ShellHandle;

use super::validation_error_row;

/// Normalizes a raw file/folder path entered in the File-Lock Finder dialog,
/// trimming whitespace and stripping surrounding matching double quotes.
fn normalize_file_lock_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .map(str::trim)
        .unwrap_or(trimmed);
    if unquoted.is_empty() {
        None
    } else {
        Some(PathBuf::from(unquoted))
    }
}

/// Returns `true` when `holder` identifies Windows Explorer (`explorer.exe` / `RmExplorer`).
fn is_explorer_lock_holder(holder: &FileLockHolder) -> bool {
    if holder.app_type.eq_ignore_ascii_case("Explorer") {
        return true;
    }
    let lower = holder.app_name.to_ascii_lowercase();
    lower.contains("explorer.exe") || lower.contains("windows explorer")
}

/// Resolves the live [`ProcessKey`] matching `pid` from `snapshot` so jumping to Details
/// preserves creation-time identity verification whenever available.
fn resolve_process_key_for_pid(snapshot: &AppSnapshot, pid: u32) -> ProcessKey {
    if let Some(row) = snapshot.details.rows.iter().find(|r| r.key.pid == pid) {
        return row.key;
    }
    if let Some(row) = snapshot
        .processes
        .rows
        .iter()
        .find(|r| r.kind == ProcessRowKind::Process && r.key.pid == pid)
    {
        return row.key;
    }
    ProcessKey::new(pid, 0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FileLocksState {
    Idle,
    Searching,
    Results(Vec<FileLockHolder>),
    ElevationRequired,
    Error(String),
}

struct FileLocksDialogView {
    store: Entity<UiStore>,
    shell: ShellHandle,
    path_input: Entity<InputState>,
    state: FileLocksState,
    validation_error: Option<String>,
    browsing: bool,
    browse_task: Option<Task<()>>,
    query_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl FileLocksDialogView {
    fn new(
        store: Entity<UiStore>,
        shell: ShellHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let path_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(i18n::tr("file_locks.path_placeholder"))
        });

        let input_sub = cx.subscribe_in(
            &path_input,
            window,
            |this, _state, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    if this.validation_error.is_some() {
                        this.validation_error = None;
                        cx.notify();
                    }
                }
                InputEvent::PressEnter { .. } => {
                    this.start_search(window, cx);
                }
                InputEvent::Focus | InputEvent::Blur => {}
            },
        );

        Self {
            store,
            shell,
            path_input,
            state: FileLocksState::Idle,
            validation_error: None,
            browsing: false,
            browse_task: None,
            query_task: None,
            _subscriptions: vec![input_sub],
        }
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
                    let display = path.display().to_string();
                    this.path_input.update(cx, |state, cx| {
                        state.set_value(display, window, cx);
                    });
                    this.validation_error = None;
                    this.start_search(window, cx);
                    return;
                }
                cx.notify();
            });
        }));
    }

    fn start_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.path_input.read(cx).value();
        let Some(path) = normalize_file_lock_path(raw.as_ref()) else {
            self.validation_error = Some(i18n::tr("file_locks.empty_path_error"));
            self.path_input.focus_handle(cx).focus(window, cx);
            cx.notify();
            return;
        };

        self.validation_error = None;
        let rx = match UiCommands::find_file_locks(&self.store, path, cx) {
            Ok(rx) => rx,
            Err(err) => {
                self.state = FileLocksState::Error(err.to_string());
                cx.notify();
                return;
            }
        };

        self.state = FileLocksState::Searching;
        cx.notify();

        self.query_task = Some(cx.spawn_in(window, async move |weak, cx| {
            let recv = cx
                .background_executor()
                .spawn(async move { rx.recv() })
                .await;

            let next_state = match recv {
                Ok(event) => {
                    if event.is_elevation_required() {
                        FileLocksState::ElevationRequired
                    } else if let Some(CommandSuccess::FileLocks(holders)) = event.outcome.as_ok() {
                        FileLocksState::Results(holders.clone())
                    } else if let Some(err) = event.error() {
                        FileLocksState::Error(err.to_string())
                    } else {
                        FileLocksState::Error("Unexpected file-lock query response".to_owned())
                    }
                }
                Err(_) => FileLocksState::Error("File-lock query channel closed".to_owned()),
            };

            let _ = weak.update(cx, |this, cx| {
                this.query_task = None;
                this.state = next_state;
                cx.notify();
            });
        }));
    }

    #[cfg(test)]
    fn set_state_for_test(&mut self, state: FileLocksState, cx: &mut Context<Self>) {
        self.state = state;
        cx.notify();
    }
}

impl Render for FileLocksDialogView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let muted_bg = theme.muted;
        let border = theme.border;
        let primary = theme.primary;
        let accent_text = theme.list_highlight_foreground;
        let success = theme.success;
        let warning = theme.warning;
        let danger = theme.danger;
        let radius = theme.radius;
        let mono_font = theme.mono_font_family.clone();

        let is_searching = matches!(self.state, FileLocksState::Searching);

        let body = match &self.state {
            FileLocksState::Idle => None,
            FileLocksState::Searching => Some(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2p5()
                    .p_3()
                    .rounded(radius)
                    .bg(muted_bg.opacity(0.55))
                    .border_1()
                    .border_color(border)
                    .child(Spinner::new().small().color(muted_fg))
                    .child(
                        div()
                            .text_sm()
                            .text_color(fg)
                            .child(i18n::tr("file_locks.searching")),
                    )
                    .into_any_element(),
            ),
            FileLocksState::ElevationRequired => {
                let store = self.store.clone();
                let supports_relaunch = store
                    .read(cx)
                    .snapshot()
                    .capabilities
                    .contains(CapabilityFlags::ELEVATION_RELAUNCH);
                let restore_route = bootstrap::active_route(cx)
                    .map(|r| r.as_str())
                    .unwrap_or("processes");
                Some(
                    h_flex()
                        .w_full()
                        .items_start()
                        .justify_between()
                        .gap_2p5()
                        .p_3()
                        .rounded(radius)
                        .bg(warning.opacity(0.14))
                        .border_1()
                        .border_color(warning.opacity(0.35))
                        .child(
                            h_flex()
                                .flex_1()
                                .min_w_0()
                                .items_start()
                                .gap_2()
                                .child(
                                    Icon::new(IconName::TriangleAlert)
                                        .small()
                                        .text_color(warning),
                                )
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .gap_0p5()
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
                                ),
                        )
                        .when(supports_relaunch, |this| {
                            this.child(
                                Button::new("file-locks-relaunch-admin")
                                    .xsmall()
                                    .primary()
                                    .label(i18n::tr("uac.action"))
                                    .on_click(move |_, window, cx| {
                                        UiCommands::relaunch_elevated(&store, restore_route, cx);
                                        window.close_dialog(cx);
                                    }),
                            )
                        })
                        .into_any_element(),
                )
            }
            FileLocksState::Error(message) => Some(
                h_flex()
                    .w_full()
                    .items_start()
                    .gap_2()
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
                    .into_any_element(),
            ),
            FileLocksState::Results(holders) if holders.is_empty() => Some(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .p_3()
                    .rounded(radius)
                    .bg(success.opacity(0.12))
                    .border_1()
                    .border_color(success.opacity(0.35))
                    .child(Icon::new(IconName::CircleCheck).small().text_color(success))
                    .child(
                        div()
                            .text_sm()
                            .text_color(fg)
                            .child(i18n::tr("file_locks.no_locks")),
                    )
                    .into_any_element(),
            ),
            FileLocksState::Results(holders) => {
                let has_explorer = holders.iter().any(is_explorer_lock_holder);
                let store_for_explorer = self.store.clone();
                let snapshot = self.store.read(cx).snapshot().clone();

                Some(
                    v_flex()
                        .w_full()
                        .gap_2()
                        .when(has_explorer, |this| {
                            this.child(
                                h_flex()
                                    .w_full()
                                    .items_center()
                                    .justify_between()
                                    .gap_2()
                                    .px_3()
                                    .py_2()
                                    .rounded(radius)
                                    .bg(warning.opacity(0.14))
                                    .border_1()
                                    .border_color(warning.opacity(0.35))
                                    .child(
                                        h_flex()
                                            .flex_1()
                                            .min_w_0()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                Icon::new(IconName::TriangleAlert)
                                                    .small()
                                                    .text_color(warning),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .text_xs()
                                                    .font_medium()
                                                    .text_color(fg)
                                                    .child(i18n::tr("file_locks.explorer_banner")),
                                            ),
                                    )
                                    .child(
                                        Button::new("file-locks-restart-explorer")
                                            .xsmall()
                                            .warning()
                                            .icon(IconName::RefreshCw)
                                            .label(i18n::tr("file_locks.restart_explorer"))
                                            .on_click(move |_, window, cx| {
                                                UiCommands::restart_explorer(
                                                    &store_for_explorer,
                                                    cx,
                                                );
                                                window.push_notification(
                                                    Notification::info(i18n::tr(
                                                        "shell_hotkeys.explorer_restarting",
                                                    )),
                                                    cx,
                                                );
                                            }),
                                    ),
                            )
                        })
                        .child(
                            v_flex()
                                .id("file-locks-holders-list")
                                .w_full()
                                .max_h(px(240.))
                                .overflow_y_scroll()
                                .gap_1p5()
                                .children(holders.iter().enumerate().map(|(ix, holder)| {
                                    let key = resolve_process_key_for_pid(&snapshot, holder.pid);
                                    let shell = self.shell.clone();
                                    let stable_id = ((holder.pid as usize) << 16) | (ix & 0xFFFF);
                                    let display_name = if holder.app_name.trim().is_empty() {
                                        rust_i18n::t!("file_locks.pid_label", pid = holder.pid)
                                            .into_owned()
                                    } else {
                                        holder.app_name.to_string()
                                    };
                                    let pid_badge =
                                        rust_i18n::t!("file_locks.pid_label", pid = holder.pid)
                                            .into_owned();
                                    let service_name = holder.service_name.clone();
                                    let restartable = holder.restartable;

                                    h_flex()
                                        .w_full()
                                        .items_center()
                                        .justify_between()
                                        .gap_2()
                                        .px_2p5()
                                        .py_2()
                                        .rounded(radius)
                                        .bg(muted_bg.opacity(0.35))
                                        .border_1()
                                        .border_color(border.opacity(0.6))
                                        .child(
                                            v_flex()
                                                .flex_1()
                                                .min_w_0()
                                                .gap_1()
                                                .child(
                                                    h_flex()
                                                        .items_center()
                                                        .gap_1p5()
                                                        .flex_wrap()
                                                        .child(
                                                            div()
                                                                .text_sm()
                                                                .font_medium()
                                                                .text_color(fg)
                                                                .truncate()
                                                                .child(display_name),
                                                        )
                                                        .child(
                                                            div()
                                                                .px_1p5()
                                                                .py_0p5()
                                                                .rounded(radius)
                                                                .bg(muted_bg)
                                                                .text_xs()
                                                                .font_family(mono_font.clone())
                                                                .text_color(muted_fg)
                                                                .child(pid_badge),
                                                        ),
                                                )
                                                .when(
                                                    service_name.is_some() || restartable,
                                                    |this| {
                                                        this.child(
                                                            h_flex()
                                                                .items_center()
                                                                .gap_1p5()
                                                                .flex_wrap()
                                                                .when_some(
                                                                    service_name,
                                                                    |this, svc| {
                                                                        this.child(
                                                                            div()
                                                                                .px_1p5()
                                                                                .py_0p5()
                                                                                .rounded(radius)
                                                                                .bg(primary
                                                                                    .opacity(0.14))
                                                                                .text_xs()
                                                                                .text_color(
                                                                                    accent_text,
                                                                                )
                                                                                .child(
                                                                                    svc.to_string(),
                                                                                ),
                                                                        )
                                                                    },
                                                                )
                                                                .when(restartable, |this| {
                                                                    this.child(
                                                                        div()
                                                                            .px_1p5()
                                                                            .py_0p5()
                                                                            .rounded(radius)
                                                                            .bg(success
                                                                                .opacity(0.14))
                                                                            .text_xs()
                                                                            .text_color(success)
                                                                            .child(i18n::tr(
                                                                                "file_locks.restartable_badge",
                                                                            )),
                                                                    )
                                                                }),
                                                        )
                                                    },
                                                ),
                                        )
                                        .child(
                                            Button::new(("file-locks-jump-details", stable_id))
                                                .xsmall()
                                                .outline()
                                                .label(i18n::tr("file_locks.go_to_details"))
                                                .on_click(move |_, window, cx| {
                                                    window.close_dialog(cx);
                                                    shell.jump_to_details(key, window, cx);
                                                }),
                                        )
                                })),
                        )
                        .into_any_element(),
                )
            }
        };

        v_flex()
            .w_full()
            .gap_3()
            .child(
                v_flex()
                    .w_full()
                    .gap_1p5()
                    .child(
                        div()
                            .text_xs()
                            .font_medium()
                            .text_color(fg)
                            .child(i18n::tr("file_locks.path_label")),
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
                                    .child(Input::new(&self.path_input).cleanable(true)),
                            )
                            .child(
                                Button::new("file-locks-browse")
                                    .outline()
                                    .icon(IconName::FolderOpen)
                                    .label(i18n::tr("file_locks.browse"))
                                    .disabled(self.browsing || is_searching)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.start_browse(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("file-locks-search")
                                    .primary()
                                    .icon(IconName::Search)
                                    .label(i18n::tr("file_locks.search"))
                                    .disabled(is_searching)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.start_search(window, cx);
                                    })),
                            ),
                    )
                    .when_some(self.validation_error.clone(), |this, err| {
                        this.child(validation_error_row(err, danger))
                    }),
            )
            .when_some(body, |this, el| this.child(el))
            .child(
                DialogFooter::new().child(
                    Button::new("file-locks-close")
                        .outline()
                        .label(i18n::tr("common.close"))
                        .on_click(|_, window, cx| {
                            window.close_dialog(cx);
                        }),
                ),
            )
    }
}

/// Opens the shell-scoped **Find file locks** (Restart Manager) modal dialog.
pub(crate) fn open_file_locks_dialog(
    store: Entity<UiStore>,
    shell: ShellHandle,
    window: &mut Window,
    cx: &mut App,
) {
    let view = cx.new(|cx| FileLocksDialogView::new(store, shell, window, cx));
    let view_for_dialog = view.clone();
    let view_for_focus = view.clone();

    window.open_dialog(cx, move |dialog, _window, _cx| {
        dialog
            .w(px(580.))
            .centered()
            .title(i18n::tr("file_locks.title"))
            .child(view_for_dialog.clone())
    });

    window.defer(cx, move |window, cx| {
        let input = view_for_focus.read(cx).path_input.clone();
        input.focus_handle(cx).focus(window, cx);
    });
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use gpui_kit::{AppContext as _, TestAppContext, VisualTestContext};
    use loom_config::{AppSettings, ConfigStore};
    use loom_engine::EngineCoordinator;
    use loom_pal::{NativeBackend, PlatformBackend as _};
    use loom_types::{
        AppSnapshot, FileLockHolder, ProcessKey, ProcessRowKind, ProcessRowProjection,
        ProcessTableSlice, RefreshSpeed,
    };

    use crate::bridge::UiStore;
    use crate::shell::ShellHandle;

    use super::{
        FileLocksDialogView, FileLocksState, is_explorer_lock_holder, normalize_file_lock_path,
        resolve_process_key_for_pid,
    };

    #[test]
    fn file_lock_helpers_normalize_paths_detect_explorer_and_resolve_keys() {
        assert_eq!(normalize_file_lock_path("   "), None);
        assert_eq!(normalize_file_lock_path("\"\""), None);
        assert_eq!(
            normalize_file_lock_path(r#"  "C:\Data\My File.txt"  "#),
            Some(PathBuf::from(r"C:\Data\My File.txt"))
        );

        let explorer_by_type = FileLockHolder {
            pid: 4200,
            app_name: Arc::from("Desktop Shell"),
            service_name: None,
            app_type: Arc::from("Explorer"),
            restartable: true,
        };
        assert!(is_explorer_lock_holder(&explorer_by_type));

        let explorer_by_name = FileLockHolder {
            pid: 4200,
            app_name: Arc::from("Windows Explorer"),
            service_name: None,
            app_type: Arc::from("MainWindow"),
            restartable: true,
        };
        assert!(is_explorer_lock_holder(&explorer_by_name));

        let notepad = FileLockHolder {
            pid: 9001,
            app_name: Arc::from("Notepad"),
            service_name: None,
            app_type: Arc::from("MainWindow"),
            restartable: true,
        };
        assert!(!is_explorer_lock_holder(&notepad));

        let key = ProcessKey::new(9001, 555_000);
        let snapshot = AppSnapshot {
            processes: Arc::new(ProcessTableSlice {
                rows: vec![ProcessRowProjection {
                    kind: ProcessRowKind::Process,
                    key,
                    depth: 0,
                    has_children: false,
                    is_expanded: false,
                    child_count: 0,
                    name: Arc::from("notepad.exe"),
                    display_name: Arc::from("Notepad"),
                    window_title: None,
                    publisher: None,
                    exe_path: None,
                    command_line: None,
                    icon: None,
                    category: Default::default(),
                    status: Default::default(),
                    cpu_permille: 0,
                    memory_bytes: 0,
                    memory_permille: 0,
                    disk_bytes_per_sec: 0,
                    disk_permille: 0,
                    net_bytes_per_sec: 0,
                    net_permille: 0,
                    gpu_util_permille: 0,
                    gpu_dedicated_bytes: 0,
                    parent_key: None,
                    leak_warning: None,
                    webview2_role: None,
                }],
                ..ProcessTableSlice::default()
            }),
            ..AppSnapshot::default()
        };
        assert_eq!(resolve_process_key_for_pid(&snapshot, 9001), key);
        assert_eq!(
            resolve_process_key_for_pid(&snapshot, 1234),
            ProcessKey::new(1234, 0)
        );
    }

    #[gpui_kit::test]
    fn file_locks_dialog_validates_empty_path_and_renders_result_states(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dir = tempfile::tempdir().expect("create temp dir");
        let config_store = ConfigStore::new(dir.path().join("settings.toml"));
        let settings = AppSettings {
            refresh_speed: RefreshSpeed::Paused,
            ..AppSettings::default()
        };
        let backend = NativeBackend::initialize().expect("init native backend");
        let coordinator =
            Arc::new(EngineCoordinator::new(backend, settings.clone()).expect("start coordinator"));
        let store = cx.new(|cx| UiStore::new(Arc::clone(&coordinator), config_store, settings, cx));
        let shell = ShellHandle::new(|_, _, _| {});

        let mut dialog_entity = None;
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                let view =
                    cx.new(|cx| FileLocksDialogView::new(store.clone(), shell.clone(), window, cx));
                dialog_entity = Some(view.clone());
                cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
            })
            .unwrap()
        });
        let dialog_view = dialog_entity.expect("dialog view built");
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.update(|window, cx| window.draw(cx).clear(cx));

        vcx.update(|window, cx| {
            dialog_view.update(cx, |view, cx| view.start_search(window, cx));
            window.draw(cx).clear(cx);
        });
        dialog_view.read_with(&vcx, |view, _| {
            assert!(view.validation_error.is_some());
            assert_eq!(view.state, FileLocksState::Idle);
        });

        vcx.update(|window, cx| {
            dialog_view.update(cx, |view, cx| {
                view.set_state_for_test(
                    FileLocksState::Results(vec![FileLockHolder {
                        pid: 4200,
                        app_name: Arc::from("Windows Explorer"),
                        service_name: Some(Arc::from("ShellSvc")),
                        app_type: Arc::from("Explorer"),
                        restartable: true,
                    }]),
                    cx,
                );
            });
            window.draw(cx).clear(cx);
        });
        dialog_view.read_with(&vcx, |view, _| {
            assert!(matches!(view.state, FileLocksState::Results(ref list) if list.len() == 1));
        });

        coordinator.shutdown();
    }
}
