//! Semantic color palette derived from [`Theme`] with theme-keyed caching.

use gpui_kit::component::Theme;
use gpui_kit::{Hsla, hsla, transparent_black};

use crate::format::HeatmapLevel;

use super::wcag::{WCAG_AA_TEXT_CONTRAST, WCAG_AA_UI_CONTRAST, ensure_wcag_contrast};

/// Semantic palette derived from the active `gpui-kit` [`Theme`].
///
/// Adapts luminance, contrast, and alpha levels cleanly across both
/// `ThemeMode::Light` and `ThemeMode::Dark`, enforcing WCAG 2.1 contrast
/// guarantees on text, badge, and chart accent tokens.
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
    /// Semantic accent color for Performance (P-core) logical CPU cards/badges.
    pub cpu_core_p: Hsla,
    /// Semantic accent color for Efficiency (E-core) logical CPU cards/badges.
    pub cpu_core_e: Hsla,
    /// Semantic accent color for Low-Power Efficiency (LP-E core) logical CPU cards/badges.
    pub cpu_core_lp_e: Hsla,
    /// Stroke/accent color for Memory utilization charts.
    pub chart_memory: Hsla,
    /// Stroke/accent color for Disk activity and read throughput charts.
    pub chart_disk: Hsla,
    /// Secondary stroke/accent color for Disk write throughput charts.
    pub chart_disk_write: Hsla,
    /// Stroke/accent color for Network receive throughput charts.
    pub chart_network: Hsla,
    /// Secondary stroke/accent color for Network send throughput charts.
    pub chart_network_send: Hsla,
    /// Stroke/accent color for GPU utilization and dedicated VRAM charts.
    pub chart_gpu: Hsla,
    /// Secondary stroke/accent color for GPU shared system memory charts.
    pub chart_gpu_shared: Hsla,

    /// Semantic warning accent for sustained memory growth / potential leak indicators.
    pub leak_warning: Hsla,
    /// Semantic indicator accent when live table sorting is temporarily paused.
    pub sort_paused: Hsla,

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
        let surface = theme.background;

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

        let (status_eco_bg, status_eco_fg_raw) = if is_dark {
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

        let (status_suspended_bg, status_suspended_fg_raw) = if is_dark {
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

        let (status_hung_bg, status_hung_fg_raw) = if is_dark {
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

        let (
            chart_cpu_raw,
            chart_cpu_kernel_raw,
            cpu_core_p_raw,
            cpu_core_e_raw,
            cpu_core_lp_e_raw,
            chart_memory_raw,
            chart_disk_raw,
            chart_disk_write_raw,
            chart_network_raw,
            chart_network_send_raw,
            chart_gpu_raw,
            chart_gpu_shared_raw,
            leak_warning_raw,
            sort_paused_raw,
        ) = if is_dark {
            (
                hsla(204.0 / 360.0, 0.86, 0.60, 1.0),
                hsla(14.0 / 360.0, 0.86, 0.62, 1.0),
                hsla(208.0 / 360.0, 0.88, 0.64, 1.0),
                hsla(156.0 / 360.0, 0.72, 0.56, 1.0),
                hsla(276.0 / 360.0, 0.76, 0.70, 1.0),
                hsla(268.0 / 360.0, 0.76, 0.66, 1.0),
                hsla(146.0 / 360.0, 0.68, 0.54, 1.0),
                hsla(42.0 / 360.0, 0.86, 0.60, 1.0),
                hsla(334.0 / 360.0, 0.78, 0.64, 1.0),
                hsla(24.0 / 360.0, 0.88, 0.62, 1.0),
                hsla(186.0 / 360.0, 0.76, 0.54, 1.0),
                hsla(220.0 / 360.0, 0.74, 0.64, 1.0),
                hsla(16.0 / 360.0, 0.90, 0.64, 1.0),
                hsla(44.0 / 360.0, 0.92, 0.62, 1.0),
            )
        } else {
            (
                hsla(204.0 / 360.0, 0.84, 0.44, 1.0),
                hsla(12.0 / 360.0, 0.82, 0.48, 1.0),
                hsla(208.0 / 360.0, 0.84, 0.42, 1.0),
                hsla(156.0 / 360.0, 0.72, 0.34, 1.0),
                hsla(276.0 / 360.0, 0.68, 0.46, 1.0),
                hsla(268.0 / 360.0, 0.66, 0.48, 1.0),
                hsla(146.0 / 360.0, 0.64, 0.38, 1.0),
                hsla(36.0 / 360.0, 0.84, 0.44, 1.0),
                hsla(334.0 / 360.0, 0.72, 0.48, 1.0),
                hsla(22.0 / 360.0, 0.84, 0.46, 1.0),
                hsla(186.0 / 360.0, 0.78, 0.38, 1.0),
                hsla(220.0 / 360.0, 0.68, 0.48, 1.0),
                hsla(14.0 / 360.0, 0.84, 0.42, 1.0),
                hsla(34.0 / 360.0, 0.88, 0.34, 1.0),
            )
        };

        // Enforce WCAG 2.1 AA contrast guarantees:
        // - >= 4.5:1 for text & status foreground tokens against the card surface
        // - >= 3.0:1 for graphical chart & badge accent tokens against the card surface
        let status_eco_fg = ensure_wcag_contrast(status_eco_fg_raw, surface, WCAG_AA_TEXT_CONTRAST);
        let status_suspended_fg =
            ensure_wcag_contrast(status_suspended_fg_raw, surface, WCAG_AA_TEXT_CONTRAST);
        let status_hung_fg =
            ensure_wcag_contrast(status_hung_fg_raw, surface, WCAG_AA_TEXT_CONTRAST);

        let chart_cpu = ensure_wcag_contrast(chart_cpu_raw, surface, WCAG_AA_UI_CONTRAST);
        let chart_cpu_kernel =
            ensure_wcag_contrast(chart_cpu_kernel_raw, surface, WCAG_AA_UI_CONTRAST);
        let cpu_core_p = ensure_wcag_contrast(cpu_core_p_raw, surface, WCAG_AA_UI_CONTRAST);
        let cpu_core_e = ensure_wcag_contrast(cpu_core_e_raw, surface, WCAG_AA_UI_CONTRAST);
        let cpu_core_lp_e = ensure_wcag_contrast(cpu_core_lp_e_raw, surface, WCAG_AA_UI_CONTRAST);
        let chart_memory = ensure_wcag_contrast(chart_memory_raw, surface, WCAG_AA_UI_CONTRAST);
        let chart_disk = ensure_wcag_contrast(chart_disk_raw, surface, WCAG_AA_UI_CONTRAST);
        let chart_disk_write =
            ensure_wcag_contrast(chart_disk_write_raw, surface, WCAG_AA_UI_CONTRAST);
        let chart_network = ensure_wcag_contrast(chart_network_raw, surface, WCAG_AA_UI_CONTRAST);
        let chart_network_send =
            ensure_wcag_contrast(chart_network_send_raw, surface, WCAG_AA_UI_CONTRAST);
        let chart_gpu = ensure_wcag_contrast(chart_gpu_raw, surface, WCAG_AA_UI_CONTRAST);
        let chart_gpu_shared =
            ensure_wcag_contrast(chart_gpu_shared_raw, surface, WCAG_AA_UI_CONTRAST);

        let leak_warning = ensure_wcag_contrast(leak_warning_raw, surface, WCAG_AA_TEXT_CONTRAST);
        let sort_paused = ensure_wcag_contrast(sort_paused_raw, surface, WCAG_AA_TEXT_CONTRAST);

        let text_primary = ensure_wcag_contrast(theme.foreground, surface, WCAG_AA_TEXT_CONTRAST);
        let text_muted =
            ensure_wcag_contrast(theme.muted_foreground, surface, WCAG_AA_TEXT_CONTRAST);

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
            cpu_core_p,
            cpu_core_e,
            cpu_core_lp_e,
            chart_memory,
            chart_disk,
            chart_disk_write,
            chart_network,
            chart_network_send,
            chart_gpu,
            chart_gpu_shared,
            leak_warning,
            sort_paused,
            card_bg: surface,
            card_selected_bg: theme.list_active,
            card_hover_bg: theme.list_hover,
            card_border: theme.border,
            card_selected_border: theme.primary,
            text_primary,
            text_muted,
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

/// Compact fingerprint of the [`Theme`] fields that feed [`LoomSemanticColors::from_theme`].
#[derive(Debug, Clone, Copy, PartialEq)]
struct ThemePaletteKey {
    is_dark: bool,
    background: Hsla,
    foreground: Hsla,
    muted: Hsla,
    muted_foreground: Hsla,
    primary: Hsla,
    border: Hsla,
    list_active: Hsla,
    list_hover: Hsla,
}

impl ThemePaletteKey {
    #[inline]
    fn from_theme(theme: &Theme) -> Self {
        Self {
            is_dark: theme.is_dark(),
            background: theme.background,
            foreground: theme.foreground,
            muted: theme.muted,
            muted_foreground: theme.muted_foreground,
            primary: theme.primary,
            border: theme.border,
            list_active: theme.list_active,
            list_hover: theme.list_hover,
        }
    }
}

/// Theme-keyed cache around [`LoomSemanticColors::from_theme`].
///
/// Computing WCAG-verified semantic colors performs iterative binary-search
/// luminance calculations (`powf(2.4)`). Calling `from_theme` per table cell
/// wastes tens of thousands of floating-point power calls per frame. Storing a
/// [`CachedSemanticColors`] inside a view or table delegate recomputes the
/// palette only when the active theme's visual tokens actually change.
#[derive(Debug, Default, Clone)]
pub struct CachedSemanticColors {
    cached: Option<(ThemePaletteKey, LoomSemanticColors)>,
}

impl CachedSemanticColors {
    /// Creates an empty [`CachedSemanticColors`] cache.
    #[inline]
    pub const fn new() -> Self {
        Self { cached: None }
    }

    /// Returns a reference to the cached [`LoomSemanticColors`] for `theme`,
    /// recomputing only if the relevant theme fields changed since the last call.
    #[inline]
    pub fn get(&mut self, theme: &Theme) -> &LoomSemanticColors {
        let key = ThemePaletteKey::from_theme(theme);
        if let Some((prev_key, _)) = &self.cached
            && *prev_key != key
        {
            self.cached = None;
        }
        &self
            .cached
            .get_or_insert_with(|| (key, LoomSemanticColors::from_theme(theme)))
            .1
    }

    /// Convenience wrapper around [`Self::get`] returning the [`Copy`] palette
    /// by value, avoiding borrow conflicts inside GPUI table delegates.
    #[inline]
    pub fn get_copy(&mut self, theme: &Theme) -> LoomSemanticColors {
        *self.get(theme)
    }

    /// Clears the cached palette so the next call recomputes from scratch.
    #[inline]
    pub fn invalidate(&mut self) {
        self.cached = None;
    }
}
