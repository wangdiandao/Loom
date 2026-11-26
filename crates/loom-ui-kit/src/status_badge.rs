//! Compact icon-only process status indicators for Efficiency Mode, Suspended, and Not Responding states.

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, IconName, Sizable as _, h_flex};
use gpui_kit::{
    Hsla, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, px,
};
use loom_types::ProcessStatusFlags;

use crate::theme::LoomSemanticColors;

/// Embedded Lucide-style leaf vector used for Efficiency Mode (`EcoQoS`) badges.
///
/// Embedded directly via [`Icon::data`] so the leaf badge always renders even
/// when the host application registers only the lightweight default asset bundle.
const ECO_LEAF_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11 20a10 10 0 0 0 10-10 25.9 25.9 0 0 0-1.04-7.281 1 1 0 0 0-1.755-.325C15.833 5.5 13 5.5 9.8 6.1A7 7 0 0 0 11 20"/><path d="M2 21a5 5 0 0 1 2.911-4.544C7.613 15.212 8.351 15.24 11 13"/></svg>"#;

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

fn status_icon_badge(
    id: (&'static str, usize),
    icon: Icon,
    label: SharedString,
    bg: Hsla,
    fg: Hsla,
) -> impl IntoElement {
    let tooltip_label = label.clone();
    h_flex()
        .id(id)
        .size(px(22.0))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(bg)
        .aria_label(label)
        .tooltip(move |window, cx| Tooltip::new(tooltip_label.clone()).build(window, cx))
        .child(icon.xsmall().text_color(fg))
}

/// Renders compact icon-only status badges (with hover tooltips) for any active
/// `EFFICIENCY_MODE`, `SUSPENDED`, or `NOT_RESPONDING` flags in [`ProcessStatusFlags`].
pub fn status_badges(
    row_id: usize,
    status: ProcessStatusFlags,
    labels: &StatusLabels,
    colors: &LoomSemanticColors,
) -> impl IntoElement {
    let mut row = h_flex().gap_1().items_center().overflow_hidden();

    if status.contains(ProcessStatusFlags::NOT_RESPONDING) {
        row = row.child(status_icon_badge(
            ("status-hung", row_id),
            Icon::new(IconName::TriangleAlert),
            labels.not_responding.clone(),
            colors.status_hung_bg,
            colors.status_hung_fg,
        ));
    }

    if status.contains(ProcessStatusFlags::SUSPENDED) {
        row = row.child(status_icon_badge(
            ("status-suspended", row_id),
            Icon::new(IconName::Pause),
            labels.suspended.clone(),
            colors.status_suspended_bg,
            colors.status_suspended_fg,
        ));
    }

    if status.contains(ProcessStatusFlags::EFFICIENCY_MODE) {
        row = row.child(status_icon_badge(
            ("status-eco", row_id),
            Icon::default().data(ECO_LEAF_SVG),
            labels.eco_mode.clone(),
            colors.status_eco_bg,
            colors.status_eco_fg,
        ));
    }

    row
}
