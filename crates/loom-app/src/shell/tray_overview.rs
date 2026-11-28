//! Lightweight `0% GPU` system tray Quick Overview popover view.
//!
//! Shares the exact same [`UiStore`] / `ArcSwap<AppSnapshot>` as [`super::AppShell`]
//! (zero duplicate sampling) and destroys its GPUI window immediately when closed,
//! focus-lost, dismissed via `Esc`, or when transitioning to the main window.

use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement, KeyDownEvent,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Subscription, Window,
    div, px,
};
use loom_types::{AppSnapshot, MonitorDomain, ProcessKey, ProcessRowKind};
use loom_ui_kit::{
    LoomSemanticColors, format_bytes, format_network_rate_mbps, format_permille,
    format_rate_bytes_per_sec,
};

use crate::bootstrap;
use crate::bridge::{ScopedMonitorLease, UiStore, UiStoreEvent};
use crate::i18n;

/// Default logical width of the tray Quick Overview popover window.
pub(crate) const TRAY_OVERVIEW_WIDTH: f32 = 380.0;
/// Default logical height of the tray Quick Overview popover window.
pub(crate) const TRAY_OVERVIEW_HEIGHT: f32 = 440.0;

/// Maximum number of top CPU-consuming processes displayed in the Quick Overview popover.
const MAX_TOP_PROCESSES: usize = 5;

/// Compact live process entry shown in the tray Quick Overview top-process list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TopProcessItem {
    /// Verified lifecycle key used to jump directly to Details on click.
    pub(crate) key: ProcessKey,
    /// Friendly display name (falling back to raw executable image name).
    pub(crate) display_name: Arc<str>,
    /// Numeric process identifier.
    pub(crate) pid: u32,
    /// Live CPU utilization in permille (`0..=1000`).
    pub(crate) cpu_permille: u16,
    /// Live private working set memory footprint in bytes.
    pub(crate) memory_bytes: u64,
}

/// Extracts up to `limit` busiest top-level processes ordered by CPU utilization descending
/// (breaking ties by memory usage descending, then PID ascending).
pub(crate) fn top_busy_processes(snapshot: &AppSnapshot, limit: usize) -> Vec<TopProcessItem> {
    if limit == 0 {
        return Vec::new();
    }

    let mut items: Vec<TopProcessItem> = snapshot
        .processes
        .rows
        .iter()
        .filter(|row| row.kind == ProcessRowKind::Process && row.depth == 0)
        .map(|row| {
            let display_name = if row.display_name.trim().is_empty() {
                Arc::clone(&row.name)
            } else {
                Arc::clone(&row.display_name)
            };
            TopProcessItem {
                key: row.key,
                display_name,
                pid: row.key.pid,
                cpu_permille: row.cpu_permille,
                memory_bytes: row.memory_bytes,
            }
        })
        .collect();

    items.sort_by(|a, b| {
        b.cpu_permille
            .cmp(&a.cpu_permille)
            .then_with(|| b.memory_bytes.cmp(&a.memory_bytes))
            .then_with(|| a.pid.cmp(&b.pid))
    });
    items.truncate(limit);
    items
}

/// Compact frameless popover view displaying live hardware vitals and top 5 CPU processes.
pub(crate) struct TrayOverviewView {
    store: Entity<UiStore>,
    focus_handle: FocusHandle,
    has_been_active: bool,
    _perf_lease: ScopedMonitorLease,
    _subscriptions: Vec<Subscription>,
}

impl TrayOverviewView {
    /// Constructs a new [`TrayOverviewView`] sharing `store` and holding a scoped
    /// `Performance` lease strictly for the lifetime of this popover window.
    pub(crate) fn new(store: Entity<UiStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        window.set_window_title(&i18n::tr("tray_overview.title"));

        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);

        let perf_lease = store.read(cx).acquire_lease(MonitorDomain::Performance);

        let mut subscriptions = Vec::with_capacity(2);
        subscriptions.push(
            cx.subscribe(
                &store,
                |_this, _store, event: &UiStoreEvent, cx| match event {
                    UiStoreEvent::SnapshotUpdated(_) | UiStoreEvent::SettingsChanged => {
                        cx.notify();
                    }
                    _ => {}
                },
            ),
        );

        // Destroy the popover window immediately on focus loss to guarantee 0% GPU dormancy.
        let has_been_active = window.is_window_active();
        subscriptions.push(cx.observe_window_activation(window, |this, window, _cx| {
            if window.is_window_active() {
                this.has_been_active = true;
            } else if this.has_been_active {
                window.remove_window();
            }
        }));

        Self {
            store,
            focus_handle,
            has_been_active,
            _perf_lease: perf_lease,
            _subscriptions: subscriptions,
        }
    }

    fn open_main_window(&self, jump_key: Option<ProcessKey>, window: &mut Window, cx: &mut App) {
        window.remove_window();
        cx.defer(move |cx| {
            bootstrap::open_main_window_from_tray_overview(jump_key, cx);
        });
    }
}

impl Render for TrayOverviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.store.read(cx).snapshot().clone();
        let perf = &snapshot.performance;
        let procs = &snapshot.processes;

        let cpu_permille = if perf.revision > 0 {
            perf.latest.cpu_total_permille
        } else {
            procs.total_cpu_permille
        };

        let mem_used = perf.latest.mem_used_bytes;
        let mem_total = perf.latest.mem_total_bytes;
        let mem_permille = if mem_total > 0 {
            perf.latest.mem_util_permille()
        } else {
            procs.total_memory_permille
        };
        let mem_detail = if mem_total > 0 {
            format!("{} / {}", format_bytes(mem_used), format_bytes(mem_total))
        } else {
            format_permille(mem_permille)
        };

        let disk_rate = if perf.revision > 0 {
            perf.latest
                .disk_read_bytes_per_sec
                .saturating_add(perf.latest.disk_write_bytes_per_sec)
        } else {
            procs.total_disk_bytes_per_sec
        };

        let net_rate = if perf.revision > 0 {
            perf.latest
                .net_recv_bytes_per_sec
                .saturating_add(perf.latest.net_sent_bytes_per_sec)
        } else {
            procs.total_net_bytes_per_sec
        };

        let gpu_permille = if perf.revision > 0 {
            perf.latest.gpu_util_permille
        } else {
            procs.total_gpu_permille
        };

        let top_processes = top_busy_processes(&snapshot, MAX_TOP_PROCESSES);

        let theme = cx.theme();
        let colors = LoomSemanticColors::from_theme(theme);
        let bg = theme.popover;
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let card_bg = theme.secondary.opacity(0.45);
        let border = theme.border;
        let radius = theme.radius;
        let hover_bg = theme.list_hover;

        v_flex()
            .id("tray-overview-root")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(bg)
            .text_color(fg)
            .border_1()
            .border_color(border)
            .rounded(radius)
            .p_3()
            .gap_3()
            .on_key_down(cx.listener(|_this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    window.remove_window();
                    cx.stop_propagation();
                }
            }))
            .child(
                // Compact popover header
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Icon::new(gpui_kit::assets::IconName::Activity)
                                    .small()
                                    .text_color(colors.chart_cpu),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .text_color(fg)
                                    .child(i18n::tr("tray_overview.title")),
                            ),
                    )
                    .child(
                        Button::new("tray-overview-close")
                            .xsmall()
                            .ghost()
                            .icon(IconName::Close)
                            .tooltip(i18n::tr("common.close"))
                            .on_click(|_, window, _| {
                                window.remove_window();
                            }),
                    ),
            )
            .child(
                // Primary CPU & Memory summary cards
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .p_2p5()
                            .rounded(radius)
                            .bg(card_bg)
                            .border_1()
                            .border_color(border.opacity(0.6))
                            .gap_1()
                            .child(
                                h_flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted_fg)
                                            .child(i18n::tr("perf.cpu")),
                                    )
                                    .child(
                                        Icon::new(IconName::Cpu)
                                            .xsmall()
                                            .text_color(colors.chart_cpu),
                                    ),
                            )
                            .child(
                                div()
                                    .text_base()
                                    .font_semibold()
                                    .text_color(fg)
                                    .child(format_permille(cpu_permille)),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .p_2p5()
                            .rounded(radius)
                            .bg(card_bg)
                            .border_1()
                            .border_color(border.opacity(0.6))
                            .gap_1()
                            .child(
                                h_flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted_fg)
                                            .child(i18n::tr("perf.memory")),
                                    )
                                    .child(
                                        Icon::new(IconName::MemoryStick)
                                            .xsmall()
                                            .text_color(colors.chart_memory),
                                    ),
                            )
                            .child(
                                div()
                                    .text_base()
                                    .font_semibold()
                                    .text_color(fg)
                                    .child(format_permille(mem_permille)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted_fg)
                                    .truncate()
                                    .child(mem_detail),
                            ),
                    ),
            )
            .child(
                // Compact Disk / Network / GPU vitals strip
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(vital_mini_tile(
                        i18n::tr("processes.col_disk"),
                        format_rate_bytes_per_sec(disk_rate),
                        colors.chart_disk,
                        card_bg,
                        border,
                        muted_fg,
                        fg,
                        radius,
                    ))
                    .child(vital_mini_tile(
                        i18n::tr("processes.col_network"),
                        format_network_rate_mbps(net_rate),
                        colors.chart_network,
                        card_bg,
                        border,
                        muted_fg,
                        fg,
                        radius,
                    ))
                    .child(vital_mini_tile(
                        i18n::tr("processes.col_gpu"),
                        format_permille(gpu_permille),
                        colors.chart_gpu,
                        card_bg,
                        border,
                        muted_fg,
                        fg,
                        radius,
                    )),
            )
            .child(
                // Top 5 busiest processes list
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .gap_1p5()
                    .child(
                        div()
                            .text_xs()
                            .font_medium()
                            .text_color(muted_fg)
                            .child(i18n::tr("tray_overview.top_processes")),
                    )
                    .child(if top_processes.is_empty() {
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_xs()
                            .text_color(muted_fg)
                            .child(i18n::tr("tray_overview.empty_processes"))
                    } else {
                        v_flex().w_full().gap_1().children(
                            top_processes.into_iter().enumerate().map(|(ix, item)| {
                                let key = item.key;
                                h_flex()
                                    .id(("tray-overview-proc", ix))
                                    .w_full()
                                    .items_center()
                                    .justify_between()
                                    .gap_2()
                                    .px_2()
                                    .py_1p5()
                                    .rounded(radius)
                                    .cursor_pointer()
                                    .hover(move |s| s.bg(hover_bg))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_main_window(Some(key), window, cx);
                                    }))
                                    .child(
                                        h_flex()
                                            .flex_1()
                                            .min_w_0()
                                            .items_center()
                                            .gap_1p5()
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .text_xs()
                                                    .font_medium()
                                                    .text_color(fg)
                                                    .truncate()
                                                    .child(item.display_name.to_string()),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(muted_fg)
                                                    .flex_shrink_0()
                                                    .child(format!("({})", item.pid)),
                                            ),
                                    )
                                    .child(
                                        h_flex()
                                            .items_center()
                                            .gap_2()
                                            .flex_shrink_0()
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(muted_fg)
                                                    .child(format_bytes(item.memory_bytes)),
                                            )
                                            .child(
                                                div()
                                                    .w(px(46.))
                                                    .text_right()
                                                    .text_xs()
                                                    .font_semibold()
                                                    .text_color(colors.chart_cpu)
                                                    .child(format_permille(item.cpu_permille)),
                                            ),
                                    )
                            }),
                        )
                    }),
            )
            .child(
                // Footer action bar
                h_flex()
                    .w_full()
                    .pt_2()
                    .border_t_1()
                    .border_color(border.opacity(0.6))
                    .items_center()
                    .justify_end()
                    .child(
                        Button::new("tray-overview-open-main")
                            .small()
                            .primary()
                            .icon(IconName::ExternalLink)
                            .label(i18n::tr("tray_overview.open_loom"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_main_window(None, window, cx);
                            })),
                    ),
            )
    }
}

#[allow(clippy::too_many_arguments)]
fn vital_mini_tile(
    label: String,
    value: String,
    accent: gpui_kit::Hsla,
    bg: gpui_kit::Hsla,
    border: gpui_kit::Hsla,
    muted_fg: gpui_kit::Hsla,
    fg: gpui_kit::Hsla,
    radius: gpui_kit::Pixels,
) -> impl IntoElement {
    v_flex()
        .flex_1()
        .min_w_0()
        .px_2()
        .py_1p5()
        .rounded(radius)
        .bg(bg)
        .border_1()
        .border_color(border.opacity(0.6))
        .gap_0p5()
        .child(
            h_flex()
                .items_center()
                .gap_1()
                .child(div().size(px(6.)).rounded_full().bg(accent))
                .child(div().text_xs().text_color(muted_fg).truncate().child(label)),
        )
        .child(
            div()
                .text_xs()
                .font_semibold()
                .text_color(fg)
                .truncate()
                .child(value),
        )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use loom_types::{
        AppSnapshot, ProcessCategory, ProcessKey, ProcessRowKind, ProcessRowProjection,
        ProcessTableSlice,
    };

    use super::top_busy_processes;

    fn mock_row(pid: u32, name: &str, cpu: u16, mem: u64, depth: u16) -> ProcessRowProjection {
        ProcessRowProjection {
            kind: ProcessRowKind::Process,
            key: ProcessKey::new(pid, u64::from(pid) * 10),
            depth,
            has_children: false,
            is_expanded: false,
            child_count: 0,
            name: Arc::from(name),
            display_name: Arc::from(name),
            window_title: None,
            publisher: None,
            exe_path: None,
            command_line: None,
            icon: None,
            category: ProcessCategory::App,
            status: Default::default(),
            cpu_permille: cpu,
            memory_bytes: mem,
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
        }
    }

    #[test]
    fn top_busy_processes_filters_headers_and_children_and_sorts_by_cpu_then_memory() {
        let mut header = mock_row(0, "Apps", 999, 999, 0);
        header.kind = ProcessRowKind::SectionHeader {
            category: ProcessCategory::App,
            count: 6,
        };

        let snapshot = AppSnapshot {
            processes: Arc::new(ProcessTableSlice {
                rows: vec![
                    header,
                    mock_row(10, "idle.exe", 5, 100, 0),
                    mock_row(20, "child_dup.exe", 950, 999, 1), // child depth > 0 skipped
                    mock_row(30, "compiler.exe", 420, 500, 0),
                    mock_row(40, "browser.exe", 250, 1200, 0),
                    mock_row(50, "editor.exe", 250, 2400, 0), // tie on CPU -> higher mem wins
                    mock_row(60, "video.exe", 180, 800, 0),
                    mock_row(70, "indexer.exe", 90, 300, 0),
                ],
                ..ProcessTableSlice::default()
            }),
            ..AppSnapshot::default()
        };

        let top5 = top_busy_processes(&snapshot, 5);
        assert_eq!(top5.len(), 5);
        let pids: Vec<u32> = top5.iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![30, 50, 40, 60, 70]);
        assert_eq!(top5[0].display_name.as_ref(), "compiler.exe");
        assert_eq!(top5[1].display_name.as_ref(), "editor.exe");
        assert!(top_busy_processes(&snapshot, 0).is_empty());
    }
}
