//! Shared stateless `gpui-kit` chart builders, gap-aware point converters,
//! and metric layout helpers for the Performance view.
//!
//! All plots compose `gpui-kit`'s [`AreaChart`] directly without custom `Canvas` or GPU shaders,
//! using gap-aware point conversion so paused/sleep sentinels never fabricate spikes or flat
//! baseline artifacts.

use std::sync::Arc;

use gpui_kit::component::chart::AreaChart;
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Div, Hsla, IntoElement, ParentElement as _, SharedString, Styled as _, WeakEntity,
    div, linear_color_stop, linear_gradient, px,
};
use loom_types::{HistoryRing, NetAdapterSlice, ThroughputRing};
use loom_ui_kit::{LoomSemanticColors, SparklinePoint, UNAVAILABLE_LABEL};

use crate::features::performance::PerformanceView;
use crate::i18n;

/// Fixed 60-sample horizontal window matching Windows Task Manager's 60-second history.
pub(crate) const HISTORY_WINDOW: usize = 60;

/// Static x-axis slot labels (`"0"`..`"59"`) avoiding per-point `String` allocations during paint.
const INDEX_LABELS: [&str; HISTORY_WINDOW] = [
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15", "16",
    "17", "18", "19", "20", "21", "22", "23", "24", "25", "26", "27", "28", "29", "30", "31", "32",
    "33", "34", "35", "36", "37", "38", "39", "40", "41", "42", "43", "44", "45", "46", "47", "48",
    "49", "50", "51", "52", "53", "54", "55", "56", "57", "58", "59",
];

#[inline]
fn index_label(index: usize) -> SharedString {
    INDEX_LABELS
        .get(index)
        .copied()
        .map(SharedString::new_static)
        .unwrap_or_else(|| SharedString::from(index.to_string()))
}

/// Single-series gap-aware data point for 60-second [`AreaChart`] plots.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SinglePoint {
    pub(crate) index: usize,
    pub(crate) value: Option<f32>,
}

/// Dual-series gap-aware data point for overlaid 60-second [`AreaChart`] plots
/// (e.g. Total + Kernel CPU, Read + Write Disk, Receive + Send Network).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DualPoint {
    pub(crate) index: usize,
    pub(crate) primary: Option<f32>,
    pub(crate) secondary: Option<f32>,
}

/// Returns the trimmed optional `Arc<str>` value or `fallback` when absent/blank.
#[inline]
pub(crate) fn opt_arc_str_or_fallback(opt: &Option<Arc<str>>, fallback: &str) -> String {
    opt.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

/// Returns a [`SharedString`] reusing `Arc<str>` when already non-empty and trimmed,
/// or `fallback` when blank.
#[inline]
pub(crate) fn non_empty_arc_shared(value: &Arc<str>, fallback: &str) -> SharedString {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        SharedString::from(fallback.to_owned())
    } else if trimmed.len() == value.len() {
        SharedString::from(Arc::clone(value))
    } else {
        SharedString::from(trimmed.to_owned())
    }
}

/// Builds the shared "Copy summary" context menu item for Performance detail panes.
pub(crate) fn copy_summary_menu_item(weak_view: WeakEntity<PerformanceView>) -> PopupMenuItem {
    PopupMenuItem::new(i18n::tr("perf.perf_polish_copy_summary")).on_click(move |_, window, cx| {
        let _ = weak_view.update(cx, |view, cx| {
            view.copy_selected_summary(window, cx);
        });
    })
}

/// Computes the gap-safe peak combined network throughput (recv + sent) in bytes/sec.
pub(crate) fn network_combined_peak_bytes(net: &NetAdapterSlice) -> u64 {
    let history_peak = net
        .recv_history
        .iter()
        .zip(net.sent_history.iter())
        .map(|(r, s)| {
            let rv = if ThroughputRing::<HISTORY_WINDOW>::is_gap(r) {
                0
            } else {
                r
            };
            let sv = if ThroughputRing::<HISTORY_WINDOW>::is_gap(s) {
                0
            } else {
                s
            };
            rv.saturating_add(sv)
        })
        .max()
        .unwrap_or(0);
    history_peak.max(
        net.recv_bytes_per_sec
            .saturating_add(net.sent_bytes_per_sec),
    )
}

/// Formats a stepped ceiling label from `(y_max, unit)` returned by
/// [`loom_ui_kit::stepped_throughput_ceiling`] or [`loom_ui_kit::stepped_network_ceiling`].
pub(crate) fn format_stepped_ceiling_label(y_max: f64, unit: &str) -> SharedString {
    if y_max.fract().abs() < 1e-6 {
        SharedString::from(format!("{} {unit}", y_max as u64))
    } else {
        SharedString::from(format!("{y_max:.1} {unit}"))
    }
}

#[inline]
fn permille_to_opt_percent(raw: u16) -> Option<f32> {
    if HistoryRing::<HISTORY_WINDOW>::is_gap(raw) {
        None
    } else {
        Some((f32::from(raw.min(1000))) / 10.0)
    }
}

#[inline]
fn throughput_to_opt_scaled(raw: u64, safe_div: f64) -> Option<f32> {
    if ThroughputRing::<HISTORY_WINDOW>::is_gap(raw) {
        None
    } else {
        Some((raw as f64 / safe_div) as f32)
    }
}

/// Converts a [`HistoryRing<60>`] (permille `0..=1000` + [`HistoryRing::GAP_SENTINEL`])
/// into a right-aligned 60-point gap-aware percentage (`0.0..=100.0`) series in a single pass.
///
/// Leading unfilled slots and `GAP_SENTINEL` entries map to `None` rather than fabricating
/// spikes or misleading zero baselines.
pub(crate) fn history_ring_to_points(ring: &HistoryRing<HISTORY_WINDOW>) -> Vec<SinglePoint> {
    let mut points = Vec::with_capacity(HISTORY_WINDOW);
    let len = ring.len().min(HISTORY_WINDOW);
    let pad = HISTORY_WINDOW - len;
    for index in 0..pad {
        points.push(SinglePoint { index, value: None });
    }
    let skip = ring.len().saturating_sub(HISTORY_WINDOW);
    for (offset, raw) in ring.iter_ordered().skip(skip).enumerate() {
        points.push(SinglePoint {
            index: pad + offset,
            value: permille_to_opt_percent(raw),
        });
    }
    points
}

/// Converts a [`HistoryRing<60>`] into gap-safe [`SparklinePoint`] entries for sidebar cards
/// in a single pass.
///
/// Returns an empty vector when no valid (non-gap) samples exist (e.g., while a discrete GPU
/// is sleeping in D3), ensuring gap sentinels never fabricate `6553.5%` spikes or flat baselines.
pub(crate) fn history_ring_to_sparkline_points(
    ring: &HistoryRing<HISTORY_WINDOW>,
) -> Vec<SparklinePoint> {
    let len = ring.len().min(HISTORY_WINDOW);
    if len == 0 {
        return Vec::new();
    }
    let skip = ring.len().saturating_sub(HISTORY_WINDOW);
    let pad = HISTORY_WINDOW - len;
    let mut has_valid = false;
    let mut points = Vec::with_capacity(HISTORY_WINDOW);
    for index in 0..pad {
        points.push(SparklinePoint::new(index, 0.0));
    }
    for (offset, raw) in ring.iter_ordered().skip(skip).enumerate() {
        let value = match permille_to_opt_percent(raw) {
            Some(v) => {
                has_valid = true;
                v
            }
            None => 0.0,
        };
        points.push(SparklinePoint::new(pad + offset, value));
    }
    if !has_valid {
        return Vec::new();
    }
    points
}

/// Converts two [`ThroughputRing<60>`] buffers (bytes/sec) into gap-safe combined
/// [`SparklinePoint`] entries scaled by `divisor` (matching [`loom_ui_kit::stepped_network_ceiling`]).
pub(crate) fn throughput_pair_to_sparkline_points(
    recv: &ThroughputRing<HISTORY_WINDOW>,
    sent: &ThroughputRing<HISTORY_WINDOW>,
    divisor: f64,
) -> Vec<SparklinePoint> {
    let safe_div = if divisor.is_finite() && divisor > 0.0 {
        divisor
    } else {
        1.0
    };
    let r_len = recv.len().min(HISTORY_WINDOW);
    let s_len = sent.len().min(HISTORY_WINDOW);
    let max_len = r_len.max(s_len);
    if max_len == 0 {
        return Vec::new();
    }

    let mut r_buf = [None::<u64>; HISTORY_WINDOW];
    let r_pad = HISTORY_WINDOW - r_len;
    for (offset, raw) in recv
        .iter()
        .skip(recv.len().saturating_sub(HISTORY_WINDOW))
        .enumerate()
    {
        if !ThroughputRing::<HISTORY_WINDOW>::is_gap(raw) {
            r_buf[r_pad + offset] = Some(raw);
        }
    }

    let mut s_buf = [None::<u64>; HISTORY_WINDOW];
    let s_pad = HISTORY_WINDOW - s_len;
    for (offset, raw) in sent
        .iter()
        .skip(sent.len().saturating_sub(HISTORY_WINDOW))
        .enumerate()
    {
        if !ThroughputRing::<HISTORY_WINDOW>::is_gap(raw) {
            s_buf[s_pad + offset] = Some(raw);
        }
    }

    let mut has_valid = false;
    let mut points = Vec::with_capacity(HISTORY_WINDOW);
    for index in 0..HISTORY_WINDOW {
        let value = match (r_buf[index], s_buf[index]) {
            (Some(rv), Some(sv)) => {
                has_valid = true;
                (rv.saturating_add(sv) as f64 / safe_div) as f32
            }
            (Some(rv), None) => {
                has_valid = true;
                (rv as f64 / safe_div) as f32
            }
            (None, Some(sv)) => {
                has_valid = true;
                (sv as f64 / safe_div) as f32
            }
            (None, None) => 0.0,
        };
        points.push(SparklinePoint::new(index, value));
    }

    if !has_valid {
        return Vec::new();
    }
    points
}

/// Converts two [`HistoryRing<60>`] buffers (permille `0..=1000` + `GAP_SENTINEL`)
/// into a right-aligned 60-point gap-aware dual percentage series in a single pass.
pub(crate) fn dual_history_rings_to_points(
    primary: &HistoryRing<HISTORY_WINDOW>,
    secondary: &HistoryRing<HISTORY_WINDOW>,
) -> Vec<DualPoint> {
    let mut p_buf = [None::<f32>; HISTORY_WINDOW];
    let p_len = primary.len().min(HISTORY_WINDOW);
    let p_pad = HISTORY_WINDOW - p_len;
    for (offset, raw) in primary
        .iter_ordered()
        .skip(primary.len().saturating_sub(HISTORY_WINDOW))
        .enumerate()
    {
        p_buf[p_pad + offset] = permille_to_opt_percent(raw);
    }

    let mut s_buf = [None::<f32>; HISTORY_WINDOW];
    let s_len = secondary.len().min(HISTORY_WINDOW);
    let s_pad = HISTORY_WINDOW - s_len;
    for (offset, raw) in secondary
        .iter_ordered()
        .skip(secondary.len().saturating_sub(HISTORY_WINDOW))
        .enumerate()
    {
        s_buf[s_pad + offset] = permille_to_opt_percent(raw);
    }

    let mut points = Vec::with_capacity(HISTORY_WINDOW);
    for index in 0..HISTORY_WINDOW {
        points.push(DualPoint {
            index,
            primary: p_buf[index],
            secondary: s_buf[index],
        });
    }
    points
}

/// Converts two [`ThroughputRing<60>`] buffers (bytes/sec + `GAP_SENTINEL`) into a
/// right-aligned 60-point gap-aware dual series divided by `divisor` so values map
/// cleanly onto a stepped ceiling's unit domain.
pub(crate) fn dual_throughput_rings_to_points(
    primary: &ThroughputRing<HISTORY_WINDOW>,
    secondary: &ThroughputRing<HISTORY_WINDOW>,
    divisor: f64,
) -> Vec<DualPoint> {
    let safe_div = if divisor.is_finite() && divisor > 0.0 {
        divisor
    } else {
        1.0
    };

    let mut p_buf = [None::<f32>; HISTORY_WINDOW];
    let p_len = primary.len().min(HISTORY_WINDOW);
    let p_pad = HISTORY_WINDOW - p_len;
    for (offset, raw) in primary
        .iter()
        .skip(primary.len().saturating_sub(HISTORY_WINDOW))
        .enumerate()
    {
        p_buf[p_pad + offset] = throughput_to_opt_scaled(raw, safe_div);
    }

    let mut s_buf = [None::<f32>; HISTORY_WINDOW];
    let s_len = secondary.len().min(HISTORY_WINDOW);
    let s_pad = HISTORY_WINDOW - s_len;
    for (offset, raw) in secondary
        .iter()
        .skip(secondary.len().saturating_sub(HISTORY_WINDOW))
        .enumerate()
    {
        s_buf[s_pad + offset] = throughput_to_opt_scaled(raw, safe_div);
    }

    let mut points = Vec::with_capacity(HISTORY_WINDOW);
    for index in 0..HISTORY_WINDOW {
        points.push(DualPoint {
            index,
            primary: p_buf[index],
            secondary: s_buf[index],
        });
    }
    points
}

/// Builds a 60-second single-series percentage (`0..100%`) [`AreaChart`].
///
/// When all points are gaps (`None`), passes an empty dataset so the chart renders
/// an honest empty grid instead of fabricating a flat `0%` baseline.
pub(crate) fn build_percent_area_chart(
    id: impl Into<SharedString>,
    points: Vec<SinglePoint>,
    series_name: impl Into<SharedString>,
    color: Hsla,
    interactive: bool,
) -> AreaChart<SinglePoint, SharedString, f32> {
    let data = if points.iter().all(|p| p.value.is_none()) {
        Vec::new()
    } else {
        points
    };

    AreaChart::new(data)
        .id(id.into())
        .x(|p: &SinglePoint| index_label(p.index))
        .y(|p: &SinglePoint| p.value.unwrap_or(0.0))
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
        .tooltip_value(|p, _series_ix, val| {
            if p.value.is_none() {
                SharedString::new_static(UNAVAILABLE_LABEL)
            } else {
                SharedString::from(format!("{val:.1}%"))
            }
        })
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
    let data = if points
        .iter()
        .all(|p| p.primary.is_none() && p.secondary.is_none())
    {
        Vec::new()
    } else {
        points
    };

    AreaChart::new(data)
        .id(id.into())
        .x(|p: &DualPoint| index_label(p.index))
        .y(|p: &DualPoint| p.primary.unwrap_or(0.0))
        .name(primary_name)
        .linear()
        .stroke(primary_color)
        .fill(linear_gradient(
            0.0,
            linear_color_stop(primary_color.opacity(0.28), 1.0),
            linear_color_stop(primary_color.opacity(0.04), 0.0),
        ))
        .y(|p: &DualPoint| p.secondary.unwrap_or(0.0))
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
        .tooltip_value(|p, series_ix, val| {
            let is_gap = if series_ix == 0 {
                p.primary.is_none()
            } else {
                p.secondary.is_none()
            };
            if is_gap {
                SharedString::new_static(UNAVAILABLE_LABEL)
            } else {
                SharedString::from(format!("{val:.1}%"))
            }
        })
}

/// Builds a 60-second stepped-ceiling dual-series throughput [`AreaChart`] (Disk Read/Write or
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
    let data = if points
        .iter()
        .all(|p| p.primary.is_none() && p.secondary.is_none())
    {
        Vec::new()
    } else {
        points
    };

    AreaChart::new(data)
        .id(id.into())
        .x(|p: &DualPoint| index_label(p.index))
        .y(|p: &DualPoint| p.primary.unwrap_or(0.0))
        .name(primary_name)
        .linear()
        .stroke(primary_color)
        .fill(linear_gradient(
            0.0,
            linear_color_stop(primary_color.opacity(0.28), 1.0),
            linear_color_stop(primary_color.opacity(0.04), 0.0),
        ))
        .y(|p: &DualPoint| p.secondary.unwrap_or(0.0))
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
        .tooltip_value(move |p, series_ix, val| {
            let is_gap = if series_ix == 0 {
                p.primary.is_none()
            } else {
                p.secondary.is_none()
            };
            if is_gap {
                SharedString::new_static(UNAVAILABLE_LABEL)
            } else {
                SharedString::from(format_value(val))
            }
        })
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

#[cfg(test)]
mod tests {
    use loom_types::{HistoryRing, ThroughputRing};

    use super::*;

    #[test]
    fn gap_sentinels_never_fabricate_spikes_in_history_or_throughput_points() {
        let mut h = HistoryRing::<HISTORY_WINDOW>::new();
        h.push(250);
        h.push_gap();
        h.push(500);

        let pts = history_ring_to_points(&h);
        assert_eq!(pts.len(), HISTORY_WINDOW);
        assert_eq!(pts[HISTORY_WINDOW - 3].value, Some(25.0));
        assert_eq!(pts[HISTORY_WINDOW - 2].value, None);
        assert_eq!(pts[HISTORY_WINDOW - 1].value, Some(50.0));
        assert!(pts[0].value.is_none());

        let spark = history_ring_to_sparkline_points(&h);
        assert_eq!(spark.len(), HISTORY_WINDOW);
        assert!(spark.iter().all(|p| p.value <= 100.0 && p.value >= 0.0));

        // Gap-only ring produces empty sparkline (no fabricated flat baseline).
        let mut gap_only = HistoryRing::<HISTORY_WINDOW>::new();
        gap_only.push_gap();
        assert!(history_ring_to_sparkline_points(&gap_only).is_empty());

        // Throughput gap sentinel never produces u64::MAX spike.
        let mut r = ThroughputRing::<HISTORY_WINDOW>::new();
        let mut w = ThroughputRing::<HISTORY_WINDOW>::new();
        r.push(1024 * 1024);
        r.push_gap();
        w.push(512 * 1024);
        w.push_gap();

        let dual = dual_throughput_rings_to_points(&r, &w, 1024.0 * 1024.0);
        assert_eq!(dual.len(), HISTORY_WINDOW);
        assert_eq!(dual[HISTORY_WINDOW - 2].primary, Some(1.0));
        assert_eq!(dual[HISTORY_WINDOW - 2].secondary, Some(0.5));
        assert_eq!(dual[HISTORY_WINDOW - 1].primary, None);
        assert_eq!(dual[HISTORY_WINDOW - 1].secondary, None);
    }

    #[test]
    fn format_stepped_ceiling_label_formats_integers_and_decimals_cleanly() {
        assert_eq!(
            format_stepped_ceiling_label(100.0, "KB/s").as_ref(),
            "100 KB/s"
        );
        assert_eq!(format_stepped_ceiling_label(1.0, "MB/s").as_ref(), "1 MB/s");
        assert_eq!(
            format_stepped_ceiling_label(2.5, "Gbps").as_ref(),
            "2.5 Gbps"
        );
        assert_eq!(
            format_stepped_ceiling_label(100.0, "Mbps").as_ref(),
            "100 Mbps"
        );
    }
}
