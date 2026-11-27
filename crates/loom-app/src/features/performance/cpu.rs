//! CPU detail pane for the Performance view (overall vs logical processors + kernel time overlay).

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::{Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::{
    Context, InteractiveElement as _, IntoElement, ParentElement as _, SharedString, Styled as _,
    div, prelude::FluentBuilder as _, px,
};
use loom_types::PerformanceSlice;
use loom_ui_kit::{
    LoomSemanticColors, format_bytes, format_freq_ghz, format_permille, format_uptime,
};

use crate::features::performance::PerformanceView;
use crate::features::performance::charts::{
    MetricStat, SpecRow, build_dual_percent_area_chart, build_percent_area_chart, chart_frame,
    device_header, dual_history_rings_to_points, history_ring_to_points, legend_swatch,
    metrics_two_column_panel,
};
use crate::features::performance::state::{CpuGraphMode, logical_cpu_grid_columns};
use crate::i18n;

/// Renders the CPU hardware detail pane.
pub(crate) fn render_cpu_detail(
    perf: &PerformanceSlice,
    mode: CpuGraphMode,
    show_kernel_times: bool,
    colors: &LoomSemanticColors,
    cx: &mut Context<PerformanceView>,
) -> impl IntoElement {
    let latest = &perf.latest;
    let info = &perf.cpu_info;
    let unavailable = i18n::tr("perf.unavailable");

    let brand: SharedString = if info.brand_name.is_empty() {
        unavailable.clone().into()
    } else {
        SharedString::from(info.brand_name.as_ref().to_owned())
    };

    let overall_btn = Button::new("perf-cpu-mode-overall")
        .small()
        .label(i18n::tr("perf.cpu_overall"))
        .map(|btn| {
            if mode == CpuGraphMode::Overall {
                btn.primary()
            } else {
                btn.outline()
            }
        })
        .on_click(cx.listener(|view, _, _window, cx| {
            view.set_cpu_graph_mode(CpuGraphMode::Overall, cx);
        }));

    let logical_btn = Button::new("perf-cpu-mode-logical")
        .small()
        .label(i18n::tr("perf.cpu_logical"))
        .map(|btn| {
            if mode == CpuGraphMode::LogicalProcessors {
                btn.primary()
            } else {
                btn.outline()
            }
        })
        .on_click(cx.listener(|view, _, _window, cx| {
            view.set_cpu_graph_mode(CpuGraphMode::LogicalProcessors, cx);
        }));

    let kernel_checkbox = Checkbox::new("perf-cpu-show-kernel-times")
        .small()
        .checked(show_kernel_times)
        .label(i18n::tr("perf.cpu_show_kernel"))
        .on_change(cx.listener(|view, checked, _window, cx| {
            view.set_show_kernel_times(*checked, cx);
        }));

    let controls_bar = h_flex()
        .w_full()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_3()
        .child(
            h_flex()
                .items_center()
                .gap_1p5()
                .child(overall_btn)
                .child(logical_btn),
        )
        .child(kernel_checkbox);

    let weak_ctx = cx.entity().downgrade();
    let chart_area = div()
        .id("perf-cpu-chart-surface")
        .w_full()
        .child(match mode {
            CpuGraphMode::Overall => {
                render_overall_cpu_chart(perf, show_kernel_times, colors).into_any_element()
            }
            CpuGraphMode::LogicalProcessors => {
                render_logical_cpu_grid(perf, colors).into_any_element()
            }
        })
        .context_menu(move |menu, _window, _cx| {
            let weak_overall = weak_ctx.clone();
            let weak_logical = weak_ctx.clone();
            let weak_kernel = weak_ctx.clone();
            menu.item(
                PopupMenuItem::new(i18n::tr("perf.cpu_overall"))
                    .checked(mode == CpuGraphMode::Overall)
                    .on_click(move |_, _, cx| {
                        let _ = weak_overall.update(cx, |view, cx| {
                            view.set_cpu_graph_mode(CpuGraphMode::Overall, cx);
                        });
                    }),
            )
            .item(
                PopupMenuItem::new(i18n::tr("perf.cpu_logical"))
                    .checked(mode == CpuGraphMode::LogicalProcessors)
                    .on_click(move |_, _, cx| {
                        let _ = weak_logical.update(cx, |view, cx| {
                            view.set_cpu_graph_mode(CpuGraphMode::LogicalProcessors, cx);
                        });
                    }),
            )
            .separator()
            .item(
                PopupMenuItem::new(i18n::tr("perf.cpu_show_kernel"))
                    .checked(show_kernel_times)
                    .on_click(move |_, _, cx| {
                        let _ = weak_kernel.update(cx, |view, cx| {
                            view.set_show_kernel_times(!show_kernel_times, cx);
                        });
                    }),
            )
        });

    let speed_text = if latest.cpu_freq_mhz > 0 {
        format_freq_ghz(latest.cpu_freq_mhz)
    } else {
        unavailable.clone()
    };

    let base_mhz = if info.base_freq_mhz > 0 {
        info.base_freq_mhz
    } else {
        latest.cpu_base_freq_mhz
    };
    let base_speed_text = if base_mhz > 0 {
        format_freq_ghz(base_mhz)
    } else {
        unavailable.clone()
    };

    let sockets_text = if info.sockets > 0 {
        info.sockets.to_string()
    } else {
        unavailable.clone()
    };

    let cores_count = if info.physical_cores > 0 {
        info.physical_cores
    } else {
        latest.physical_core_count
    };
    let cores_text = if cores_count > 0 {
        cores_count.to_string()
    } else {
        unavailable.clone()
    };

    let logical_count = if info.logical_processors > 0 {
        info.logical_processors
    } else {
        latest.logical_cpu_count
    };
    let logical_text = if logical_count > 0 {
        logical_count.to_string()
    } else {
        unavailable.clone()
    };

    let virt_text = match info.virtualization_enabled {
        Some(true) => i18n::tr("perf.enabled"),
        Some(false) => i18n::tr("perf.disabled"),
        None => unavailable.clone(),
    };

    let format_cache = |bytes: u64| {
        if bytes > 0 {
            format_bytes(bytes)
        } else {
            unavailable.clone()
        }
    };

    let left_stats = vec![
        MetricStat::new(
            i18n::tr("perf.cpu_utilization"),
            format_permille(latest.cpu_total_permille),
        ),
        MetricStat::new(i18n::tr("perf.cpu_speed"), speed_text),
        MetricStat::new(
            i18n::tr("perf.cpu_processes"),
            latest.process_count.to_string(),
        ),
        MetricStat::new(
            i18n::tr("perf.cpu_threads"),
            latest.thread_count.to_string(),
        ),
        MetricStat::new(
            i18n::tr("perf.cpu_handles"),
            latest.handle_count.to_string(),
        ),
        MetricStat::new(
            i18n::tr("perf.cpu_uptime"),
            format_uptime(latest.uptime_secs),
        ),
    ];

    let right_specs = vec![
        SpecRow::new(i18n::tr("perf.cpu_base_speed"), base_speed_text),
        SpecRow::new(i18n::tr("perf.cpu_sockets"), sockets_text),
        SpecRow::new(i18n::tr("perf.cpu_cores"), cores_text),
        SpecRow::new(i18n::tr("perf.cpu_logical_processors"), logical_text),
        SpecRow::new(i18n::tr("perf.cpu_virtualization"), virt_text),
        SpecRow::new(
            i18n::tr("perf.cpu_l1_cache"),
            format_cache(info.l1_cache_bytes),
        ),
        SpecRow::new(
            i18n::tr("perf.cpu_l2_cache"),
            format_cache(info.l2_cache_bytes),
        ),
        SpecRow::new(
            i18n::tr("perf.cpu_l3_cache"),
            format_cache(info.l3_cache_bytes),
        ),
    ];

    v_flex()
        .w_full()
        .gap_4()
        .child(device_header(i18n::tr("perf.cpu"), brand, None, colors))
        .child(controls_bar)
        .child(chart_area)
        .child(metrics_two_column_panel(left_stats, right_specs, colors))
}

fn render_overall_cpu_chart(
    perf: &PerformanceSlice,
    show_kernel_times: bool,
    colors: &LoomSemanticColors,
) -> impl IntoElement {
    let util_label = i18n::tr("perf.cpu_utilization");
    if show_kernel_times {
        let kernel_label = i18n::tr("perf.cpu_kernel_time");
        let points = dual_history_rings_to_points(&perf.cpu_history, &perf.cpu_kernel_history);
        let chart = build_dual_percent_area_chart(
            "perf-cpu-overall-dual-chart",
            points,
            util_label.clone(),
            colors.chart_cpu,
            kernel_label.clone(),
            colors.chart_cpu_kernel,
        );
        let legend = vec![
            legend_swatch(util_label, colors.chart_cpu, colors).into_any_element(),
            legend_swatch(kernel_label, colors.chart_cpu_kernel, colors).into_any_element(),
        ];
        chart_frame(
            i18n::tr("perf.cpu_overall"),
            "100%",
            legend,
            colors.chart_cpu,
            240.0,
            chart,
            colors,
        )
    } else {
        let points = history_ring_to_points(&perf.cpu_history);
        let chart = build_percent_area_chart(
            "perf-cpu-overall-chart",
            points,
            util_label,
            colors.chart_cpu,
            true,
        );
        chart_frame(
            i18n::tr("perf.cpu_overall"),
            "100%",
            Vec::new(),
            colors.chart_cpu,
            240.0,
            chart,
            colors,
        )
    }
}

fn render_logical_cpu_grid(
    perf: &PerformanceSlice,
    colors: &LoomSemanticColors,
) -> impl IntoElement {
    let core_count = perf.per_core_history.len();
    if core_count == 0 {
        return v_flex()
            .w_full()
            .h(px(240.))
            .items_center()
            .justify_center()
            .rounded_md()
            .border_1()
            .border_color(colors.card_border)
            .bg(colors.card_bg)
            .child(
                div()
                    .text_sm()
                    .text_color(colors.text_muted)
                    .child(i18n::tr("perf.cpu_sampling_cores")),
            );
    }

    let cols = logical_cpu_grid_columns(core_count);
    let cell_height = if core_count <= 4 {
        118.0
    } else if core_count <= 16 {
        86.0
    } else {
        66.0
    };

    let mut grid = v_flex().w_full().gap_2();
    for (row_ix, chunk) in perf.per_core_history.chunks(cols).enumerate() {
        let mut row = h_flex().w_full().gap_2();
        for (col_ix, ring) in chunk.iter().enumerate() {
            let core_ix = row_ix * cols + col_ix;
            let live_permille = perf
                .per_core_permille
                .get(core_ix)
                .copied()
                .or_else(|| ring.latest())
                .unwrap_or(0);
            let label = i18n::perf_cpu_core_label(core_ix);
            let pct_label = if cols >= 8 {
                format!("{:.0}%", f32::from(live_permille) / 10.0)
            } else {
                format_permille(live_permille)
            };
            let points = history_ring_to_points(ring);
            let chart = build_percent_area_chart(
                SharedString::from(format!("perf-cpu-core-{core_ix}")),
                points,
                label.clone(),
                colors.chart_cpu,
                true,
            );

            row = row.child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.chart_cpu.opacity(0.4))
                    .bg(colors.card_bg)
                    .overflow_hidden()
                    .child(
                        h_flex()
                            .w_full()
                            .px_1p5()
                            .py_0p5()
                            .items_center()
                            .justify_between()
                            .gap_1()
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(if cols >= 8 { 10. } else { 11. }))
                                    .font_medium()
                                    .text_color(colors.text_muted)
                                    .child(label),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(if cols >= 8 { 10. } else { 11. }))
                                    .font_semibold()
                                    .text_color(colors.text_primary)
                                    .child(pct_label),
                            ),
                    )
                    .child(div().w_full().h(px(cell_height)).child(chart)),
            );
        }
        // Pad incomplete trailing row so cell widths remain uniform.
        for _ in chunk.len()..cols {
            row = row.child(div().flex_1().min_w_0());
        }
        grid = grid.child(row);
    }

    grid
}
