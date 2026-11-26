//! Right-aligned resource heatmap table cell widget.

use gpui_kit::component::h_flex;
use gpui_kit::{IntoElement, ParentElement as _, SharedString, Styled as _};

use crate::format::HeatmapLevel;
use crate::theme::LoomSemanticColors;

/// Renders a compact, right-aligned table cell tinted according to [`HeatmapLevel`].
///
/// Cells at [`HeatmapLevel::None`] have a transparent background; higher tiers
/// progressively tint with warm amber/orange hues from [`LoomSemanticColors`].
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
        .rounded_sm()
        .bg(colors.heatmap_bg(level))
        .text_xs()
        .text_color(colors.text_primary)
        .whitespace_nowrap()
        .overflow_hidden()
        .child(text.into())
}
