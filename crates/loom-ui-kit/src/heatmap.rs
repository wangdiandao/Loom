//! Right-aligned resource heatmap table cell widget.

use gpui_kit::component::h_flex;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{IntoElement, ParentElement as _, SharedString, Styled as _};

use crate::format::{HeatmapLevel, MetricDisplay};
use crate::theme::LoomSemanticColors;

/// Renders a compact, right-aligned table cell tinted according to [`HeatmapLevel`].
///
/// Cells at [`HeatmapLevel::None`] omit background/radius styling to minimize
/// paint overhead; higher tiers progressively tint with warm amber/orange hues
/// from [`LoomSemanticColors`].
pub fn heatmap_cell(
    text: impl Into<SharedString>,
    level: HeatmapLevel,
    colors: &LoomSemanticColors,
) -> impl IntoElement {
    h_flex()
        .size_full()
        .justify_end()
        .items_center()
        .px_2()
        .py_0p5()
        .when(level != HeatmapLevel::None, |d| {
            d.rounded_sm().bg(colors.heatmap_bg(level))
        })
        .text_xs()
        .text_color(colors.text_primary)
        .whitespace_nowrap()
        .overflow_hidden()
        .child(text.into())
}

/// Renders a right-aligned, monospace-styled table cell tinted according to [`HeatmapLevel`].
///
/// Intended for tabular numeric strings such as cumulative CPU time (`"00:01:23"`)
/// that do not split into a separate [`MetricDisplay`] unit suffix.
pub fn heatmap_cell_mono(
    text: impl Into<SharedString>,
    level: HeatmapLevel,
    colors: &LoomSemanticColors,
    mono_font: SharedString,
) -> impl IntoElement {
    h_flex()
        .size_full()
        .justify_end()
        .items_center()
        .px_2()
        .py_0p5()
        .when(level != HeatmapLevel::None, |d| {
            d.rounded_sm().bg(colors.heatmap_bg(level))
        })
        .text_xs()
        .font_family(mono_font)
        .text_color(colors.text_primary)
        .whitespace_nowrap()
        .overflow_hidden()
        .child(text.into())
}

/// Renders a uniform monospace heatmap cell from a split [`MetricDisplay`].
///
/// Separating `metric.value` and `metric.unit` into adjacent text children
/// allows `gpui-fast::NumberShaping` to shape the numeric digits without calling
/// the OS text shaper while the static unit suffix hits `LineLayoutCache`.
pub fn heatmap_cell_metric(
    metric: MetricDisplay,
    level: HeatmapLevel,
    colors: &LoomSemanticColors,
    mono_font: SharedString,
) -> impl IntoElement {
    let mut cell = h_flex()
        .size_full()
        .justify_end()
        .items_center()
        .px_2()
        .py_0p5()
        .when(level != HeatmapLevel::None, |d| {
            d.rounded_sm().bg(colors.heatmap_bg(level))
        })
        .text_xs()
        .font_family(mono_font)
        .text_color(colors.text_primary)
        .whitespace_nowrap()
        .overflow_hidden()
        .child(metric.value);

    if !metric.unit.is_empty() {
        cell = cell.child(SharedString::new_static(metric.unit));
    }

    cell
}
