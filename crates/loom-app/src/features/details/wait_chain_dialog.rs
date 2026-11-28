//! Feature-local **Analyze wait chain** dialog for the Details view.
//!
//! Mounted via `gpui-kit`'s `Root` dialog layer (`WindowExt::open_dialog`) and composed
//! exclusively from `gpui-kit` built-in widgets (`Tree`, `TreeState`, `TreeItem`, `ListItem`,
//! `Button`, `Spinner`, `DialogFooter`).

use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::list::ListItem;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tree::{TreeItem, TreeState, tree};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, Render, SharedString, Styled as _, Task, Window, div,
    prelude::FluentBuilder as _, px,
};
use loom_engine::CommandSuccess;
use loom_types::{CapabilityFlags, ProcessKey, WaitChainNode};

use crate::bridge::{UiCommands, UiStore};
use crate::i18n;

use super::state::process_key_element_id;

/// Flattened display metadata keyed by [`TreeItem::id`] for rendering each wait-chain row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WaitChainNodeMeta {
    /// OS thread ID (`0` for synchronization object nodes).
    pub(super) thread_id: u32,
    /// Owning process key if this node is a thread belonging to a resolvable process.
    pub(super) process_key: Option<ProcessKey>,
    /// Owning process image name, when resolved.
    pub(super) process_name: Option<Arc<str>>,
    /// Synchronization object type (e.g., `Thread`, `SendMessage`, `Mutex`, `CriticalSection`).
    pub(super) object_type: Arc<str>,
    /// Optional synchronization object name.
    pub(super) object_name: Option<Arc<str>>,
    /// Whether this node participates in a detected deadlock cycle.
    pub(super) is_deadlocked: bool,
    /// `true` when this node represents another killable blocking process (`pid > 4` and != target).
    pub(super) can_end_blocking: bool,
}

/// Converts nested [`WaitChainNode`] roots into expanded `gpui-kit` [`TreeItem`]s and
/// an ID-keyed metadata map for rendering icons, badges, and inline actions.
pub(super) fn build_wait_chain_tree_items(
    nodes: &[WaitChainNode],
    target_key: ProcessKey,
) -> (
    Vec<TreeItem>,
    HashMap<SharedString, WaitChainNodeMeta>,
    bool,
) {
    let mut meta = HashMap::new();
    let mut any_deadlock = false;

    fn convert(
        node: &WaitChainNode,
        id_path: String,
        target_key: ProcessKey,
        meta: &mut HashMap<SharedString, WaitChainNodeMeta>,
        any_deadlock: &mut bool,
    ) -> TreeItem {
        if node.is_deadlocked {
            *any_deadlock = true;
        }
        let id: SharedString = id_path.clone().into();
        let label = format_wait_chain_node_label(node);
        let can_end_blocking = node
            .process_key
            .is_some_and(|pk| pk != target_key && pk.pid > 4);

        meta.insert(
            id.clone(),
            WaitChainNodeMeta {
                thread_id: node.thread_id,
                process_key: node.process_key,
                process_name: node.process_name.clone(),
                object_type: Arc::clone(&node.object_type),
                object_name: node.object_name.clone(),
                is_deadlocked: node.is_deadlocked,
                can_end_blocking,
            },
        );

        let children: Vec<TreeItem> = node
            .children
            .iter()
            .enumerate()
            .map(|(ix, child)| {
                convert(
                    child,
                    format!("{id_path}.{ix}"),
                    target_key,
                    meta,
                    any_deadlock,
                )
            })
            .collect();

        TreeItem::new(id, label).expanded(true).children(children)
    }

    let items = nodes
        .iter()
        .enumerate()
        .map(|(ix, node)| {
            convert(
                node,
                ix.to_string(),
                target_key,
                &mut meta,
                &mut any_deadlock,
            )
        })
        .collect();

    (items, meta, any_deadlock)
}

/// Formats a concise human-readable label for a [`WaitChainNode`].
pub(super) fn format_wait_chain_node_label(node: &WaitChainNode) -> String {
    if node.object_type.as_ref() == "Thread" || node.thread_id != 0 {
        match (&node.process_name, node.process_key) {
            (Some(proc_name), Some(pk)) => {
                format!("Thread {} — {} (PID {})", node.thread_id, proc_name, pk.pid)
            }
            (None, Some(pk)) => format!("Thread {} — PID {}", node.thread_id, pk.pid),
            (Some(proc_name), None) => format!("Thread {} — {}", node.thread_id, proc_name),
            (None, None) => format!("Thread {}", node.thread_id),
        }
    } else if let Some(ref obj_name) = node.object_name {
        format!("{}: {}", node.object_type, obj_name)
    } else {
        node.object_type.to_string()
    }
}

enum WaitChainLoadState {
    Loading,
    Ready {
        empty: bool,
        has_deadlock: bool,
        tree_state: Entity<TreeState>,
        node_meta: Arc<HashMap<SharedString, WaitChainNodeMeta>>,
    },
    ElevationRequired,
    Error(String),
}

struct WaitChainDialogView {
    store: Entity<UiStore>,
    key: ProcessKey,
    name: String,
    state: WaitChainLoadState,
    _load_task: Option<Task<()>>,
}

impl WaitChainDialogView {
    fn new(
        store: Entity<UiStore>,
        key: ProcessKey,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self {
            store,
            key,
            name,
            state: WaitChainLoadState::Loading,
            _load_task: None,
        };
        view.start_analysis(window, cx);
        view
    }

    fn start_analysis(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.state = WaitChainLoadState::Loading;
        cx.notify();

        let key = self.key;
        match UiCommands::analyze_wait_chain_async(&self.store, key, cx) {
            Ok(reply_rx) => {
                self._load_task = Some(cx.spawn_in(window, async move |weak, cx| {
                    let recv = cx
                        .background_executor()
                        .spawn(async move { reply_rx.recv() })
                        .await;
                    let _ = weak.update(cx, |this, cx| {
                        this.state = match recv {
                            Ok(event) => {
                                if event.is_elevation_required() {
                                    WaitChainLoadState::ElevationRequired
                                } else if let Some(CommandSuccess::WaitChain(nodes)) =
                                    event.outcome.as_ok()
                                {
                                    let empty = nodes.is_empty();
                                    let (items, meta, has_deadlock) =
                                        build_wait_chain_tree_items(nodes, key);
                                    let tree_state = cx.new(|cx| TreeState::new(cx).items(items));
                                    WaitChainLoadState::Ready {
                                        empty,
                                        has_deadlock,
                                        tree_state,
                                        node_meta: Arc::new(meta),
                                    }
                                } else if let Some(err) = event.error() {
                                    WaitChainLoadState::Error(err.to_string())
                                } else {
                                    WaitChainLoadState::Error(
                                        "Unexpected wait chain query response".to_owned(),
                                    )
                                }
                            }
                            Err(_) => WaitChainLoadState::Error(
                                "Wait chain query channel closed".to_owned(),
                            ),
                        };
                        cx.notify();
                    });
                }));
            }
            Err(err) => {
                self.state = WaitChainLoadState::Error(err.to_string());
                self._load_task = None;
                cx.notify();
            }
        }
    }
}

impl Render for WaitChainDialogView {
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

        let content = match &self.state {
            WaitChainLoadState::Loading => v_flex()
                .w_full()
                .gap_4()
                .child(
                    h_flex()
                        .w_full()
                        .py_6()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .child(Spinner::new().small().color(muted_fg))
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted_fg)
                                .child(i18n::tr("details.wait_chain_loading")),
                        ),
                )
                .child(
                    DialogFooter::new().child(
                        Button::new("wait-chain-loading-close")
                            .outline()
                            .label(i18n::tr("common.close"))
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    ),
                )
                .into_any_element(),

            WaitChainLoadState::ElevationRequired => {
                let store = self.store.clone();
                let supports_relaunch = self
                    .store
                    .read(cx)
                    .snapshot()
                    .capabilities
                    .contains(CapabilityFlags::ELEVATION_RELAUNCH);
                v_flex()
                    .w_full()
                    .gap_4()
                    .child(
                        h_flex()
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
                            ),
                    )
                    .child(
                        DialogFooter::new()
                            .gap_2()
                            .child(
                                Button::new("wait-chain-uac-close")
                                    .outline()
                                    .label(i18n::tr("common.close"))
                                    .on_click(|_, window, cx| {
                                        window.close_dialog(cx);
                                    }),
                            )
                            .when(supports_relaunch, |footer| {
                                footer.child(
                                    Button::new("wait-chain-uac-relaunch")
                                        .primary()
                                        .label(i18n::tr("uac.action"))
                                        .on_click(move |_, window, cx| {
                                            UiCommands::relaunch_elevated(&store, "details", cx);
                                            window.close_dialog(cx);
                                        }),
                                )
                            }),
                    )
                    .into_any_element()
            }

            WaitChainLoadState::Error(message) => v_flex()
                .w_full()
                .gap_4()
                .child(
                    h_flex()
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
                        ),
                )
                .child(
                    DialogFooter::new()
                        .gap_2()
                        .child(
                            Button::new("wait-chain-error-retry")
                                .outline()
                                .label(i18n::tr("details.wait_chain_refresh"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.start_analysis(window, cx);
                                })),
                        )
                        .child(
                            Button::new("wait-chain-error-close")
                                .primary()
                                .label(i18n::tr("common.close"))
                                .on_click(|_, window, cx| {
                                    window.close_dialog(cx);
                                }),
                        ),
                )
                .into_any_element(),

            WaitChainLoadState::Ready {
                empty,
                has_deadlock,
                tree_state,
                node_meta,
            } => {
                let empty = *empty;
                let has_deadlock = *has_deadlock;

                let body = if empty {
                    let normal_msg =
                        rust_i18n::t!("details.wait_chain_normal", name = self.name.as_str())
                            .into_owned();
                    h_flex()
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
                                .child(normal_msg),
                        )
                        .into_any_element()
                } else {
                    let tree_state = tree_state.clone();
                    let meta_map = Arc::clone(node_meta);
                    let store_for_end = self.store.clone();
                    let deadlock_label = i18n::tr("details.wait_chain_deadlock_badge");
                    let end_blocking_label = i18n::tr("details.wait_chain_end_blocking");

                    v_flex()
                        .w_full()
                        .gap_2p5()
                        .when(has_deadlock, |col| {
                            col.child(
                                h_flex()
                                    .w_full()
                                    .items_center()
                                    .gap_2()
                                    .px_3()
                                    .py_2()
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
                                            .child(i18n::tr("details.wait_chain_deadlock_banner")),
                                    ),
                            )
                        })
                        .child(
                            div()
                                .w_full()
                                .h(px(300.))
                                .rounded(radius)
                                .border_1()
                                .border_color(border)
                                .bg(muted_bg.opacity(0.22))
                                .overflow_hidden()
                                .child(tree(
                                    &tree_state,
                                    move |ix, entry, _selected, _window, cx| {
                                        let theme = cx.theme();
                                        let item_id = &entry.item().id;
                                        let node_info = meta_map.get(item_id);
                                        let is_deadlocked =
                                            node_info.is_some_and(|m| m.is_deadlocked);
                                        let blocking_key = node_info
                                            .filter(|m| m.can_end_blocking)
                                            .and_then(|m| m.process_key);
                                        let is_thread = node_info.is_some_and(|m| {
                                            m.object_type.as_ref() == "Thread" || m.thread_id != 0
                                        });

                                        let chevron_icon = if entry.is_folder() {
                                            if entry.is_expanded() {
                                                Some(IconName::ChevronDown)
                                            } else {
                                                Some(IconName::ChevronRight)
                                            }
                                        } else {
                                            None
                                        };

                                        let node_icon = if is_deadlocked {
                                            Icon::new(IconName::TriangleAlert)
                                                .xsmall()
                                                .text_color(theme.danger)
                                        } else if is_thread {
                                            Icon::new(IconName::Cpu)
                                                .xsmall()
                                                .text_color(theme.primary)
                                        } else {
                                            Icon::new(IconName::Info)
                                                .xsmall()
                                                .text_color(theme.warning)
                                        };

                                        let indent = px(16. * entry.depth() as f32 + 8.);
                                        let store_btn = store_for_end.clone();
                                        let deadlock_badge_text = deadlock_label.clone();
                                        let end_btn_text = end_blocking_label.clone();

                                        ListItem::new(("wait-chain-row", ix))
                                            .pl(indent)
                                            .pr_2()
                                            .py_1()
                                            .child(
                                                h_flex()
                                                    .w_full()
                                                    .items_center()
                                                    .justify_between()
                                                    .gap_2()
                                                    .child(
                                                        h_flex()
                                                            .flex_1()
                                                            .min_w_0()
                                                            .items_center()
                                                            .gap_1p5()
                                                            .child(
                                                                if let Some(chev) = chevron_icon {
                                                                    Icon::new(chev)
                                                                        .xsmall()
                                                                        .text_color(
                                                                            theme.muted_foreground,
                                                                        )
                                                                        .into_any_element()
                                                                } else {
                                                                    div()
                                                                        .w(px(12.))
                                                                        .flex_shrink_0()
                                                                        .into_any_element()
                                                                },
                                                            )
                                                            .child(node_icon)
                                                            .child(
                                                                div()
                                                                    .flex_1()
                                                                    .min_w_0()
                                                                    .truncate()
                                                                    .text_xs()
                                                                    .text_color(theme.foreground)
                                                                    .child(
                                                                        entry.item().label.clone(),
                                                                    ),
                                                            )
                                                            .when(is_deadlocked, |row| {
                                                                row.child(
                                                                    div()
                                                                        .flex_shrink_0()
                                                                        .px_1p5()
                                                                        .py_0p5()
                                                                        .rounded(theme.radius)
                                                                        .bg(theme
                                                                            .danger
                                                                            .opacity(0.16))
                                                                        .text_xs()
                                                                        .font_medium()
                                                                        .text_color(theme.danger)
                                                                        .child(deadlock_badge_text),
                                                                )
                                                            }),
                                                    )
                                                    .when_some(blocking_key, |row, bkey| {
                                                        row.child(
                                                        div()
                                                            .flex_shrink_0()
                                                            .on_mouse_down(
                                                                MouseButton::Left,
                                                                |_, _, cx| {
                                                                    cx.stop_propagation();
                                                                },
                                                            )
                                                            .child(
                                                                Button::new((
                                                                    "wait-chain-end-blocker",
                                                                    process_key_element_id(bkey),
                                                                ))
                                                                .xsmall()
                                                                .danger()
                                                                .outline()
                                                                .label(end_btn_text)
                                                                .on_click(move |_, _, cx| {
                                                                    UiCommands::terminate_process(
                                                                        &store_btn, bkey, cx,
                                                                    );
                                                                }),
                                                            ),
                                                    )
                                                    }),
                                            )
                                    },
                                )),
                        )
                        .into_any_element()
                };

                v_flex()
                    .w_full()
                    .gap_3()
                    .child(body)
                    .child(
                        DialogFooter::new()
                            .gap_2()
                            .child(
                                Button::new("wait-chain-refresh")
                                    .outline()
                                    .label(i18n::tr("details.wait_chain_refresh"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.start_analysis(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("wait-chain-close")
                                    .primary()
                                    .label(i18n::tr("common.close"))
                                    .on_click(|_, window, cx| {
                                        window.close_dialog(cx);
                                    }),
                            ),
                    )
                    .into_any_element()
            }
        };

        v_flex().w_full().gap_3().child(content)
    }
}

/// Opens the feature-local **Analyze wait chain** dialog inside `gpui-kit`'s `Root` dialog layer.
pub(super) fn open_wait_chain_dialog(
    store: Entity<UiStore>,
    key: ProcessKey,
    name: String,
    window: &mut Window,
    cx: &mut App,
) {
    let dialog_title = format!(
        "{} — {} (PID {})",
        i18n::tr("details.wait_chain_title"),
        name,
        key.pid
    );
    let dialog_view = cx.new(|cx| WaitChainDialogView::new(store, key, name, window, cx));
    window.open_dialog(cx, move |dialog, _window, _cx| {
        dialog
            .w(px(620.))
            .centered()
            .title(dialog_title.clone())
            .child(dialog_view.clone())
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use loom_types::{ProcessKey, WaitChainNode};

    use super::{build_wait_chain_tree_items, format_wait_chain_node_label};

    #[test]
    fn build_wait_chain_tree_items_preserves_hierarchy_deadlock_and_blocker_eligibility() {
        let target = ProcessKey::new(100, 1_000);
        let blocker = ProcessKey::new(200, 2_000);

        let chain = vec![WaitChainNode {
            thread_id: 11,
            process_key: Some(target),
            process_name: Some(Arc::from("app.exe")),
            object_type: Arc::from("Thread"),
            object_name: None,
            is_deadlocked: true,
            children: vec![WaitChainNode {
                thread_id: 0,
                process_key: None,
                process_name: None,
                object_type: Arc::from("Mutex"),
                object_name: Some(Arc::from("Global\\DemoMutex")),
                is_deadlocked: true,
                children: vec![WaitChainNode {
                    thread_id: 22,
                    process_key: Some(blocker),
                    process_name: Some(Arc::from("blocker.exe")),
                    object_type: Arc::from("Thread"),
                    object_name: None,
                    is_deadlocked: true,
                    children: Vec::new(),
                }],
            }],
        }];

        let (items, meta, has_deadlock) = build_wait_chain_tree_items(&chain, target);
        assert!(has_deadlock);
        assert_eq!(items.len(), 1);
        assert!(items[0].is_expanded());
        assert_eq!(items[0].children.len(), 1);
        assert_eq!(items[0].children[0].children.len(), 1);

        let root_meta = meta.get(&items[0].id).expect("root meta");
        assert!(
            !root_meta.can_end_blocking,
            "target process itself is not marked as external blocker"
        );

        let lock_meta = meta.get(&items[0].children[0].id).expect("lock meta");
        assert_eq!(lock_meta.object_type.as_ref(), "Mutex");
        assert!(!lock_meta.can_end_blocking);

        let blocker_meta = meta
            .get(&items[0].children[0].children[0].id)
            .expect("blocker meta");
        assert_eq!(blocker_meta.process_key, Some(blocker));
        assert!(blocker_meta.can_end_blocking);

        assert_eq!(
            format_wait_chain_node_label(&chain[0]),
            "Thread 11 — app.exe (PID 100)"
        );
        assert_eq!(
            format_wait_chain_node_label(&chain[0].children[0]),
            "Mutex: Global\\DemoMutex"
        );
    }
}
