//! Semantic color tokens derived from [`gpui_kit::component::Theme`].

use gpui_kit::component::Theme;
use gpui_kit::{Hsla, hsla, transparent_black};

use crate::format::HeatmapLevel;

/// Semantic palette derived from the active `gpui-kit` [`Theme`].
///
/// Adapts luminance, contrast, and alpha levels cleanly across both
/// `ThemeMode::Light` and `ThemeMode::Dark`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoomSemanticColors {
    /// Whether this palette was derived for a dark theme.
    pub is_dark: bool,

    heatmap_low: Hsla,
    heatmap_medium: Hsla,
    heatmap_high: Hsla,
    heatmap_critical: Hsla,

    /// Background pill tint for Efficiency Mode / EcoQoS.
    pub status_eco_bg: Hsla,
    /// Foreground text/icon color for Efficiency Mode / EcoQoS (green leaf).
    pub status_eco_fg: Hsla,
    /// Background pill tint for suspended processes.
    pub status_suspended_bg: Hsla,
    /// Foreground text/icon color for suspended processes.
    pub status_suspended_fg: Hsla,
    /// Background pill tint for non-responsive / hung processes.
    pub status_hung_bg: Hsla,
    /// Foreground text/icon color for non-responsive / hung processes (red).
    pub status_hung_fg: Hsla,

    /// Memory composition bar color for actively used RAM.
    pub mem_in_use: Hsla,
    /// Memory composition bar color for modified pages awaiting write-back.
    pub mem_modified: Hsla,
    /// Memory composition bar color for standby cached RAM.
    pub mem_standby: Hsla,
    /// Memory composition bar color for free / zeroed RAM.
    pub mem_free: Hsla,

    /// Primary stroke/accent color for CPU total utilization charts.
    pub chart_cpu: Hsla,
    /// Secondary overlay color for CPU kernel-time charts.
    pub chart_cpu_kernel: Hsla,
    /// Stroke/accent color for Memory utilization charts.
    pub chart_memory: Hsla,
    /// Stroke/accent color for Disk activity/throughput charts.
    pub chart_disk: Hsla,
    /// Stroke/accent color for Network throughput charts.
    pub chart_network: Hsla,
    /// Stroke/accent color for GPU utilization/VRAM charts.
    pub chart_gpu: Hsla,

    /// Default surface background for selectable cards.
    pub card_bg: Hsla,
    /// Selected surface background for active cards.
    pub card_selected_bg: Hsla,
    /// Hover surface background for cards.
    pub card_hover_bg: Hsla,
    /// Default border color for cards and composition bars.
    pub card_border: Hsla,
    /// Highlighted border color for selected cards.
    pub card_selected_border: Hsla,
    /// Primary foreground text color.
    pub text_primary: Hsla,
    /// Muted secondary text color.
    pub text_muted: Hsla,
}

impl LoomSemanticColors {
    /// Derives Loom's semantic palette from the active [`Theme`].
    pub fn from_theme(theme: &Theme) -> Self {
        let is_dark = theme.is_dark();

        let (heatmap_low, heatmap_medium, heatmap_high, heatmap_critical) = if is_dark {
            (
                hsla(38.0 / 360.0, 0.85, 0.52, 0.16),
                hsla(31.0 / 360.0, 0.88, 0.50, 0.28),
                hsla(23.0 / 360.0, 0.90, 0.48, 0.40),
                hsla(14.0 / 360.0, 0.88, 0.46, 0.52),
            )
        } else {
            (
                hsla(38.0 / 360.0, 0.92, 0.56, 0.14),
                hsla(31.0 / 360.0, 0.93, 0.52, 0.24),
                hsla(23.0 / 360.0, 0.92, 0.49, 0.36),
                hsla(14.0 / 360.0, 0.90, 0.47, 0.48),
            )
        };

        let (status_eco_bg, status_eco_fg) = if is_dark {
            (
                hsla(142.0 / 360.0, 0.65, 0.42, 0.20),
                hsla(142.0 / 360.0, 0.72, 0.64, 1.0),
            )
        } else {
            (
                hsla(142.0 / 360.0, 0.65, 0.45, 0.14),
                hsla(142.0 / 360.0, 0.72, 0.28, 1.0),
            )
        };

        let (status_suspended_bg, status_suspended_fg) = if is_dark {
            (
                hsla(215.0 / 360.0, 0.28, 0.55, 0.22),
                hsla(215.0 / 360.0, 0.32, 0.78, 1.0),
            )
        } else {
            (
                hsla(215.0 / 360.0, 0.25, 0.50, 0.14),
                hsla(215.0 / 360.0, 0.32, 0.34, 1.0),
            )
        };

        let (status_hung_bg, status_hung_fg) = if is_dark {
            (hsla(0.0, 0.78, 0.54, 0.22), hsla(0.0, 0.85, 0.70, 1.0))
        } else {
            (hsla(0.0, 0.78, 0.52, 0.14), hsla(0.0, 0.75, 0.42, 1.0))
        };

        let (mem_in_use, mem_modified, mem_standby, mem_free) = if is_dark {
            (
                hsla(265.0 / 360.0, 0.72, 0.62, 1.0),
                hsla(33.0 / 360.0, 0.88, 0.56, 1.0),
                hsla(205.0 / 360.0, 0.65, 0.56, 0.65),
                theme.muted.opacity(0.65),
            )
        } else {
            (
                hsla(265.0 / 360.0, 0.68, 0.52, 1.0),
                hsla(30.0 / 360.0, 0.90, 0.50, 1.0),
                hsla(205.0 / 360.0, 0.72, 0.52, 0.50),
                theme.muted.opacity(0.85),
            )
        };

        let (chart_cpu, chart_cpu_kernel, chart_memory, chart_disk, chart_network, chart_gpu) =
            if is_dark {
                (
                    hsla(204.0 / 360.0, 0.86, 0.60, 1.0),
                    hsla(14.0 / 360.0, 0.86, 0.62, 1.0),
                    hsla(268.0 / 360.0, 0.76, 0.66, 1.0),
                    hsla(146.0 / 360.0, 0.68, 0.54, 1.0),
                    hsla(334.0 / 360.0, 0.78, 0.64, 1.0),
                    hsla(186.0 / 360.0, 0.76, 0.54, 1.0),
                )
            } else {
                (
                    hsla(204.0 / 360.0, 0.84, 0.44, 1.0),
                    hsla(12.0 / 360.0, 0.82, 0.48, 1.0),
                    hsla(268.0 / 360.0, 0.66, 0.48, 1.0),
                    hsla(146.0 / 360.0, 0.64, 0.38, 1.0),
                    hsla(334.0 / 360.0, 0.72, 0.48, 1.0),
                    hsla(186.0 / 360.0, 0.78, 0.38, 1.0),
                )
            };

        Self {
            is_dark,
            heatmap_low,
            heatmap_medium,
            heatmap_high,
            heatmap_critical,
            status_eco_bg,
            status_eco_fg,
            status_suspended_bg,
            status_suspended_fg,
            status_hung_bg,
            status_hung_fg,
            mem_in_use,
            mem_modified,
            mem_standby,
            mem_free,
            chart_cpu,
            chart_cpu_kernel,
            chart_memory,
            chart_disk,
            chart_network,
            chart_gpu,
            card_bg: theme.background,
            card_selected_bg: theme.list_active,
            card_hover_bg: theme.list_hover,
            card_border: theme.border,
            card_selected_border: theme.primary,
            text_primary: theme.foreground,
            text_muted: theme.muted_foreground,
        }
    }

    /// Returns the background tint for a given [`HeatmapLevel`].
    ///
    /// [`HeatmapLevel::None`] returns a fully transparent color so idle cells
    /// remain visually clean.
    #[inline]
    pub fn heatmap_bg(&self, level: HeatmapLevel) -> Hsla {
        match level {
            HeatmapLevel::None => transparent_black(),
            HeatmapLevel::Low => self.heatmap_low,
            HeatmapLevel::Medium => self.heatmap_medium,
            HeatmapLevel::High => self.heatmap_high,
            HeatmapLevel::Critical => self.heatmap_critical,
        }
    }
}

impl From<&Theme> for LoomSemanticColors {
    #[inline]
    fn from(theme: &Theme) -> Self {
        Self::from_theme(theme)
    }
}
