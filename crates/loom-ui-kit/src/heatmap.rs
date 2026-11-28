//! Right-aligned resource heatmap table cell widget.

use gpui_kit::component::h_flex;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{Div, IntoElement, ParentElement as _, SharedString, Styled as _};

use crate::format::{HeatmapLevel, MetricDisplay};
use crate::theme::LoomSemanticColors;

#[inline]
fn heatmap_cell_base(level: HeatmapLevel, colors: &LoomSemanticColors) -> Div {
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
}

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
    heatmap_cell_base(level, colors).child(text.into())
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
    heatmap_cell_base(level, colors)
        .font_family(mono_font)
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
    let mut cell = heatmap_cell_base(level, colors)
        .font_family(mono_font)
        .child(metric.value);

    if !metric.unit.is_empty() {
        cell = cell.child(SharedString::new_static(metric.unit));
    }

    cell
}

#[cfg(test)]
mod tests {
    use gpui_kit::SharedString;
    use gpui_kit::component::{Theme, ThemeColor, ThemeMode};

    use crate::format::{HeatmapLevel, format_bytes_metric, format_permille_metric};
    use crate::theme::LoomSemanticColors;

    use super::{heatmap_cell, heatmap_cell_metric, heatmap_cell_mono};

    #[test]
    fn heatmap_widgets_construct_cleanly() {
        let mut theme = Theme::from(ThemeColor::dark().as_ref());
        theme.mode = ThemeMode::Dark;
        let colors = LoomSemanticColors::from_theme(&theme);
        let mono = SharedString::new_static("Consolas");

        let _cell = heatmap_cell("42.5%", HeatmapLevel::Medium, &colors);
        let _metric = heatmap_cell_metric(
            format_bytes_metric(64 * 1024 * 1024),
            HeatmapLevel::Low,
            &colors,
            mono.clone(),
        );
        let _pct = heatmap_cell_metric(
            format_permille_metric(425),
            HeatmapLevel::Medium,
            &colors,
            mono.clone(),
        );
        let _time = heatmap_cell_mono("00:01:23", HeatmapLevel::None, &colors, mono);
    }
}
