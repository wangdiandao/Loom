//! Compact performance sidebar card composing `gpui-kit`'s [`AreaChart`].

use gpui_kit::component::chart::AreaChart;
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::{
    Div, Hsla, InteractiveElement as _, ParentElement as _, SharedString, Stateful, Styled as _,
    linear_color_stop, linear_gradient, px,
};

use crate::theme::LoomSemanticColors;

/// A single ordered data point rendered inside [`sparkline_card`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SparklinePoint {
    /// Monotonic sample index along the horizontal axis.
    pub index: usize,
    /// Metric value for this sample (typically in `0.0..=100.0`).
    pub value: f32,
}

impl SparklinePoint {
    /// Creates a new [`SparklinePoint`].
    #[inline]
    pub const fn new(index: usize, value: f32) -> Self {
        Self { index, value }
    }

    /// Converts a slice of `f32` samples into indexed [`SparklinePoint`] values.
    pub fn from_slice(values: &[f32]) -> Vec<Self> {
        values
            .iter()
            .enumerate()
            .map(|(index, &value)| Self { index, value })
            .collect()
    }

    /// Converts an iterator of permille values (`0..=1000`) into percentage
    /// (`0.0..=100.0`) [`SparklinePoint`] values.
    pub fn from_permille_iter(values: impl IntoIterator<Item = u16>) -> Vec<Self> {
        values
            .into_iter()
            .enumerate()
            .map(|(index, permille)| Self {
                index,
                value: f32::from(permille) / 10.0,
            })
            .collect()
    }
}

/// Presentational parameters for [`sparkline_card`].
#[derive(Debug, Clone)]
pub struct SparklineCardProps {
    /// Unique identifier for the card and its embedded [`AreaChart`].
    pub id: SharedString,
    /// Resource title (e.g. `"CPU"`, `"Memory"`, `"Disk 0 (C:)"`).
    pub title: SharedString,
    /// Secondary subtitle (e.g. `"14% 3.82 GHz"` or `"14.2 / 32.0 GB"`).
    pub subtitle: SharedString,
    /// Highlighted primary metric (e.g. `"14%"` or `"44%"`).
    pub primary_metric: SharedString,
    /// Ordered history samples rendered as a miniature area sparkline.
    pub points: Vec<SparklinePoint>,
    /// Upper bound of the Y-axis domain (falls back to `100.0` if non-positive or non-finite).
    pub y_max: f32,
    /// Stroke and accent color for the sparkline.
    pub accent_color: Hsla,
    /// Whether this card is currently selected.
    pub selected: bool,
}

/// Renders a selectable Flexbox card containing a miniature non-interactive
/// [`AreaChart`] alongside title, primary metric, and subtitle text.
///
/// Returns a [`Stateful<Div>`] so callers can chain `.on_click(...)` directly.
pub fn sparkline_card(props: SparklineCardProps, colors: &LoomSemanticColors) -> Stateful<Div> {
    let chart_id = SharedString::from(format!("{}-chart", props.id));
    let y_max = if props.y_max.is_finite() && props.y_max > 0.0 {
        props.y_max
    } else {
        100.0
    };

    let chart = AreaChart::new(props.points)
        .id(chart_id)
        .x(|point: &SparklinePoint| SharedString::from(point.index.to_string()))
        .y(|point: &SparklinePoint| point.value)
        .linear()
        .stroke(props.accent_color)
        .fill(linear_gradient(
            0.0,
            linear_color_stop(props.accent_color.opacity(0.35), 1.0),
            linear_color_stop(props.accent_color.opacity(0.05), 0.0),
        ))
        .y_domain(0.0, y_max)
        .y_padding(2.0, 0.0)
        .x_axis(false)
        .y_axis(false)
        .grid(false)
        .interactive(false)
        .appear(false);

    let (bg, border) = if props.selected {
        (colors.card_selected_bg, colors.card_selected_border)
    } else {
        (colors.card_bg, colors.card_border)
    };
    let hover_bg = colors.card_hover_bg;

    h_flex()
        .id(props.id)
        .w_full()
        .gap_2p5()
        .px_2p5()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(border)
        .bg(bg)
        .cursor_pointer()
        .hover(move |style| style.bg(hover_bg))
        .child(
            h_flex()
                .w(px(64.0))
                .h(px(42.0))
                .flex_shrink_0()
                .rounded_sm()
                .border_1()
                .border_color(props.accent_color.opacity(0.45))
                .overflow_hidden()
                .child(chart),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    h_flex()
                        .w_full()
                        .justify_between()
                        .items_baseline()
                        .gap_1()
                        .child(
                            h_flex()
                                .min_w_0()
                                .overflow_hidden()
                                .text_sm()
                                .font_semibold()
                                .text_color(colors.text_primary)
                                .whitespace_nowrap()
                                .child(props.title),
                        )
                        .child(
                            h_flex()
                                .flex_shrink_0()
                                .text_sm()
                                .font_semibold()
                                .text_color(colors.text_primary)
                                .whitespace_nowrap()
                                .child(props.primary_metric),
                        ),
                )
                .child(
                    h_flex()
                        .min_w_0()
                        .overflow_hidden()
                        .text_xs()
                        .text_color(colors.text_muted)
                        .whitespace_nowrap()
                        .child(props.subtitle),
                ),
        )
}
