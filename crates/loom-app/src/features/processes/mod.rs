//! Live-data Processes overview vertical feature slice.
//!
//! Renders real live OS processes from [`UiStore`] with virtualized scrolling,
//! Grouped / Tree view mode switching, `loom-ui-kit` heatmap cells and status badges,
//! and cross-page navigation to Details via [`ShellHandle`].

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::{
    Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, UniformListScrollHandle, Window, div,
    prelude::FluentBuilder as _, px, uniform_list,
};
use loom_types::{ProcessRowKind, ProcessViewMode};
use loom_ui_kit::{
    HeatmapLevel, LoomSemanticColors, StatusLabels, format_disk_cell, format_memory_cell,
    format_network_cell, format_permille, heatmap_cell, status_badges,
};

use crate::bridge::{UiCommands, UiStore, UiStoreEvent};
use crate::i18n;
use crate::shell::ShellHandle;

const PROCESS_ROW_HEIGHT: f32 = 30.0;

/// Live Processes overview view.
pub(crate) struct ProcessesView {
    store: Entity<UiStore>,
    shell: ShellHandle,
    last_processes_rev: u64,
    scroll_handle: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl ProcessesView {
    /// Creates a new [`ProcessesView`] subscribed to [`UiStore`] snapshot changes.
    pub(crate) fn new(store: Entity<UiStore>, shell: ShellHandle, cx: &mut Context<Self>) -> Self {
        let initial_rev = store.read(cx).snapshot().revisions.processes;
        let sub = cx.subscribe(&store, |this, _store, event, cx| match event {
            UiStoreEvent::SnapshotUpdated(revs) => {
                if revs.processes != this.last_processes_rev {
                    this.last_processes_rev = revs.processes;
                    cx.notify();
                }
            }
            UiStoreEvent::SettingsChanged => cx.notify(),
            UiStoreEvent::ElevationRequired { .. } | UiStoreEvent::CommandFailed { .. } => {}
        });

        Self {
            store,
            shell,
            last_processes_rev: initial_rev,
            scroll_handle: UniformListScrollHandle::new(),
            _subscriptions: vec![sub],
        }
    }
}

impl Render for ProcessesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store_ref = self.store.read(cx);
        let snapshot = store_ref.snapshot().clone();
        let settings = store_ref.settings().clone();
        let slice = snapshot.processes.clone();
        let view_mode = slice.view_mode;
        let row_count = slice.rows.len();

        let theme = cx.theme();
        let border = theme.border;
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let muted_bg = theme.muted;
        let secondary_bg = theme.secondary;
        let primary = theme.primary;
        let radius = theme.radius;
        let mono_font = theme.mono_font_family.clone();
        let semantic = LoomSemanticColors::from_theme(theme);

        let store_grouped = self.store.clone();
        let store_tree = self.store.clone();
        let store_refresh = self.store.clone();

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
                            .child(i18n::tr("processes.title")),
                    )
                    .child(metric_pill(
                        i18n::tr("processes.count_label"),
                        slice.total_process_count.to_string(),
                        muted_bg,
                        muted_fg,
                        fg,
                        radius,
                    ))
                    .child(metric_pill(
                        i18n::tr("processes.cpu_label"),
                        format_permille(slice.total_cpu_permille),
                        muted_bg,
                        muted_fg,
                        fg,
                        radius,
                    ))
                    .child(metric_pill(
                        i18n::tr("processes.memory_label"),
                        format_permille(slice.total_memory_permille),
                        muted_bg,
                        muted_fg,
                        fg,
                        radius,
                    )),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("processes-mode-grouped")
                            .small()
                            .label(i18n::tr("processes.view_grouped"))
                            .map(|btn| {
                                if view_mode == ProcessViewMode::Grouped {
                                    btn.primary()
                                } else {
                                    btn.ghost()
                                }
                            })
                            .on_click(move |_, _, cx| {
                                UiCommands::set_process_view_mode(
                                    &store_grouped,
                                    ProcessViewMode::Grouped,
                                    cx,
                                );
                            }),
                    )
                    .child(
                        Button::new("processes-mode-tree")
                            .small()
                            .label(i18n::tr("processes.view_tree"))
                            .map(|btn| {
                                if view_mode == ProcessViewMode::Tree {
                                    btn.primary()
                                } else {
                                    btn.ghost()
                                }
                            })
                            .on_click(move |_, _, cx| {
                                UiCommands::set_process_view_mode(
                                    &store_tree,
                                    ProcessViewMode::Tree,
                                    cx,
                                );
                            }),
                    )
                    .child(
                        Button::new("processes-refresh")
                            .small()
                            .ghost()
                            .icon(IconName::RefreshCw)
                            .label(i18n::tr("processes.refresh"))
                            .on_click(move |_, _, cx| {
                                UiCommands::force_refresh(&store_refresh, cx);
                            }),
                    ),
            );

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
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(i18n::tr("processes.col_name")),
            )
            .child(
                div()
                    .w(px(124.))
                    .flex_shrink_0()
                    .child(i18n::tr("processes.col_status")),
            )
            .child(
                div()
                    .w(px(82.))
                    .flex_shrink_0()
                    .text_right()
                    .child(i18n::tr("processes.col_cpu")),
            )
            .child(
                div()
                    .w(px(100.))
                    .flex_shrink_0()
                    .text_right()
                    .child(i18n::tr("processes.col_memory")),
            )
            .child(
                div()
                    .w(px(92.))
                    .flex_shrink_0()
                    .text_right()
                    .child(i18n::tr("processes.col_disk")),
            )
            .child(
                div()
                    .w(px(92.))
                    .flex_shrink_0()
                    .text_right()
                    .child(i18n::tr("processes.col_network")),
            )
            .child(
                div()
                    .w(px(64.))
                    .flex_shrink_0()
                    .text_right()
                    .child(i18n::tr("processes.col_pid")),
            )
            .child(div().w(px(32.)).flex_shrink_0());

        let body = if row_count == 0 {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .p_8()
                .child(
                    Icon::new(IconName::Search)
                        .size(px(24.))
                        .text_color(muted_fg),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(muted_fg)
                        .child(i18n::tr("processes.empty_search")),
                )
                .into_any_element()
        } else {
            let store_for_rows = self.store.clone();
            let shell_for_rows = self.shell.clone();
            let mem_mode = settings.memory_display_mode;
            let disk_mode = settings.disk_display_mode;
            let net_mode = settings.network_display_mode;
            let status_labels = StatusLabels::new(
                i18n::tr("processes.status_efficiency"),
                i18n::tr("processes.status_suspended"),
                i18n::tr("processes.status_not_responding"),
            );
            let jump_label = i18n::tr("processes.jump_to_details");

            div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(
                    uniform_list(
                        "processes-virtual-list",
                        row_count,
                        move |range, _window, cx| {
                            let theme = cx.theme();
                            let row_border = theme.border.opacity(0.45);
                            let row_hover = theme.accent.opacity(0.5);
                            let section_bg = theme.muted.opacity(0.6);

                            range
                                .filter_map(|ix| {
                                    let row = slice.rows.get(ix)?;
                                    match row.kind {
                                        ProcessRowKind::SectionHeader { category, count } => {
                                            let label = format!(
                                                "{} ({count})",
                                                i18n::process_category_label(category)
                                            );
                                            Some(
                                                h_flex()
                                                    .id(("proc-section", ix))
                                                    .w_full()
                                                    .h(px(PROCESS_ROW_HEIGHT))
                                                    .items_center()
                                                    .px_4()
                                                    .bg(section_bg)
                                                    .border_b_1()
                                                    .border_color(row_border)
                                                    .text_xs()
                                                    .font_semibold()
                                                    .text_color(primary)
                                                    .child(label)
                                                    .into_any_element(),
                                            )
                                        }
                                        ProcessRowKind::Process => {
                                            let key = row.key;
                                            let indent = f32::from(row.depth.min(8)) * 16.0;
                                            let name_text: SharedString = if row.child_count > 0 {
                                                format!(
                                                    "{} ({})",
                                                    row.display_name, row.child_count
                                                )
                                                .into()
                                            } else {
                                                row.display_name.to_string().into()
                                            };
                                            let cpu_text = format_permille(row.cpu_permille);
                                            let mem_text = format_memory_cell(
                                                row.memory_bytes,
                                                row.memory_permille,
                                                mem_mode,
                                            );
                                            let disk_text = format_disk_cell(
                                                row.disk_bytes_per_sec,
                                                0,
                                                disk_mode,
                                            );
                                            let net_text = format_network_cell(
                                                row.net_bytes_per_sec,
                                                0,
                                                net_mode,
                                            );

                                            let toggle_store = store_for_rows.clone();
                                            let jump_shell = shell_for_rows.clone();
                                            let has_children = row.has_children;
                                            let is_expanded = row.is_expanded;

                                            Some(
                                                h_flex()
                                                    .id(("proc-row", key.pid as usize))
                                                    .w_full()
                                                    .h(px(PROCESS_ROW_HEIGHT))
                                                    .items_center()
                                                    .px_4()
                                                    .gap_2()
                                                    .border_b_1()
                                                    .border_color(row_border)
                                                    .text_xs()
                                                    .text_color(fg)
                                                    .hover(|s| s.bg(row_hover))
                                                    .child(
                                                        h_flex()
                                                            .flex_1()
                                                            .min_w_0()
                                                            .items_center()
                                                            .gap_1()
                                                            .pl(px(indent))
                                                            .child(if has_children {
                                                                Button::new((
                                                                "proc-expand",
                                                                key.pid as usize,
                                                            ))
                                                            .xsmall()
                                                            .ghost()
                                                            .icon(if is_expanded {
                                                                IconName::ChevronDown
                                                            } else {
                                                                IconName::ChevronRight
                                                            })
                                                            .on_click(move |_, _, cx| {
                                                                UiCommands::toggle_process_expanded(
                                                                    &toggle_store,
                                                                    key,
                                                                    cx,
                                                                );
                                                            })
                                                            .into_any_element()
                                                            } else {
                                                                div()
                                                                    .w(px(20.))
                                                                    .flex_shrink_0()
                                                                    .into_any_element()
                                                            })
                                                            .child(
                                                                div()
                                                                    .flex_1()
                                                                    .min_w_0()
                                                                    .truncate()
                                                                    .child(name_text),
                                                            ),
                                                    )
                                                    .child(
                                                        div()
                                                            .w(px(124.))
                                                            .flex_shrink_0()
                                                            .overflow_hidden()
                                                            .child(status_badges(
                                                                row.status,
                                                                &status_labels,
                                                                &semantic,
                                                            )),
                                                    )
                                                    .child(div().w(px(82.)).flex_shrink_0().child(
                                                        heatmap_cell(
                                                            cpu_text,
                                                            HeatmapLevel::from_permille(
                                                                row.cpu_permille,
                                                            ),
                                                            &semantic,
                                                        ),
                                                    ))
                                                    .child(div().w(px(100.)).flex_shrink_0().child(
                                                        heatmap_cell(
                                                            mem_text,
                                                            HeatmapLevel::from_permille(
                                                                row.memory_permille,
                                                            ),
                                                            &semantic,
                                                        ),
                                                    ))
                                                    .child(div().w(px(92.)).flex_shrink_0().child(
                                                        heatmap_cell(
                                                            disk_text,
                                                            HeatmapLevel::from_bytes_per_sec(
                                                                row.disk_bytes_per_sec,
                                                            ),
                                                            &semantic,
                                                        ),
                                                    ))
                                                    .child(div().w(px(92.)).flex_shrink_0().child(
                                                        heatmap_cell(
                                                            net_text,
                                                            HeatmapLevel::from_bytes_per_sec(
                                                                row.net_bytes_per_sec,
                                                            ),
                                                            &semantic,
                                                        ),
                                                    ))
                                                    .child(
                                                        div()
                                                            .w(px(64.))
                                                            .flex_shrink_0()
                                                            .text_right()
                                                            .font_family(mono_font.clone())
                                                            .text_color(muted_fg)
                                                            .child(key.pid.to_string()),
                                                    )
                                                    .child(
                                                        div().w(px(32.)).flex_shrink_0().child(
                                                            Button::new((
                                                                "proc-jump-details",
                                                                key.pid as usize,
                                                            ))
                                                            .xsmall()
                                                            .ghost()
                                                            .icon(IconName::ArrowRight)
                                                            .tooltip(jump_label.clone())
                                                            .on_click(move |_, window, cx| {
                                                                jump_shell.jump_to_details(
                                                                    key, window, cx,
                                                                );
                                                            }),
                                                        ),
                                                    )
                                                    .into_any_element(),
                                            )
                                        }
                                    }
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

fn metric_pill(
    label: String,
    value: String,
    bg: gpui_kit::Hsla,
    label_fg: gpui_kit::Hsla,
    value_fg: gpui_kit::Hsla,
    radius: gpui_kit::Pixels,
) -> impl IntoElement {
    h_flex()
        .items_center()
        .gap_1p5()
        .px_2p5()
        .py_1()
        .rounded(radius)
        .bg(bg.opacity(0.65))
        .text_xs()
        .child(div().text_color(label_fg).child(label))
        .child(div().font_semibold().text_color(value_fg).child(value))
}
