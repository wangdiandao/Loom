//! CPU detail pane for the Performance view (overall vs adaptive logical-core grid + P/E/LP-E colors + kernel time overlay).

use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::{
    Context, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    Styled as _, div, px,
};
use loom_types::{CpuCoreKind, HistoryRing, PerformanceSlice};
use loom_ui_kit::{
    CoreCardHeaderMode, CoreGridLayout, LoomSemanticColors, format_bytes, format_freq_ghz,
    format_opt_percent_permille, format_permille, format_uptime,
};

use crate::features::performance::PerformanceView;
use crate::features::performance::cache::PerformanceSeriesCache;
use crate::features::performance::charts::{
    HISTORY_WINDOW, MetricStat, SpecRow, build_dual_percent_area_chart, build_percent_area_chart,
    chart_frame, copy_summary_menu_item, device_header, legend_swatch, metrics_two_column_panel,
    non_empty_arc_shared,
};
use crate::features::performance::state::CpuGraphMode;
use crate::i18n;

/// Returns `true` when `core_kinds` describes a heterogeneous hybrid CPU topology
/// (at least two distinct non-`Unknown` core classes such as `P` + `E` or `P` + `E` + `LP-E`).
pub(crate) fn is_hybrid_cpu_topology(core_kinds: &[CpuCoreKind]) -> bool {
    let mut has_p = false;
    let mut has_e = false;
    let mut has_lpe = false;
    for &kind in core_kinds {
        match kind {
            CpuCoreKind::Performance => has_p = true,
            CpuCoreKind::Efficiency => has_e = true,
            CpuCoreKind::LowPowerEfficiency => has_lpe = true,
            CpuCoreKind::Unknown => {}
        }
    }
    usize::from(has_p) + usize::from(has_e) + usize::from(has_lpe) >= 2
}

/// Maps a logical core's [`CpuCoreKind`] to its semantic accent color when a
/// hybrid CPU topology is active, or falls back to `colors.chart_cpu` on uniform CPUs.
#[inline]
pub(crate) fn core_kind_accent(
    kind: CpuCoreKind,
    is_hybrid: bool,
    colors: &LoomSemanticColors,
) -> Hsla {
    if !is_hybrid {
        return colors.chart_cpu;
    }
    match kind {
        CpuCoreKind::Performance => colors.cpu_core_p,
        CpuCoreKind::Efficiency => colors.cpu_core_e,
        CpuCoreKind::LowPowerEfficiency => colors.cpu_core_lp_e,
        CpuCoreKind::Unknown => colors.chart_cpu,
    }
}

/// Returns the compact badge text (`"P"`, `"E"`, `"LP-E"`) for a classified core kind.
pub(crate) fn core_kind_badge_label(kind: CpuCoreKind) -> Option<String> {
    match kind {
        CpuCoreKind::Performance => Some(i18n::tr("perf.perf_polish_core_p")),
        CpuCoreKind::Efficiency => Some(i18n::tr("perf.perf_polish_core_e")),
        CpuCoreKind::LowPowerEfficiency => Some(i18n::tr("perf.perf_polish_core_lp_e")),
        CpuCoreKind::Unknown => None,
    }
}

/// Renders the CPU hardware detail pane.
pub(crate) fn render_cpu_detail(
    perf: &PerformanceSlice,
    mode: CpuGraphMode,
    show_kernel_times: bool,
    core_grid_layout: CoreGridLayout,
    cache: &mut PerformanceSeriesCache,
    colors: &LoomSemanticColors,
    cx: &mut Context<PerformanceView>,
) -> impl IntoElement {
    let latest = &perf.latest;
    let info = &perf.cpu_info;
    let unavailable = i18n::tr("perf.unavailable");

    let brand = non_empty_arc_shared(&info.brand_name, &unavailable);

    let chart_area = div()
        .id("perf-cpu-chart-surface")
        .w_full()
        .child(match mode {
            CpuGraphMode::Overall => {
                render_overall_cpu_chart(perf, show_kernel_times, cache, colors).into_any_element()
            }
            CpuGraphMode::LogicalProcessors => {
                render_logical_cpu_grid(perf, core_grid_layout, cache, colors).into_any_element()
            }
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

    let weak_ctx = cx.entity().downgrade();
    v_flex()
        .id("perf-cpu-detail")
        .w_full()
        .gap_4()
        .child(device_header(i18n::tr("perf.cpu"), brand, None, colors))
        .child(chart_area)
        .child(metrics_two_column_panel(left_stats, right_specs, colors))
        .context_menu(move |menu, _window, _cx| {
            let weak_overall = weak_ctx.clone();
            let weak_logical = weak_ctx.clone();
            let weak_kernel = weak_ctx.clone();
            let weak_copy = weak_ctx.clone();
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
            .separator()
            .item(copy_summary_menu_item(weak_copy))
        })
}

fn render_overall_cpu_chart(
    perf: &PerformanceSlice,
    show_kernel_times: bool,
    cache: &mut PerformanceSeriesCache,
    colors: &LoomSemanticColors,
) -> impl IntoElement {
    let util_label = i18n::tr("perf.cpu_utilization");
    if show_kernel_times {
        let kernel_label = i18n::tr("perf.cpu_kernel_time");
        let points = cache.cpu_dual(perf).to_vec();
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
        let points = cache.cpu_overall(perf).to_vec();
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

pub(crate) fn render_logical_cpu_grid(
    perf: &PerformanceSlice,
    layout: CoreGridLayout,
    cache: &mut PerformanceSeriesCache,
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

    let cols = layout.columns.max(1);
    let header_mode = layout.header_mode;
    let is_hybrid = is_hybrid_cpu_topology(&perf.cpu_info.core_kinds);

    let plot_height = match header_mode {
        CoreCardHeaderMode::Full => (layout.card_height_px - 32.0).max(32.0),
        CoreCardHeaderMode::Short => (layout.card_height_px - 22.0).max(28.0),
        CoreCardHeaderMode::Hidden => (layout.card_height_px - 4.0).max(28.0),
    };

    let mut grid = v_flex().w_full().gap_2();

    // When a heterogeneous P/E/LP-E topology is detected, show a compact color legend.
    if is_hybrid {
        let has_p = perf.cpu_info.core_kinds.contains(&CpuCoreKind::Performance);
        let has_e = perf.cpu_info.core_kinds.contains(&CpuCoreKind::Efficiency);
        let has_lpe = perf
            .cpu_info
            .core_kinds
            .contains(&CpuCoreKind::LowPowerEfficiency);
        let mut legend_row = h_flex().w_full().flex_wrap().items_center().gap_3();
        if has_p {
            legend_row = legend_row.child(legend_swatch(
                i18n::tr("perf.perf_polish_core_p_legend"),
                colors.cpu_core_p,
                colors,
            ));
        }
        if has_e {
            legend_row = legend_row.child(legend_swatch(
                i18n::tr("perf.perf_polish_core_e_legend"),
                colors.cpu_core_e,
                colors,
            ));
        }
        if has_lpe {
            legend_row = legend_row.child(legend_swatch(
                i18n::tr("perf.perf_polish_core_lp_e_legend"),
                colors.cpu_core_lp_e,
                colors,
            ));
        }
        grid = grid.child(legend_row);
    }

    let cached_cores = cache.cpu_per_core(perf);
    for (row_ix, chunk) in perf.per_core_history.chunks(cols).enumerate() {
        let mut row = h_flex().w_full().gap_2();
        for (col_ix, ring) in chunk.iter().enumerate() {
            let core_ix = row_ix * cols + col_ix;
            let core_kind = perf
                .cpu_info
                .core_kinds
                .get(core_ix)
                .copied()
                .unwrap_or(CpuCoreKind::Unknown);
            let accent = core_kind_accent(core_kind, is_hybrid, colors);
            let badge_label = is_hybrid
                .then(|| core_kind_badge_label(core_kind))
                .flatten();

            let live_permille = perf
                .per_core_permille
                .get(core_ix)
                .copied()
                .filter(|&v| !HistoryRing::<HISTORY_WINDOW>::is_gap(v))
                .or_else(|| {
                    ring.latest()
                        .filter(|&v| !HistoryRing::<HISTORY_WINDOW>::is_gap(v))
                });

            let full_label = i18n::perf_cpu_core_label(core_ix);
            let series_name = match &badge_label {
                Some(tag) => format!("{full_label} ({tag})"),
                None => full_label.clone(),
            };
            let pct_label: SharedString = match (header_mode, live_permille) {
                (CoreCardHeaderMode::Short, Some(p)) => {
                    format!("{:.0}%", f32::from(p) / 10.0).into()
                }
                (_, opt) => format_opt_percent_permille(opt),
            };

            let points = cached_cores.get(core_ix).cloned().unwrap_or_default();
            let chart = build_percent_area_chart(
                SharedString::from(format!("perf-cpu-core-{core_ix}")),
                points,
                series_name,
                accent,
                true,
            );

            let mut card = v_flex()
                .flex_1()
                .min_w_0()
                .rounded_md()
                .border_1()
                .border_color(accent.opacity(0.45))
                .bg(colors.card_bg)
                .overflow_hidden();

            match header_mode {
                CoreCardHeaderMode::Full => {
                    let mut left = h_flex().items_center().gap_1().min_w_0().child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .font_medium()
                            .text_color(colors.text_muted)
                            .child(full_label),
                    );
                    if let Some(tag) = badge_label {
                        left = left.child(
                            div()
                                .px_1()
                                .rounded_sm()
                                .bg(accent.opacity(0.16))
                                .text_size(px(9.5))
                                .font_semibold()
                                .text_color(accent)
                                .child(tag),
                        );
                    }
                    card = card.child(
                        h_flex()
                            .w_full()
                            .px_1p5()
                            .py_0p5()
                            .items_center()
                            .justify_between()
                            .gap_1()
                            .child(left)
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(11.))
                                    .font_semibold()
                                    .text_color(colors.text_primary)
                                    .child(pct_label),
                            ),
                    );
                }
                CoreCardHeaderMode::Short => {
                    let mut left = h_flex().items_center().gap_1().min_w_0().child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(10.))
                            .font_medium()
                            .text_color(colors.text_muted)
                            .child(format!("{core_ix}")),
                    );
                    if let Some(tag) = badge_label {
                        left = left.child(
                            div()
                                .text_size(px(9.))
                                .font_semibold()
                                .text_color(accent)
                                .child(tag),
                        );
                    }
                    card = card.child(
                        h_flex()
                            .w_full()
                            .px_1()
                            .items_center()
                            .justify_between()
                            .gap_1()
                            .child(left)
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(10.))
                                    .font_semibold()
                                    .text_color(colors.text_primary)
                                    .child(pct_label),
                            ),
                    );
                }
                CoreCardHeaderMode::Hidden => {}
            }

            card = card.child(div().w_full().h(px(plot_height)).child(chart));
            row = row.child(card);
        }

        // Pad incomplete trailing row so cell widths remain uniform.
        for _ in chunk.len()..cols {
            row = row.child(div().flex_1().min_w_0());
        }
        grid = grid.child(row);
    }

    grid
}

#[cfg(test)]
mod tests {
    use gpui_kit::component::{Theme, ThemeColor, ThemeMode};
    use loom_types::CpuCoreKind;
    use loom_ui_kit::LoomSemanticColors;

    use super::*;

    #[test]
    fn hybrid_topology_detection_and_core_kind_colors() {
        let mut theme = Theme::from(ThemeColor::dark().as_ref());
        theme.mode = ThemeMode::Dark;
        let colors = LoomSemanticColors::from_theme(&theme);

        // Uniform / empty topologies are not hybrid.
        assert!(!is_hybrid_cpu_topology(&[]));
        assert!(!is_hybrid_cpu_topology(&[CpuCoreKind::Unknown; 8]));
        assert!(!is_hybrid_cpu_topology(&[CpuCoreKind::Performance; 8]));

        // Hybrid P+E and P+E+LP-E topologies are detected.
        assert!(is_hybrid_cpu_topology(&[
            CpuCoreKind::Performance,
            CpuCoreKind::Efficiency,
        ]));
        assert!(is_hybrid_cpu_topology(&[
            CpuCoreKind::Performance,
            CpuCoreKind::Efficiency,
            CpuCoreKind::LowPowerEfficiency,
        ]));

        // Color mapping distinguishes P, E, and LP-E on hybrid CPUs and falls back on uniform CPUs.
        assert_eq!(
            core_kind_accent(CpuCoreKind::Performance, true, &colors),
            colors.cpu_core_p
        );
        assert_eq!(
            core_kind_accent(CpuCoreKind::Efficiency, true, &colors),
            colors.cpu_core_e
        );
        assert_eq!(
            core_kind_accent(CpuCoreKind::LowPowerEfficiency, true, &colors),
            colors.cpu_core_lp_e
        );
        assert_eq!(
            core_kind_accent(CpuCoreKind::Performance, false, &colors),
            colors.chart_cpu
        );
    }
}
