//! Compact icon-only process status indicators for Efficiency Mode, Suspended, and Not Responding states.

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, IconName, Sizable as _, h_flex};
use gpui_kit::{
    Hsla, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, px,
};
use loom_types::ProcessStatusFlags;

use crate::theme::LoomSemanticColors;

/// Asset path registered by host applications to serve [`ECO_LEAF_SVG`] through
/// GPUI's path-keyed SVG atlas cache.
pub const ECO_LEAF_ICON_PATH: &str = "icons/loom-eco-leaf.svg";

/// Embedded Lucide-style leaf vector used for Efficiency Mode (`EcoQoS`) badges.
///
/// Host applications should serve this slice for [`ECO_LEAF_ICON_PATH`] in their
/// [`gpui_kit::AssetSource`] so GPUI caches the rasterized glyph across frames
/// instead of copying raw SVG bytes on every render.
pub const ECO_LEAF_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11 20a10 10 0 0 0 10-10 25.9 25.9 0 0 0-1.04-7.281 1 1 0 0 0-1.755-.325C15.833 5.5 13 5.5 9.8 6.1A7 7 0 0 0 11 20"/><path d="M2 21a5 5 0 0 1 2.911-4.544C7.613 15.212 8.351 15.24 11 13"/></svg>"#;

/// Localized accessible tooltip labels passed to [`status_badges`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLabels {
    /// Localized label for Efficiency Mode / EcoQoS (e.g. `"Efficiency mode"` / `"效能模式"`).
    pub eco_mode: SharedString,
    /// Localized label for suspended processes (e.g. `"Suspended"` / `"已挂起"`).
    pub suspended: SharedString,
    /// Localized label for non-responsive UI processes (e.g. `"Not responding"` / `"未响应"`).
    pub not_responding: SharedString,
}

impl StatusLabels {
    /// Creates a new [`StatusLabels`] set from localized strings.
    pub fn new(
        eco_mode: impl Into<SharedString>,
        suspended: impl Into<SharedString>,
        not_responding: impl Into<SharedString>,
    ) -> Self {
        Self {
            eco_mode: eco_mode.into(),
            suspended: suspended.into(),
            not_responding: not_responding.into(),
        }
    }
}

/// Fast constant-time check indicating whether [`status_badges`] or
/// [`status_badges_with_a11y`] will emit at least one visible badge icon.
///
/// Callers in high-density tables can skip constructing an empty badge flex
/// container for the common case where none of `NOT_RESPONDING`, `SUSPENDED`,
/// or `EFFICIENCY_MODE` is set.
#[inline]
pub const fn has_visible_status_badges(status: ProcessStatusFlags) -> bool {
    status.intersects(
        ProcessStatusFlags::NOT_RESPONDING
            .union(ProcessStatusFlags::SUSPENDED)
            .union(ProcessStatusFlags::EFFICIENCY_MODE),
    )
}

fn status_icon_badge(
    id: (&'static str, usize),
    icon: Icon,
    label: SharedString,
    bg: Hsla,
    fg: Hsla,
    attach_a11y: bool,
) -> impl IntoElement {
    let tooltip_label = label.clone();
    let mut badge = h_flex()
        .id(id)
        .size(px(22.0))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(bg)
        .tooltip(move |window, cx| Tooltip::new(tooltip_label.clone()).build(window, cx))
        .child(icon.xsmall().text_color(fg));

    if attach_a11y {
        badge = badge.aria_label(label);
    }

    badge
}

/// Renders compact icon-only status badges (with hover tooltips) for any active
/// `EFFICIENCY_MODE`, `SUSPENDED`, or `NOT_RESPONDING` flags in [`ProcessStatusFlags`],
/// conditionally attaching `.aria_label(...)` only when `a11y_active` is `true`
/// or the `test-support` Cargo feature is enabled.
pub fn status_badges_with_a11y(
    row_id: usize,
    status: ProcessStatusFlags,
    labels: &StatusLabels,
    colors: &LoomSemanticColors,
    a11y_active: bool,
) -> impl IntoElement {
    let attach_a11y = a11y_active || cfg!(feature = "test-support");
    let mut row = h_flex().gap_1().items_center().overflow_hidden();

    if status.contains(ProcessStatusFlags::NOT_RESPONDING) {
        row = row.child(status_icon_badge(
            ("status-hung", row_id),
            Icon::new(IconName::TriangleAlert),
            labels.not_responding.clone(),
            colors.status_hung_bg,
            colors.status_hung_fg,
            attach_a11y,
        ));
    }

    if status.contains(ProcessStatusFlags::SUSPENDED) {
        row = row.child(status_icon_badge(
            ("status-suspended", row_id),
            Icon::new(IconName::Pause),
            labels.suspended.clone(),
            colors.status_suspended_bg,
            colors.status_suspended_fg,
            attach_a11y,
        ));
    }

    if status.contains(ProcessStatusFlags::EFFICIENCY_MODE) {
        row = row.child(status_icon_badge(
            ("status-eco", row_id),
            Icon::default().path(ECO_LEAF_ICON_PATH),
            labels.eco_mode.clone(),
            colors.status_eco_bg,
            colors.status_eco_fg,
            attach_a11y,
        ));
    }

    row
}

/// Renders compact icon-only status badges (with hover tooltips and ARIA labels)
/// for any active `EFFICIENCY_MODE`, `SUSPENDED`, or `NOT_RESPONDING` flags in
/// [`ProcessStatusFlags`].
pub fn status_badges(
    row_id: usize,
    status: ProcessStatusFlags,
    labels: &StatusLabels,
    colors: &LoomSemanticColors,
) -> impl IntoElement {
    status_badges_with_a11y(row_id, status, labels, colors, true)
}

#[cfg(test)]
mod tests {
    use gpui_kit::component::{Theme, ThemeColor, ThemeMode};
    use loom_types::ProcessStatusFlags;

    use crate::theme::LoomSemanticColors;

    use super::{StatusLabels, has_visible_status_badges, status_badges, status_badges_with_a11y};

    #[test]
    fn status_badge_fast_check_and_construction() {
        assert!(!has_visible_status_badges(ProcessStatusFlags::empty()));
        assert!(!has_visible_status_badges(
            ProcessStatusFlags::ELEVATED
                | ProcessStatusFlags::PROTECTED
                | ProcessStatusFlags::CRITICAL
                | ProcessStatusFlags::SYSTEM_IDLE
                | ProcessStatusFlags::KERNEL
        ));
        assert!(has_visible_status_badges(
            ProcessStatusFlags::NOT_RESPONDING
        ));
        assert!(has_visible_status_badges(ProcessStatusFlags::SUSPENDED));
        assert!(has_visible_status_badges(
            ProcessStatusFlags::EFFICIENCY_MODE
        ));
        assert!(has_visible_status_badges(
            ProcessStatusFlags::ELEVATED | ProcessStatusFlags::EFFICIENCY_MODE
        ));

        let mut theme = Theme::from(ThemeColor::dark().as_ref());
        theme.mode = ThemeMode::Dark;
        let colors = LoomSemanticColors::from_theme(&theme);
        let labels = StatusLabels::new("Efficiency mode", "Suspended", "Not responding");

        let _all = status_badges(
            42,
            ProcessStatusFlags::EFFICIENCY_MODE
                | ProcessStatusFlags::SUSPENDED
                | ProcessStatusFlags::NOT_RESPONDING,
            &labels,
            &colors,
        );
        let _no_a11y = status_badges_with_a11y(
            7,
            ProcessStatusFlags::EFFICIENCY_MODE,
            &labels,
            &colors,
            false,
        );
        let _with_a11y = status_badges_with_a11y(
            7,
            ProcessStatusFlags::SUSPENDED | ProcessStatusFlags::NOT_RESPONDING,
            &labels,
            &colors,
            true,
        );
    }
}
