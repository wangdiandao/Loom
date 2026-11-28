//! Compact performance sidebar card composing `gpui-kit`'s [`AreaChart`].

use gpui_kit::component::chart::AreaChart;
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::{
    Div, Hsla, InteractiveElement as _, ParentElement as _, Role, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, div, linear_color_stop, linear_gradient, px,
};

use crate::format::format_u32_shared;
use crate::theme::LoomSemanticColors;

/// Sentinel permille value (`u16::MAX`) indicating a missing, paused, or
/// error sample inside fixed-width `u16` history buffers.
pub const PERMILLE_GAP_SENTINEL: u16 = u16::MAX;

/// A single slot in a right-aligned time-series window that distinguishes a
/// missing/paused/error interval (`value: None`) from a genuine `0.0` sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GapAwareSparklinePoint {
    /// Monotonic sample index along the horizontal axis (`0..window_len`).
    pub index: usize,
    /// Finite metric value when sampled cleanly, or `None` for a gap slot.
    pub value: Option<f32>,
}

impl GapAwareSparklinePoint {
    /// Creates a new [`GapAwareSparklinePoint`], normalizing non-finite floats
    /// (`NaN`, `±inf`) into `None`.
    #[inline]
    pub fn new(index: usize, value: Option<f32>) -> Self {
        Self {
            index,
            value: value.filter(|v| v.is_finite()),
        }
    }

    /// Returns `true` when this slot represents a missing/paused/error gap.
    #[inline]
    pub const fn is_gap(&self) -> bool {
        self.value.is_none()
    }
}

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

    /// Converts an iterator of permille values (`0..=1000`) into a fixed-length
    /// right-aligned window of percentage [`SparklinePoint`] values, left-padded
    /// with `0.0` so the newest sample always sits at the right edge.
    pub fn from_permille_padded(
        values: impl IntoIterator<Item = u16>,
        window_len: usize,
    ) -> Vec<Self> {
        Self::from_f32_padded(
            values
                .into_iter()
                .map(|permille| f32::from(permille) / 10.0),
            window_len,
        )
    }

    /// Converts an iterator of `f32` samples into a fixed-length right-aligned
    /// window of [`SparklinePoint`] values, left-padded with `0.0`.
    pub fn from_f32_padded(values: impl IntoIterator<Item = f32>, window_len: usize) -> Vec<Self> {
        if window_len == 0 {
            return Vec::new();
        }
        let samples: Vec<f32> = values.into_iter().collect();
        let mut points = Vec::with_capacity(window_len);
        if samples.len() >= window_len {
            let start = samples.len() - window_len;
            for (index, &value) in samples[start..].iter().enumerate() {
                points.push(Self { index, value });
            }
        } else {
            let pad = window_len - samples.len();
            for index in 0..pad {
                points.push(Self { index, value: 0.0 });
            }
            for (offset, &value) in samples.iter().enumerate() {
                points.push(Self {
                    index: pad + offset,
                    value,
                });
            }
        }
        points
    }

    /// Builds a fixed-length right-aligned window of [`GapAwareSparklinePoint`]
    /// entries from `Option<f32>` samples.
    ///
    /// Leading slots before enough samples exist, `None` entries, and non-finite
    /// float sentinels (`NaN`, `±inf`) are all preserved as `value: None` gaps
    /// rather than fabricated `0.0` points.
    pub fn from_opt_f32_window(
        values: impl IntoIterator<Item = Option<f32>>,
        window_len: usize,
    ) -> Vec<GapAwareSparklinePoint> {
        if window_len == 0 {
            return Vec::new();
        }
        let samples: Vec<Option<f32>> = values.into_iter().collect();
        let mut window = Vec::with_capacity(window_len);
        if samples.len() >= window_len {
            let start = samples.len() - window_len;
            for (index, &opt) in samples[start..].iter().enumerate() {
                window.push(GapAwareSparklinePoint::new(index, opt));
            }
        } else {
            let pad = window_len - samples.len();
            for index in 0..pad {
                window.push(GapAwareSparklinePoint::new(index, None));
            }
            for (offset, &opt) in samples.iter().enumerate() {
                window.push(GapAwareSparklinePoint::new(pad + offset, opt));
            }
        }
        window
    }

    /// Splits a right-aligned `Option<f32>` sample stream into contiguous
    /// valid [`SparklinePoint`] segments separated by gaps (`None` or
    /// non-finite sentinels).
    ///
    /// Preserves each point's true horizontal `index` within `0..window_len`
    /// so paused/error intervals break the line instead of fabricating flat
    /// zero lines or bogus connecting ramps.
    pub fn from_opt_f32_segments(
        values: impl IntoIterator<Item = Option<f32>>,
        window_len: usize,
    ) -> Vec<Vec<Self>> {
        let window = Self::from_opt_f32_window(values, window_len);
        let mut segments: Vec<Vec<Self>> = Vec::new();
        let mut current: Vec<Self> = Vec::new();

        for slot in window {
            match slot.value {
                Some(value) => current.push(Self::new(slot.index, value)),
                None => {
                    if !current.is_empty() {
                        segments.push(std::mem::take(&mut current));
                    }
                }
            }
        }
        if !current.is_empty() {
            segments.push(current);
        }
        segments
    }

    /// Splits a right-aligned `Option<u16>` permille (`0..=1000`) stream into
    /// contiguous percentage (`0.0..=100.0`) [`SparklinePoint`] segments
    /// separated by `None` gaps.
    pub fn from_opt_permille_segments(
        values: impl IntoIterator<Item = Option<u16>>,
        window_len: usize,
    ) -> Vec<Vec<Self>> {
        Self::from_opt_f32_segments(
            values
                .into_iter()
                .map(|opt| opt.map(|permille| f32::from(permille) / 10.0)),
            window_len,
        )
    }

    /// Splits a right-aligned `f32` sample stream into contiguous finite
    /// [`SparklinePoint`] segments, treating `NaN` and `±inf` sentinels as gaps.
    pub fn from_f32_gap_safe_segments(
        values: impl IntoIterator<Item = f32>,
        window_len: usize,
    ) -> Vec<Vec<Self>> {
        Self::from_opt_f32_segments(
            values
                .into_iter()
                .map(|v| if v.is_finite() { Some(v) } else { None }),
            window_len,
        )
    }

    /// Splits a right-aligned `u16` permille stream into contiguous percentage
    /// [`SparklinePoint`] segments, treating [`PERMILLE_GAP_SENTINEL`]
    /// (`u16::MAX`) as a gap.
    pub fn from_permille_sentinel_segments(
        values: impl IntoIterator<Item = u16>,
        window_len: usize,
    ) -> Vec<Vec<Self>> {
        Self::from_opt_permille_segments(
            values.into_iter().map(|permille| {
                if permille == PERMILLE_GAP_SENTINEL {
                    None
                } else {
                    Some(permille)
                }
            }),
            window_len,
        )
    }

    /// Builds a right-aligned `Vec<SparklinePoint>` from `Option<f32>` samples
    /// while omitting `None` and non-finite gap slots and preserving each valid
    /// sample's true horizontal `index` within `0..window_len`.
    pub fn from_opt_f32_padded(
        values: impl IntoIterator<Item = Option<f32>>,
        window_len: usize,
    ) -> Vec<Self> {
        Self::from_opt_f32_window(values, window_len)
            .into_iter()
            .filter_map(|slot| slot.value.map(|v| Self::new(slot.index, v)))
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
        .x(|point: &SparklinePoint| format_u32_shared(point.index as u32))
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

    let aria_label: SharedString = match (
        props.primary_metric.trim().is_empty(),
        props.subtitle.trim().is_empty(),
    ) {
        (true, true) => props.title.clone(),
        (false, true) => format!("{} {}", props.title, props.primary_metric).into(),
        (true, false) => format!("{} {}", props.title, props.subtitle).into(),
        (false, false) => format!(
            "{} {} {}",
            props.title, props.primary_metric, props.subtitle
        )
        .into(),
    };

    h_flex()
        .id(props.id)
        .role(Role::Button)
        .aria_label(aria_label)
        .w_full()
        .gap_2p5()
        .px_2p5()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(border)
        .bg(bg)
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
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .font_semibold()
                                .text_color(colors.text_primary)
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
                    div()
                        .w_full()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(colors.text_muted)
                        .child(props.subtitle),
                ),
        )
}

#[cfg(test)]
mod tests {
    use gpui_kit::component::{Theme, ThemeColor, ThemeMode};

    use crate::theme::LoomSemanticColors;

    use super::{
        GapAwareSparklinePoint, PERMILLE_GAP_SENTINEL, SparklineCardProps, SparklinePoint,
        sparkline_card,
    };

    #[test]
    fn sparkline_points_helpers() {
        let from_f32 = SparklinePoint::from_slice(&[10.0, 25.5, 80.0]);
        assert_eq!(
            from_f32,
            vec![
                SparklinePoint::new(0, 10.0),
                SparklinePoint::new(1, 25.5),
                SparklinePoint::new(2, 80.0),
            ]
        );

        let from_permille = SparklinePoint::from_permille_iter([0, 125, 1000]);
        assert_eq!(
            from_permille,
            vec![
                SparklinePoint::new(0, 0.0),
                SparklinePoint::new(1, 12.5),
                SparklinePoint::new(2, 100.0),
            ]
        );

        let padded = SparklinePoint::from_permille_padded([250, 500], 4);
        assert_eq!(
            padded,
            vec![
                SparklinePoint::new(0, 0.0),
                SparklinePoint::new(1, 0.0),
                SparklinePoint::new(2, 25.0),
                SparklinePoint::new(3, 50.0),
            ]
        );

        let truncated = SparklinePoint::from_f32_padded([1.0, 2.0, 3.0, 4.0], 2);
        assert_eq!(
            truncated,
            vec![SparklinePoint::new(0, 3.0), SparklinePoint::new(1, 4.0)]
        );
    }

    #[test]
    fn gap_aware_sparkline_helpers_and_card_construction() {
        let window =
            SparklinePoint::from_opt_f32_window([Some(10.0), None, Some(f32::NAN), Some(40.0)], 6);
        assert_eq!(window.len(), 6);
        assert!(window[0].is_gap());
        assert!(window[1].is_gap());
        assert_eq!(window[2], GapAwareSparklinePoint::new(2, Some(10.0)));
        assert!(window[3].is_gap());
        assert!(window[4].is_gap());
        assert_eq!(window[5], GapAwareSparklinePoint::new(5, Some(40.0)));

        let segments = SparklinePoint::from_opt_f32_segments(
            [
                Some(12.0),
                Some(18.0),
                None,
                Some(f32::INFINITY),
                Some(45.0),
            ],
            6,
        );
        assert_eq!(
            segments,
            vec![
                vec![SparklinePoint::new(1, 12.0), SparklinePoint::new(2, 18.0)],
                vec![SparklinePoint::new(5, 45.0)],
            ]
        );

        let permille_segs = SparklinePoint::from_permille_sentinel_segments(
            [100, PERMILLE_GAP_SENTINEL, 250, 500],
            4,
        );
        assert_eq!(
            permille_segs,
            vec![
                vec![SparklinePoint::new(0, 10.0)],
                vec![SparklinePoint::new(2, 25.0), SparklinePoint::new(3, 50.0)],
            ]
        );

        let finite_segs = SparklinePoint::from_f32_gap_safe_segments([5.0, f32::NAN, 15.0], 3);
        assert_eq!(
            finite_segs,
            vec![
                vec![SparklinePoint::new(0, 5.0)],
                vec![SparklinePoint::new(2, 15.0)],
            ]
        );

        let padded = SparklinePoint::from_opt_f32_padded([Some(10.0), None, Some(30.0)], 5);
        assert_eq!(
            padded,
            vec![SparklinePoint::new(2, 10.0), SparklinePoint::new(4, 30.0)]
        );

        let mut theme = Theme::from(ThemeColor::dark().as_ref());
        theme.mode = ThemeMode::Dark;
        let colors = LoomSemanticColors::from_theme(&theme);
        let _card = sparkline_card(
            SparklineCardProps {
                id: "cpu".into(),
                title: "CPU".into(),
                subtitle: "3.82 GHz".into(),
                primary_metric: "14%".into(),
                points: SparklinePoint::from_slice(&[5.0, 12.0, 14.0]),
                y_max: 100.0,
                accent_color: colors.chart_cpu,
                selected: true,
            },
            &colors,
        );
    }
}
