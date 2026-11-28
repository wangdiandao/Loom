//! Feature-local **Set Affinity** dialog for the Details view.
//!
//! Mounted via `gpui-kit`'s `Root` dialog layer (`WindowExt::open_dialog`) so it
//! never bloats `shell/` with feature-specific dialog state. Supports multi-group
//! processor affinity masks with `> 64` logical CPUs (OS processor groups).

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Styled as _, Task, Window, div, prelude::FluentBuilder as _, px,
};
use loom_engine::CommandSuccess;
use loom_types::{CapabilityFlags, CpuAffinityMask, ProcessKey};

use crate::bridge::{UiCommands, UiStore};
use crate::i18n;

/// Number of logical CPUs per OS processor group.
pub(super) const CPUS_PER_GROUP: u32 = 64;

/// Contiguous logical CPU range belonging to a single processor group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CpuGroupRange {
    /// Zero-based processor group index (`0`, `1`, ...).
    pub(super) group_index: usize,
    /// Inclusive first logical CPU index in this group.
    pub(super) start_cpu: u32,
    /// Exclusive upper logical CPU index in this group.
    pub(super) end_cpu_exclusive: u32,
}

/// Slices `logical_cpu_count` into 64-CPU processor group ranges.
pub(super) fn compute_cpu_groups(logical_cpu_count: u32) -> Vec<CpuGroupRange> {
    if logical_cpu_count == 0 {
        return Vec::new();
    }

    let group_count = (logical_cpu_count as usize).div_ceil(CPUS_PER_GROUP as usize);
    let mut groups = Vec::with_capacity(group_count);
    for group_index in 0..group_count {
        let start_cpu = (group_index as u32).saturating_mul(CPUS_PER_GROUP);
        let end_cpu_exclusive = start_cpu
            .saturating_add(CPUS_PER_GROUP)
            .min(logical_cpu_count);
        groups.push(CpuGroupRange {
            group_index,
            start_cpu,
            end_cpu_exclusive,
        });
    }
    groups
}

/// Pure selection state for the Set Affinity checkbox grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AffinitySelectionState {
    mask: CpuAffinityMask,
}

impl AffinitySelectionState {
    /// Creates a new selection state from an existing [`CpuAffinityMask`].
    pub(super) fn new(mask: CpuAffinityMask) -> Self {
        Self { mask }
    }

    /// Returns a reference to the underlying [`CpuAffinityMask`].
    #[inline]
    pub(super) fn mask(&self) -> &CpuAffinityMask {
        &self.mask
    }

    /// Returns `true` if every logical CPU is currently enabled.
    #[inline]
    pub(super) fn all_checked(&self) -> bool {
        self.mask.logical_cpu_count > 0 && self.mask.enabled_count() == self.mask.logical_cpu_count
    }

    /// Returns `true` if at least one CPU is enabled, but not all of them.
    #[inline]
    pub(super) fn is_indeterminate(&self) -> bool {
        let enabled = self.mask.enabled_count();
        enabled > 0 && enabled < self.mask.logical_cpu_count
    }

    /// Returns `true` if no logical CPUs are enabled.
    #[inline]
    pub(super) fn is_empty(&self) -> bool {
        self.mask.is_empty()
    }

    /// Returns `true` when the system exposes more than one 64-CPU processor group.
    #[inline]
    pub(super) fn has_multiple_groups(&self) -> bool {
        self.mask.logical_cpu_count > CPUS_PER_GROUP
    }

    /// Returns the processor group ranges for this mask.
    pub(super) fn groups(&self) -> Vec<CpuGroupRange> {
        compute_cpu_groups(self.mask.logical_cpu_count)
    }

    /// Enables or clears every logical CPU when the master `All processors` checkbox changes.
    pub(super) fn toggle_all(&mut self, checked: bool) {
        self.mask = if checked {
            CpuAffinityMask::all(self.mask.logical_cpu_count)
        } else {
            CpuAffinityMask::empty(self.mask.logical_cpu_count)
        };
    }

    /// Toggles a single logical CPU bit in the affinity mask.
    pub(super) fn set_cpu(&mut self, cpu_idx: u32, enabled: bool) -> bool {
        self.mask.set(cpu_idx, enabled)
    }
}

/// Asynchronous loading state of [`SetAffinityDialogView`].
enum AffinityLoadState {
    Loading,
    Ready(AffinitySelectionState),
    ElevationRequired,
    Error(String),
}

/// Feature-local GPUI view rendered inside `gpui-kit`'s `Root` dialog layer.
struct SetAffinityDialogView {
    store: Entity<UiStore>,
    key: ProcessKey,
    state: AffinityLoadState,
    _load_task: Option<Task<()>>,
}

impl SetAffinityDialogView {
    fn new(
        store: Entity<UiStore>,
        key: ProcessKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (state, load_task) = match UiCommands::get_affinity_async(&store, key, cx) {
            Ok(reply_rx) => {
                let task = cx.spawn_in(window, async move |weak, cx| {
                    let recv = cx
                        .background_executor()
                        .spawn(async move { reply_rx.recv() })
                        .await;
                    let next_state = match recv {
                        Ok(event) => {
                            if event.is_elevation_required() {
                                AffinityLoadState::ElevationRequired
                            } else if let Some(CommandSuccess::Affinity(mask)) =
                                event.outcome.as_ok()
                            {
                                AffinityLoadState::Ready(AffinitySelectionState::new(mask.clone()))
                            } else if let Some(err) = event.error() {
                                AffinityLoadState::Error(err.to_string())
                            } else {
                                AffinityLoadState::Error(
                                    "Unexpected affinity query response".to_owned(),
                                )
                            }
                        }
                        Err(_) => {
                            AffinityLoadState::Error("Affinity query channel closed".to_owned())
                        }
                    };

                    let _ = weak.update(cx, |this, cx| {
                        this.state = next_state;
                        cx.notify();
                    });
                });
                (AffinityLoadState::Loading, Some(task))
            }
            Err(err) => (AffinityLoadState::Error(err.to_string()), None),
        };

        Self {
            store,
            key,
            state,
            _load_task: load_task,
        }
    }

    fn apply_and_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let AffinityLoadState::Ready(selection) = &self.state else {
            return;
        };
        if selection.is_empty() {
            cx.notify();
            return;
        }
        let mask = selection.mask().clone();
        UiCommands::set_affinity(&self.store, self.key, mask, cx);
        window.close_dialog(cx);
    }
}

impl Render for SetAffinityDialogView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let muted_bg = theme.muted;
        let border = theme.border;
        let danger = theme.danger;
        let warning = theme.warning;
        let radius = theme.radius;

        let content = match &self.state {
            AffinityLoadState::Loading => v_flex()
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
                                .child(i18n::tr("details.affinity_loading")),
                        ),
                )
                .child(
                    DialogFooter::new().child(
                        Button::new("affinity-loading-cancel")
                            .outline()
                            .label(i18n::tr("common.cancel"))
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    ),
                )
                .into_any_element(),

            AffinityLoadState::ElevationRequired => {
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
                            .child(
                                Button::new("affinity-elevation-close")
                                    .outline()
                                    .label(i18n::tr("common.close"))
                                    .on_click(|_, window, cx| {
                                        window.close_dialog(cx);
                                    }),
                            )
                            .when(supports_relaunch, |footer| {
                                footer.child(
                                    Button::new("affinity-elevation-relaunch")
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

            AffinityLoadState::Error(message) => v_flex()
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
                    DialogFooter::new().child(
                        Button::new("affinity-error-close")
                            .outline()
                            .label(i18n::tr("common.close"))
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    ),
                )
                .into_any_element(),

            AffinityLoadState::Ready(selection) => {
                let all_checked = selection.all_checked();
                let indeterminate = selection.is_indeterminate();
                let is_empty = selection.is_empty();
                let enabled_count = selection.mask().enabled_count();
                let total_cpus = selection.mask().logical_cpu_count;
                let has_multiple_groups = selection.has_multiple_groups();
                let groups = selection.groups();

                let mut group_sections = Vec::with_capacity(groups.len());
                for group in groups {
                    let mut cpu_items =
                        Vec::with_capacity((group.end_cpu_exclusive - group.start_cpu) as usize);
                    for cpu_idx in group.start_cpu..group.end_cpu_exclusive {
                        let checked = selection.mask().is_enabled(cpu_idx);
                        let label = rust_i18n::t!("details.affinity_cpu_label", index = cpu_idx)
                            .into_owned();
                        cpu_items.push(
                            div().w(px(110.)).flex_shrink_0().child(
                                Checkbox::new(("affinity-cpu", cpu_idx as usize))
                                    .label(label)
                                    .checked(checked)
                                    .on_change(cx.listener(move |this, next: &bool, _, cx| {
                                        if let AffinityLoadState::Ready(sel) = &mut this.state {
                                            sel.set_cpu(cpu_idx, *next);
                                            cx.notify();
                                        }
                                    })),
                            ),
                        );
                    }

                    let group_label =
                        rust_i18n::t!("details.affinity_group_label", group = group.group_index)
                            .into_owned();

                    group_sections.push(
                        v_flex()
                            .w_full()
                            .gap_2()
                            .when(has_multiple_groups, |section| {
                                section.child(
                                    h_flex()
                                        .w_full()
                                        .items_center()
                                        .justify_between()
                                        .px_2()
                                        .py_1()
                                        .rounded(radius)
                                        .bg(muted_bg.opacity(0.65))
                                        .text_xs()
                                        .font_semibold()
                                        .text_color(fg)
                                        .child(group_label)
                                        .child(div().text_color(muted_fg).child(format!(
                                            "CPU {}–{}",
                                            group.start_cpu,
                                            group.end_cpu_exclusive.saturating_sub(1)
                                        ))),
                                )
                            })
                            .child(h_flex().w_full().flex_wrap().gap_2().children(cpu_items)),
                    );
                }

                v_flex()
                    .w_full()
                    .gap_3()
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .pb_2()
                            .border_b_1()
                            .border_color(border)
                            .child(
                                Checkbox::new("affinity-all-cpus")
                                    .label(i18n::tr("details.affinity_all_cpus"))
                                    .checked(all_checked)
                                    .indeterminate(indeterminate)
                                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                        if let AffinityLoadState::Ready(sel) = &mut this.state {
                                            sel.toggle_all(*checked);
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded(radius)
                                    .bg(muted_bg.opacity(0.7))
                                    .text_xs()
                                    .text_color(muted_fg)
                                    .child(format!("{enabled_count} / {total_cpus}")),
                            ),
                    )
                    .child(
                        v_flex()
                            .id("affinity-cpu-grid-scroll")
                            .w_full()
                            .max_h(px(320.))
                            .overflow_y_scrollbar()
                            .gap_3()
                            .pr_2()
                            .children(group_sections),
                    )
                    .when(is_empty, |container| {
                        container.child(
                            h_flex()
                                .w_full()
                                .items_center()
                                .gap_2()
                                .px_2p5()
                                .py_2()
                                .rounded(radius)
                                .bg(danger.opacity(0.12))
                                .border_1()
                                .border_color(danger.opacity(0.35))
                                .text_xs()
                                .text_color(danger)
                                .child(Icon::new(IconName::CircleAlert).xsmall().text_color(danger))
                                .child(i18n::tr("details.affinity_empty_error")),
                        )
                    })
                    .child(
                        DialogFooter::new()
                            .pt_1()
                            .child(
                                Button::new("affinity-cancel")
                                    .outline()
                                    .label(i18n::tr("common.cancel"))
                                    .on_click(|_, window, cx| {
                                        window.close_dialog(cx);
                                    }),
                            )
                            .child(
                                Button::new("affinity-apply")
                                    .primary()
                                    .label(i18n::tr("details.affinity_apply"))
                                    .disabled(is_empty)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.apply_and_close(window, cx);
                                    })),
                            ),
                    )
                    .into_any_element()
            }
        };

        v_flex().w_full().gap_3().child(content)
    }
}

/// Opens the feature-local **Set Affinity** dialog inside `gpui-kit`'s `Root` dialog layer.
pub(super) fn open_set_affinity_dialog(
    store: Entity<UiStore>,
    key: ProcessKey,
    name: String,
    window: &mut Window,
    cx: &mut App,
) {
    let dialog_title = format!(
        "{} — {} (PID {})",
        i18n::tr("details.affinity_title"),
        name,
        key.pid
    );
    let dialog_view = cx.new(|cx| SetAffinityDialogView::new(store, key, window, cx));
    window.open_dialog(cx, move |dialog, _window, _cx| {
        dialog
            .w(px(540.))
            .title(dialog_title.clone())
            .child(dialog_view.clone())
    });
}
