//! On-demand Details vertical feature slice gated by an RAII [`ScopedMonitorLease`].
//!
//! Acquires `MonitorDomain::Details` upon construction and automatically releases it
//! when unmounted so hidden views incur zero projection overhead.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::{
    Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render, Styled as _,
    Subscription, UniformListScrollHandle, Window, div, prelude::FluentBuilder as _, px,
    uniform_list,
};
use loom_types::{MonitorDomain, ProcessKey, ProcessStatusFlags};
use loom_ui_kit::{HeatmapLevel, LoomSemanticColors, format_bytes, format_permille, heatmap_cell};

use crate::bridge::{ScopedMonitorLease, UiStore, UiStoreEvent};
use crate::i18n;

const DETAILS_ROW_HEIGHT: f32 = 28.0;

/// Live Details table preview holding a scoped `MonitorDomain::Details` lease.
pub(crate) struct DetailsView {
    store: Entity<UiStore>,
    lease: ScopedMonitorLease,
    last_details_rev: u64,
    highlighted_key: Option<ProcessKey>,
    scroll_handle: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl DetailsView {
    /// Creates a new [`DetailsView`], acquiring `MonitorDomain::Details` immediately.
    pub(crate) fn new(
        store: Entity<UiStore>,
        highlighted_key: Option<ProcessKey>,
        cx: &mut Context<Self>,
    ) -> Self {
        let lease = store.read(cx).acquire_lease(MonitorDomain::Details);
        let initial_rev = store.read(cx).snapshot().revisions.details;
        let sub = cx.subscribe(&store, |this, _store, event, cx| match event {
            UiStoreEvent::SnapshotUpdated(revs) => {
                if revs.details != this.last_details_rev {
                    this.last_details_rev = revs.details;
                    cx.notify();
                }
            }
            UiStoreEvent::SettingsChanged => cx.notify(),
            UiStoreEvent::ElevationRequired { .. } | UiStoreEvent::CommandFailed { .. } => {}
        });

        Self {
            store,
            lease,
            last_details_rev: initial_rev,
            highlighted_key,
            scroll_handle: UniformListScrollHandle::new(),
            _subscriptions: vec![sub],
        }
    }

    /// Updates the highlighted target [`ProcessKey`] (e.g., from `NavigationIntent::JumpToDetails`).
    pub(crate) fn set_highlighted_key(&mut self, key: Option<ProcessKey>, cx: &mut Context<Self>) {
        self.highlighted_key = key;
        if let Some(target) = key {
            let rows = &self.store.read(cx).snapshot().details.rows;
            if let Some(ix) = rows.iter().position(|row| row.key == target) {
                self.scroll_handle
                    .scroll_to_item(ix, gpui_kit::ScrollStrategy::Center);
            }
        }
        cx.notify();
    }
}

impl Render for DetailsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        debug_assert_eq!(self.lease.domain(), MonitorDomain::Details);

        let snapshot = self.store.read(cx).snapshot().clone();
        let details = snapshot.details.clone();
        let row_count = details.rows.len();
        let highlighted = self.highlighted_key;

        let theme = cx.theme();
        let border = theme.border;
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let muted_bg = theme.muted;
        let secondary_bg = theme.secondary;
        let primary = theme.primary;
        let success = theme.success;
        let radius = theme.radius;
        let mono_font = theme.mono_font_family.clone();
        let semantic = LoomSemanticColors::from_theme(theme);

        let header = h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .gap_3()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(border)
            .child(
                h_flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .text_color(fg)
                            .child(i18n::tr("details.title")),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1p5()
                            .px_2p5()
                            .py_1()
                            .rounded(radius)
                            .bg(success.opacity(0.14))
                            .text_xs()
                            .text_color(success)
                            .child(Icon::new(IconName::CircleCheck).xsmall())
                            .child(i18n::tr("details.lease_active")),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1p5()
                            .px_2p5()
                            .py_1()
                            .rounded(radius)
                            .bg(muted_bg.opacity(0.65))
                            .text_xs()
                            .child(
                                div()
                                    .text_color(muted_fg)
                                    .child(i18n::tr("details.rows_label")),
                            )
                            .child(
                                div()
                                    .font_semibold()
                                    .text_color(fg)
                                    .child(row_count.to_string()),
                            ),
                    ),
            )
            .when_some(highlighted, |this, key| {
                this.child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .px_2p5()
                        .py_1()
                        .rounded(radius)
                        .bg(primary.opacity(0.14))
                        .text_xs()
                        .text_color(primary)
                        .child(format!(
                            "{}: {}",
                            i18n::tr("details.highlighted_pid"),
                            key.pid
                        ))
                        .child(
                            Button::new("details-clear-highlight")
                                .xsmall()
                                .ghost()
                                .label(i18n::tr("details.clear_highlight"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_highlighted_key(None, cx);
                                })),
                        ),
                )
            });

        let column_header = h_flex()
            .w_full()
            .h(px(30.))
            .flex_shrink_0()
            .items_center()
            .px_4()
            .gap_2()
            .bg(secondary_bg.opacity(0.45))
            .border_b_1()
            .border_color(border)
            .text_xs()
            .font_medium()
            .text_color(muted_fg)
            .child(div().flex_1().min_w_0().child(i18n::tr("details.col_name")))
            .child(
                div()
                    .w(px(64.))
                    .flex_shrink_0()
                    .text_right()
                    .child(i18n::tr("details.col_pid")),
            )
            .child(
                div()
                    .w(px(92.))
                    .flex_shrink_0()
                    .child(i18n::tr("details.col_status")),
            )
            .child(
                div()
                    .w(px(120.))
                    .flex_shrink_0()
                    .child(i18n::tr("details.col_user")),
            )
            .child(
                div()
                    .w(px(72.))
                    .flex_shrink_0()
                    .text_right()
                    .child(i18n::tr("details.col_cpu")),
            )
            .child(
                div()
                    .w(px(92.))
                    .flex_shrink_0()
                    .text_right()
                    .child(i18n::tr("details.col_memory")),
            )
            .child(
                div()
                    .w(px(64.))
                    .flex_shrink_0()
                    .text_right()
                    .child(i18n::tr("details.col_threads")),
            )
            .child(
                div()
                    .w(px(64.))
                    .flex_shrink_0()
                    .text_right()
                    .child(i18n::tr("details.col_handles")),
            )
            .child(
                div()
                    .w(px(90.))
                    .flex_shrink_0()
                    .child(i18n::tr("details.col_priority")),
            )
            .child(
                div()
                    .w(px(64.))
                    .flex_shrink_0()
                    .child(i18n::tr("details.col_platform")),
            );

        let body = if details.revision == 0 {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .p_8()
                .child(
                    div()
                        .text_sm()
                        .text_color(muted_fg)
                        .child(i18n::tr("details.loading")),
                )
                .into_any_element()
        } else if row_count == 0 {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .p_8()
                .child(
                    div()
                        .text_sm()
                        .text_color(muted_fg)
                        .child(i18n::tr("details.empty_search")),
                )
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(
                    uniform_list(
                        "details-virtual-list",
                        row_count,
                        move |range, _window, cx| {
                            let theme = cx.theme();
                            let row_border = theme.border.opacity(0.45);
                            let row_hover = theme.accent.opacity(0.45);
                            let highlight_bg = theme.primary.opacity(0.16);

                            range
                                .filter_map(|ix| {
                                    let row = details.rows.get(ix)?;
                                    let is_highlighted = highlighted == Some(row.key);
                                    let status_text =
                                        if row.status.contains(ProcessStatusFlags::SUSPENDED) {
                                            i18n::tr("processes.status_suspended")
                                        } else if row
                                            .status
                                            .contains(ProcessStatusFlags::NOT_RESPONDING)
                                        {
                                            i18n::tr("processes.status_not_responding")
                                        } else {
                                            i18n::tr("details.status_running")
                                        };
                                    let user_text =
                                        row.user_name.as_deref().unwrap_or("—").to_owned();

                                    Some(
                                        h_flex()
                                            .id(("details-row", row.key.pid as usize))
                                            .w_full()
                                            .h(px(DETAILS_ROW_HEIGHT))
                                            .items_center()
                                            .px_4()
                                            .gap_2()
                                            .border_b_1()
                                            .border_color(row_border)
                                            .text_xs()
                                            .text_color(fg)
                                            .when(is_highlighted, |s| s.bg(highlight_bg))
                                            .hover(|s| s.bg(row_hover))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .child(row.name.to_string()),
                                            )
                                            .child(
                                                div()
                                                    .w(px(64.))
                                                    .flex_shrink_0()
                                                    .text_right()
                                                    .font_family(mono_font.clone())
                                                    .text_color(muted_fg)
                                                    .child(row.key.pid.to_string()),
                                            )
                                            .child(
                                                div()
                                                    .w(px(92.))
                                                    .flex_shrink_0()
                                                    .truncate()
                                                    .text_color(muted_fg)
                                                    .child(status_text),
                                            )
                                            .child(
                                                div()
                                                    .w(px(120.))
                                                    .flex_shrink_0()
                                                    .truncate()
                                                    .text_color(muted_fg)
                                                    .child(user_text),
                                            )
                                            .child(div().w(px(72.)).flex_shrink_0().child(
                                                heatmap_cell(
                                                    format_permille(row.cpu_permille),
                                                    HeatmapLevel::from_permille(row.cpu_permille),
                                                    &semantic,
                                                ),
                                            ))
                                            .child(
                                                div()
                                                    .w(px(92.))
                                                    .flex_shrink_0()
                                                    .text_right()
                                                    .font_family(mono_font.clone())
                                                    .child(format_bytes(
                                                        row.private_working_set_bytes,
                                                    )),
                                            )
                                            .child(
                                                div()
                                                    .w(px(64.))
                                                    .flex_shrink_0()
                                                    .text_right()
                                                    .font_family(mono_font.clone())
                                                    .text_color(muted_fg)
                                                    .child(row.thread_count.to_string()),
                                            )
                                            .child(
                                                div()
                                                    .w(px(64.))
                                                    .flex_shrink_0()
                                                    .text_right()
                                                    .font_family(mono_font.clone())
                                                    .text_color(muted_fg)
                                                    .child(row.handle_count.to_string()),
                                            )
                                            .child(
                                                div()
                                                    .w(px(90.))
                                                    .flex_shrink_0()
                                                    .truncate()
                                                    .text_color(muted_fg)
                                                    .child(i18n::process_priority_label(
                                                        row.priority,
                                                    )),
                                            )
                                            .child(
                                                div()
                                                    .w(px(64.))
                                                    .flex_shrink_0()
                                                    .text_color(muted_fg)
                                                    .child(i18n::process_arch_label(row.arch)),
                                            )
                                            .into_any_element(),
                                    )
                                })
                                .collect()
                        },
                    )
                    .size_full()
                    .track_scroll(&self.scroll_handle),
                )
                .vertical_scrollbar(&self.scroll_handle)
                .into_any_element()
        };

        v_flex()
            .size_full()
            .min_h_0()
            .child(header)
            .child(column_header)
            .child(body)
    }
}
