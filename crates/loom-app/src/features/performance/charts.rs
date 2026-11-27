//! Shared stateless `gpui-kit` chart builders and metric layout helpers for the Performance view.
//!
//! All plots compose `gpui-kit`'s [`AreaChart`] directly without custom `Canvas` or GPU shaders.

use gpui_kit::component::chart::AreaChart;
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Div, Hsla, IntoElement, ParentElement as _, SharedString, Styled as _, div,
    linear_color_stop, linear_gradient, px,
};
use loom_types::{HistoryRing, ThroughputRing};
use loom_ui_kit::LoomSemanticColors;

use crate::i18n;

/// Fixed 60-sample horizontal window matching Windows Task Manager's 60-second history.
pub(crate) const HISTORY_WINDOW: usize = 60;

/// Single-series data point for 60-second [`AreaChart`] plots.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SinglePoint {
    pub(crate) index: usize,
    pub(crate) value: f32,
}

/// Dual-series data point for overlaid 60-second [`AreaChart`] plots (e.g. Total + Kernel CPU,
/// Read + Write Disk, Receive + Send Network).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DualPoint {
    pub(crate) index: usize,
    pub(crate) primary: f32,
    pub(crate) secondary: f32,
}

/// Converts a [`HistoryRing<60>`] (permille `0..=1000`) into a right-aligned 60-point
/// percentage (`0.0..=100.0`) series, left-padded with `0.0`.
pub(crate) fn history_ring_to_points(ring: &HistoryRing<HISTORY_WINDOW>) -> Vec<SinglePoint> {
    let len = ring.len();
    let pad = HISTORY_WINDOW.saturating_sub(len);
    let mut points = Vec::with_capacity(HISTORY_WINDOW);
    for index in 0..pad {
        points.push(SinglePoint { index, value: 0.0 });
    }
    for (offset, permille) in ring.iter_ordered().enumerate() {
        points.push(SinglePoint {
            index: pad + offset,
            value: f32::from(permille) / 10.0,
        });
    }
    points
}

/// Converts two [`HistoryRing<60>`] buffers (permille `0..=1000`) into a right-aligned
/// 60-point dual percentage series.
pub(crate) fn dual_history_rings_to_points(
    primary: &HistoryRing<HISTORY_WINDOW>,
    secondary: &HistoryRing<HISTORY_WINDOW>,
) -> Vec<DualPoint> {
    let p_vals: Vec<u16> = primary.iter_ordered().collect();
    let s_vals: Vec<u16> = secondary.iter_ordered().collect();
    let p_pad = HISTORY_WINDOW.saturating_sub(p_vals.len());
    let s_pad = HISTORY_WINDOW.saturating_sub(s_vals.len());

    let mut points = Vec::with_capacity(HISTORY_WINDOW);
    for index in 0..HISTORY_WINDOW {
        let p = index
            .checked_sub(p_pad)
            .and_then(|i| p_vals.get(i).copied())
            .unwrap_or(0);
        let s = index
            .checked_sub(s_pad)
            .and_then(|i| s_vals.get(i).copied())
            .unwrap_or(0);
        points.push(DualPoint {
            index,
            primary: f32::from(p) / 10.0,
            secondary: f32::from(s) / 10.0,
        });
    }
    points
}

/// Converts two [`ThroughputRing<60>`] buffers (bytes/sec) into a right-aligned 60-point
/// dual series scaled by `multiplier` (use `1.0` for bytes/sec or `8.0` for bits/sec).
pub(crate) fn dual_throughput_rings_to_points(
    primary: &ThroughputRing<HISTORY_WINDOW>,
    secondary: &ThroughputRing<HISTORY_WINDOW>,
    multiplier: f32,
) -> Vec<DualPoint> {
    let p_vals: Vec<u64> = primary.iter().collect();
    let s_vals: Vec<u64> = secondary.iter().collect();
    let p_pad = HISTORY_WINDOW.saturating_sub(p_vals.len());
    let s_pad = HISTORY_WINDOW.saturating_sub(s_vals.len());

    let mut points = Vec::with_capacity(HISTORY_WINDOW);
    for index in 0..HISTORY_WINDOW {
        let p = index
            .checked_sub(p_pad)
            .and_then(|i| p_vals.get(i).copied())
            .unwrap_or(0);
        let s = index
            .checked_sub(s_pad)
            .and_then(|i| s_vals.get(i).copied())
            .unwrap_or(0);
        points.push(DualPoint {
            index,
            primary: p as f32 * multiplier,
            secondary: s as f32 * multiplier,
        });
    }
    points
}

/// Builds a 60-second single-series percentage (`0..100%`) [`AreaChart`].
pub(crate) fn build_percent_area_chart(
    id: impl Into<SharedString>,
    points: Vec<SinglePoint>,
    series_name: impl Into<SharedString>,
    color: Hsla,
    interactive: bool,
) -> AreaChart<SinglePoint, SharedString, f32> {
    AreaChart::new(points)
        .id(id.into())
        .x(|p: &SinglePoint| SharedString::from(p.index.to_string()))
        .y(|p: &SinglePoint| p.value)
        .name(series_name)
        .linear()
        .stroke(color)
        .fill(linear_gradient(
            0.0,
            linear_color_stop(color.opacity(0.32), 1.0),
            linear_color_stop(color.opacity(0.05), 0.0),
        ))
        .y_domain(0.0, 100.0)
        .y_padding(2.0, 0.0)
        .y_tick_count(5)
        .grid(true)
        .grid_columns(6)
        .grid_dashed(true)
        .x_axis(false)
        .y_axis(false)
        .interactive(interactive)
        .appear(false)
        .tooltip_title(|p: &SinglePoint| {
            let secs_ago = HISTORY_WINDOW.saturating_sub(1).saturating_sub(p.index);
            i18n::perf_time_offset_label(secs_ago).into()
        })
        .tooltip_value(|_p, _series_ix, val| SharedString::from(format!("{val:.1}%")))
}

/// Builds a 60-second dual-series percentage (`0..100%`) [`AreaChart`] (e.g. CPU Total + Kernel).
pub(crate) fn build_dual_percent_area_chart(
    id: impl Into<SharedString>,
    points: Vec<DualPoint>,
    primary_name: impl Into<SharedString>,
    primary_color: Hsla,
    secondary_name: impl Into<SharedString>,
    secondary_color: Hsla,
) -> AreaChart<DualPoint, SharedString, f32> {
    AreaChart::new(points)
        .id(id.into())
        .x(|p: &DualPoint| SharedString::from(p.index.to_string()))
        .y(|p: &DualPoint| p.primary)
        .name(primary_name)
        .linear()
        .stroke(primary_color)
        .fill(linear_gradient(
            0.0,
            linear_color_stop(primary_color.opacity(0.28), 1.0),
            linear_color_stop(primary_color.opacity(0.04), 0.0),
        ))
        .y(|p: &DualPoint| p.secondary)
        .name(secondary_name)
        .linear()
        .stroke(secondary_color)
        .fill(linear_gradient(
            0.0,
            linear_color_stop(secondary_color.opacity(0.36), 1.0),
            linear_color_stop(secondary_color.opacity(0.08), 0.0),
        ))
        .y_domain(0.0, 100.0)
        .y_padding(2.0, 0.0)
        .y_tick_count(5)
        .grid(true)
        .grid_columns(6)
        .grid_dashed(true)
        .x_axis(false)
        .y_axis(false)
        .interactive(true)
        .appear(false)
        .tooltip_title(|p: &DualPoint| {
            let secs_ago = HISTORY_WINDOW.saturating_sub(1).saturating_sub(p.index);
            i18n::perf_time_offset_label(secs_ago).into()
        })
        .tooltip_value(|_p, _series_ix, val| SharedString::from(format!("{val:.1}%")))
}

/// Builds a 60-second adaptive-scale dual-series throughput [`AreaChart`] (Disk Read/Write or
/// Network Receive/Send).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_dual_throughput_area_chart(
    id: impl Into<SharedString>,
    points: Vec<DualPoint>,
    y_max: f32,
    primary_name: impl Into<SharedString>,
    primary_color: Hsla,
    secondary_name: impl Into<SharedString>,
    secondary_color: Hsla,
    format_value: impl Fn(f64) -> String + 'static,
) -> AreaChart<DualPoint, SharedString, f32> {
    let ceiling = if y_max.is_finite() && y_max > 0.0 {
        y_max
    } else {
        1.0
    };
    AreaChart::new(points)
        .id(id.into())
        .x(|p: &DualPoint| SharedString::from(p.index.to_string()))
        .y(|p: &DualPoint| p.primary)
        .name(primary_name)
        .linear()
        .stroke(primary_color)
        .fill(linear_gradient(
            0.0,
            linear_color_stop(primary_color.opacity(0.28), 1.0),
            linear_color_stop(primary_color.opacity(0.04), 0.0),
        ))
        .y(|p: &DualPoint| p.secondary)
        .name(secondary_name)
        .linear()
        .stroke(secondary_color)
        .fill(linear_gradient(
            0.0,
            linear_color_stop(secondary_color.opacity(0.22), 1.0),
            linear_color_stop(secondary_color.opacity(0.03), 0.0),
        ))
        .y_domain(0.0, ceiling)
        .y_padding(2.0, 0.0)
        .y_tick_count(5)
        .grid(true)
        .grid_columns(6)
        .grid_dashed(true)
        .x_axis(false)
        .y_axis(false)
        .interactive(true)
        .appear(false)
        .tooltip_title(|p: &DualPoint| {
            let secs_ago = HISTORY_WINDOW.saturating_sub(1).saturating_sub(p.index);
            i18n::perf_time_offset_label(secs_ago).into()
        })
        .tooltip_value(move |_p, _series_ix, val| SharedString::from(format_value(val)))
}

/// Renders a compact color swatch + series label for chart headers.
pub(crate) fn legend_swatch(
    label: impl Into<SharedString>,
    color: Hsla,
    colors: &LoomSemanticColors,
) -> Div {
    h_flex()
        .items_center()
        .gap_1p5()
        .child(div().w(px(10.)).h(px(3.)).rounded_full().bg(color))
        .child(
            div()
                .text_xs()
                .text_color(colors.text_muted)
                .child(label.into()),
        )
}

/// Wraps a chart element in a consistent Performance frame with title, optional
/// legend, top-right max scale label, accent border, and bottom 60s axis captions.
#[allow(clippy::too_many_arguments)]
pub(crate) fn chart_frame(
    title: impl Into<SharedString>,
    scale_label: impl Into<SharedString>,
    legend: Vec<AnyElement>,
    accent_color: Hsla,
    height_px: f32,
    chart: impl IntoElement,
    colors: &LoomSemanticColors,
) -> Div {
    let mut header_left = h_flex().items_center().gap_3().child(
        div()
            .text_xs()
            .font_medium()
            .text_color(colors.text_muted)
            .child(title.into()),
    );
    for item in legend {
        header_left = header_left.child(item);
    }

    v_flex()
        .w_full()
        .gap_1()
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap_2()
                .child(header_left)
                .child(
                    div()
                        .text_xs()
                        .text_color(colors.text_muted)
                        .child(scale_label.into()),
                ),
        )
        .child(
            div()
                .w_full()
                .h(px(height_px))
                .rounded_md()
                .border_1()
                .border_color(accent_color.opacity(0.45))
                .bg(colors.card_bg)
                .overflow_hidden()
                .child(chart),
        )
        .child(
            h_flex()
                .w_full()
                .justify_between()
                .text_xs()
                .text_color(colors.text_muted)
                .child(i18n::tr("perf.time_axis_start"))
                .child(i18n::tr("perf.time_axis_end")),
        )
}

/// Renders the device header row with primary title on the left and hardware model/subtitle on the right.
pub(crate) fn device_header(
    title: impl Into<SharedString>,
    model_or_subtitle: impl Into<SharedString>,
    badge: Option<AnyElement>,
    colors: &LoomSemanticColors,
) -> Div {
    let model = model_or_subtitle.into();
    let mut left = h_flex().items_center().gap_2().flex_shrink_0().child(
        div()
            .text_xl()
            .font_semibold()
            .text_color(colors.text_primary)
            .child(title.into()),
    );
    if let Some(badge_el) = badge {
        left = left.child(badge_el);
    }

    h_flex()
        .w_full()
        .items_baseline()
        .justify_between()
        .gap_4()
        .child(left)
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_sm()
                .font_medium()
                .text_color(colors.text_muted)
                .child(model),
        )
}

/// A live metric stat item for the left half of the bottom 2-column metrics panel.
pub(crate) struct MetricStat {
    pub(crate) label: SharedString,
    pub(crate) value: SharedString,
}

impl MetricStat {
    pub(crate) fn new(label: impl Into<SharedString>, value: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

/// A hardware specification key-value row for the right half of the bottom 2-column metrics panel.
pub(crate) struct SpecRow {
    pub(crate) label: SharedString,
    pub(crate) value: SharedString,
}

impl SpecRow {
    pub(crate) fn new(label: impl Into<SharedString>, value: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

/// Renders the bottom 2-column Windows 11 Task Manager style metrics panel:
/// - Left column: live counters arranged in a clean 2-column sub-grid with emphasized values.
/// - Right column: compact key-value hardware specifications table.
pub(crate) fn metrics_two_column_panel(
    left_stats: Vec<MetricStat>,
    right_specs: Vec<SpecRow>,
    colors: &LoomSemanticColors,
) -> Div {
    let mut left_grid = v_flex().flex_1().min_w(px(260.)).gap_3();
    for pair in left_stats.chunks(2) {
        let mut row = h_flex().w_full().gap_4();
        for stat in pair {
            row = row.child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .text_xs()
                            .text_color(colors.text_muted)
                            .truncate()
                            .child(stat.label.clone()),
                    )
                    .child(
                        div()
                            .text_base()
                            .font_semibold()
                            .text_color(colors.text_primary)
                            .truncate()
                            .child(stat.value.clone()),
                    ),
            );
        }
        if pair.len() == 1 {
            row = row.child(div().flex_1());
        }
        left_grid = left_grid.child(row);
    }

    let mut right_table = v_flex().flex_1().min_w(px(260.)).gap_1p5();
    for spec in right_specs {
        right_table = right_table.child(
            h_flex()
                .w_full()
                .items_baseline()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(colors.text_muted)
                        .child(spec.label),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .font_medium()
                        .text_color(colors.text_primary)
                        .child(spec.value),
                ),
        );
    }

    h_flex()
        .w_full()
        .flex_wrap()
        .items_start()
        .gap_6()
        .pt_2()
        .border_t_1()
        .border_color(colors.card_border)
        .child(left_grid)
        .child(right_table)
}
