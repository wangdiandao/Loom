//! Four-segment Flexbox physical memory composition bar with per-segment tooltips.

use gpui_kit::component::h_flex;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, div, px, relative,
};

use crate::format::format_bytes;
use crate::theme::LoomSemanticColors;

/// Physical memory breakdown across the four Windows-style composition buckets.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MemoryBarSegments {
    /// Actively used physical memory in bytes.
    pub in_use_bytes: u64,
    /// Modified pages awaiting write-back in bytes.
    pub modified_bytes: u64,
    /// Cached standby pages in bytes.
    pub standby_bytes: u64,
    /// Completely free physical memory in bytes.
    pub free_bytes: u64,
}

impl MemoryBarSegments {
    /// Returns the saturating total across all four segments in bytes.
    #[inline]
    pub const fn total_bytes(&self) -> u64 {
        self.in_use_bytes
            .saturating_add(self.modified_bytes)
            .saturating_add(self.standby_bytes)
            .saturating_add(self.free_bytes)
    }

    /// Computes normalized segment proportions `[in_use, modified, standby, free]`
    /// summing to `1.0`.
    ///
    /// When [`Self::total_bytes`] is `0`, returns `[0.0, 0.0, 0.0, 1.0]` so the
    /// rendered bar displays an empty free track rather than collapsing.
    pub fn normalized_weights(&self) -> [f32; 4] {
        let total = self.total_bytes();
        if total == 0 {
            return [0.0, 0.0, 0.0, 1.0];
        }
        let denom = total as f64;
        [
            (self.in_use_bytes as f64 / denom) as f32,
            (self.modified_bytes as f64 / denom) as f32,
            (self.standby_bytes as f64 / denom) as f32,
            (self.free_bytes as f64 / denom) as f32,
        ]
    }
}

/// Localized labels for the four [`MemoryBarSegments`] tooltips.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryBarLabels {
    /// Label for actively used memory (e.g. `"In use"` / `"使用中"`).
    pub in_use: SharedString,
    /// Label for modified memory (e.g. `"Modified"` / `"已修改"`).
    pub modified: SharedString,
    /// Label for standby cached memory (e.g. `"Standby"` / `"备用"`).
    pub standby: SharedString,
    /// Label for free memory (e.g. `"Free"` / `"可用"`).
    pub free: SharedString,
}

impl MemoryBarLabels {
    /// Creates a new [`MemoryBarLabels`] instance from localized strings.
    pub fn new(
        in_use: impl Into<SharedString>,
        modified: impl Into<SharedString>,
        standby: impl Into<SharedString>,
        free: impl Into<SharedString>,
    ) -> Self {
        Self {
            in_use: in_use.into(),
            modified: modified.into(),
            standby: standby.into(),
            free: free.into(),
        }
    }
}

/// Renders a four-segment proportional memory composition bar using Flexbox
/// widths and interactive [`Tooltip`] popups on each non-empty segment.
pub fn memory_composition_bar(
    id: impl Into<SharedString>,
    segments: MemoryBarSegments,
    labels: &MemoryBarLabels,
    colors: &LoomSemanticColors,
) -> impl IntoElement {
    let base_id = id.into();
    let weights = segments.normalized_weights();

    let entries = [
        (
            "in-use",
            &labels.in_use,
            segments.in_use_bytes,
            weights[0],
            colors.mem_in_use,
        ),
        (
            "modified",
            &labels.modified,
            segments.modified_bytes,
            weights[1],
            colors.mem_modified,
        ),
        (
            "standby",
            &labels.standby,
            segments.standby_bytes,
            weights[2],
            colors.mem_standby,
        ),
        (
            "free",
            &labels.free,
            segments.free_bytes,
            weights[3],
            colors.mem_free,
        ),
    ];

    let mut bar = h_flex()
        .w_full()
        .h(px(28.0))
        .rounded_md()
        .overflow_hidden()
        .border_1()
        .border_color(colors.card_border);

    for (suffix, label, bytes, weight, color) in entries {
        if weight <= 0.0 {
            continue;
        }
        let seg_id = SharedString::from(format!("{base_id}-{suffix}"));
        let tip = SharedString::from(format!(
            "{label}: {} ({:.1}%)",
            format_bytes(bytes),
            weight * 100.0
        ));
        bar = bar.child(
            div()
                .id(seg_id)
                .h_full()
                .w(relative(weight))
                .bg(color)
                .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx)),
        );
    }

    bar
}

#[cfg(test)]
mod tests {
    use gpui_kit::component::{Theme, ThemeColor, ThemeMode};

    use crate::theme::LoomSemanticColors;

    use super::{MemoryBarLabels, MemoryBarSegments, memory_composition_bar};

    #[test]
    fn memory_bar_segment_proportions_and_construction() {
        let empty = MemoryBarSegments::default();
        assert_eq!(empty.total_bytes(), 0);
        assert_eq!(empty.normalized_weights(), [0.0, 0.0, 0.0, 1.0]);

        let segments = MemoryBarSegments {
            in_use_bytes: 50,
            modified_bytes: 10,
            standby_bytes: 25,
            free_bytes: 15,
        };
        assert_eq!(segments.total_bytes(), 100);
        let weights = segments.normalized_weights();
        assert!((weights[0] - 0.50).abs() < 1e-6);
        assert!((weights[1] - 0.10).abs() < 1e-6);
        assert!((weights[2] - 0.25).abs() < 1e-6);
        assert!((weights[3] - 0.15).abs() < 1e-6);
        let sum: f32 = weights.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6);

        let huge = MemoryBarSegments {
            in_use_bytes: u64::MAX,
            modified_bytes: 100,
            standby_bytes: 0,
            free_bytes: 0,
        };
        assert_eq!(huge.total_bytes(), u64::MAX);

        let mut theme = Theme::from(ThemeColor::dark().as_ref());
        theme.mode = ThemeMode::Dark;
        let colors = LoomSemanticColors::from_theme(&theme);
        let labels = MemoryBarLabels::new("In use", "Modified", "Standby", "Free");
        let _bar = memory_composition_bar("mem-bar", segments, &labels, &colors);
    }
}
