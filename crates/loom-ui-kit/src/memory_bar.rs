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
