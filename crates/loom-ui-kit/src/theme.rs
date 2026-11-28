//! Semantic color tokens derived from [`gpui_kit::component::Theme`] and
//! pure-Rust WCAG 2.1 contrast verification utilities.

use gpui_kit::component::Theme;
use gpui_kit::{Hsla, hsla, transparent_black};

use crate::format::HeatmapLevel;

/// Minimum WCAG 2.1 AA contrast ratio for graphical objects and UI components
/// (`3.0:1`).
pub const WCAG_AA_UI_CONTRAST: f32 = 3.0;

/// Minimum WCAG 2.1 AA contrast ratio for normal-sized text (`4.5:1`).
pub const WCAG_AA_TEXT_CONTRAST: f32 = 4.5;

/// Minimum WCAG 2.1 AAA contrast ratio for enhanced text readability (`7.0:1`).
pub const WCAG_AAA_TEXT_CONTRAST: f32 = 7.0;

/// Linearizes a single sRGB channel (`0.0..=1.0`) per W3C WCAG 2.1.
#[inline]
fn linearize_srgb_channel(channel: f32) -> f32 {
    let c = if channel.is_finite() {
        channel.clamp(0.0, 1.0)
    } else {
        0.0
    };
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Computes the W3C WCAG 2.1 relative luminance (`0.0..=1.0`) of an sRGB color
/// whose channels are in `0.0..=1.0`.
#[inline]
pub fn srgb_relative_luminance(r: f32, g: f32, b: f32) -> f32 {
    let r_lin = linearize_srgb_channel(r);
    let g_lin = linearize_srgb_channel(g);
    let b_lin = linearize_srgb_channel(b);
    0.2126 * r_lin + 0.7152 * g_lin + 0.0722 * b_lin
}

/// Computes the W3C WCAG 2.1 contrast ratio (`1.0..=21.0`) between two sRGB
/// colors (`(r, g, b)` with channels in `0.0..=1.0`).
#[inline]
pub fn wcag_contrast_ratio(fg: (f32, f32, f32), bg: (f32, f32, f32)) -> f32 {
    let l1 = srgb_relative_luminance(fg.0, fg.1, fg.2);
    let l2 = srgb_relative_luminance(bg.0, bg.1, bg.2);
    let lighter = l1.max(l2);
    let darker = l1.min(l2);
    (lighter + 0.05) / (darker + 0.05)
}

/// Converts a GPUI [`Hsla`] color (ignoring alpha) to sRGB `(r, g, b)` in
/// `0.0..=1.0`.
pub fn hsla_to_srgb(color: Hsla) -> (f32, f32, f32) {
    let h = if color.h.is_finite() {
        color.h.rem_euclid(1.0)
    } else {
        0.0
    };
    let s = if color.s.is_finite() {
        color.s.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let l = if color.l.is_finite() {
        color.l.clamp(0.0, 1.0)
    } else {
        0.0
    };

    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h6 = h * 6.0;
    let x = c * (1.0 - (h6 % 2.0 - 1.0).abs());
    let m = l - c * 0.5;

    let (r1, g1, b1) = if h6 < 1.0 {
        (c, x, 0.0)
    } else if h6 < 2.0 {
        (x, c, 0.0)
    } else if h6 < 3.0 {
        (0.0, c, x)
    } else if h6 < 4.0 {
        (0.0, x, c)
    } else if h6 < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };

    (
        (r1 + m).clamp(0.0, 1.0),
        (g1 + m).clamp(0.0, 1.0),
        (b1 + m).clamp(0.0, 1.0),
    )
}

/// Alpha-composites an sRGB foreground with opacity `alpha` over an opaque sRGB
/// background.
#[inline]
pub(crate) fn alpha_composite_srgb(
    fg: (f32, f32, f32),
    alpha: f32,
    bg: (f32, f32, f32),
) -> (f32, f32, f32) {
    let a = if alpha.is_finite() {
        alpha.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let inv = 1.0 - a;
    (
        (fg.0 * a + bg.0 * inv).clamp(0.0, 1.0),
        (fg.1 * a + bg.1 * inv).clamp(0.0, 1.0),
        (fg.2 * a + bg.2 * inv).clamp(0.0, 1.0),
    )
}

/// Adjusts `fg`'s lightness monotonically toward white (`l -> 1.0`) or black
/// (`l -> 0.0`) until its WCAG 2.1 contrast ratio against `bg` is at least
/// `min_ratio`, preserving hue, saturation, and alpha as closely as possible.
///
/// If `fg` already meets `min_ratio` (or `min_ratio <= 1.0`), `fg` is returned
/// unchanged. If neither pure white nor pure black can satisfy `min_ratio`
/// against `bg` (for example, `min_ratio == 21.0` on a mid-tone surface),
/// returns whichever endpoint (`l = 1.0` or `l = 0.0`) achieves the highest
/// possible contrast.
pub fn ensure_wcag_contrast(fg: Hsla, bg: Hsla, min_ratio: f32) -> Hsla {
    if !min_ratio.is_finite() || min_ratio <= 1.0 {
        return fg;
    }

    let bg_rgb = hsla_to_srgb(bg);
    let fg_alpha = if fg.a.is_finite() {
        fg.a.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let effective_fg = alpha_composite_srgb(hsla_to_srgb(fg), fg_alpha, bg_rgb);
    if wcag_contrast_ratio(effective_fg, bg_rgb) >= min_ratio {
        return fg;
    }

    let eval_lum = |lightness: f32| -> f32 {
        let candidate = hsla(fg.h, fg.s, lightness, fg_alpha);
        let composited = alpha_composite_srgb(hsla_to_srgb(candidate), fg_alpha, bg_rgb);
        srgb_relative_luminance(composited.0, composited.1, composited.2)
    };

    let bg_lum = srgb_relative_luminance(bg_rgb.0, bg_rgb.1, bg_rgb.2);
    let lum_at_white = eval_lum(1.0);
    let lum_at_black = eval_lum(0.0);

    let required_bright_lum = min_ratio * (bg_lum + 0.05) - 0.05;
    let required_dark_lum = (bg_lum + 0.05) / min_ratio - 0.05;

    let can_go_lighter = required_bright_lum <= lum_at_white + 1e-6;
    let can_go_darker = required_dark_lum >= lum_at_black - 1e-6;

    // Binary search works monotonically because relative luminance is strictly
    // non-decreasing with HSL lightness for fixed (h, s, alpha).
    let search_lighter = || -> f32 {
        let mut lo = 0.0_f32;
        let mut hi = 1.0_f32;
        for _ in 0..32 {
            let mid = 0.5 * (lo + hi);
            if eval_lum(mid) >= required_bright_lum {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        hi
    };

    let search_darker = || -> f32 {
        let mut lo = 0.0_f32;
        let mut hi = 1.0_f32;
        for _ in 0..32 {
            let mid = 0.5 * (lo + hi);
            if eval_lum(mid) <= required_dark_lum {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    };

    let orig_l = if fg.l.is_finite() {
        fg.l.clamp(0.0, 1.0)
    } else {
        0.5
    };

    let chosen_l = match (can_go_lighter, can_go_darker) {
        (true, true) => {
            let l_light = search_lighter();
            let l_dark = search_darker();
            let dist_light = (l_light - orig_l).abs();
            let dist_dark = (l_dark - orig_l).abs();
            if (dist_light - dist_dark).abs() < 1e-4 {
                // Tie-break toward lighter on dark surfaces, darker on light surfaces.
                if bg_lum < 0.179_128_78 {
                    l_light
                } else {
                    l_dark
                }
            } else if dist_light < dist_dark {
                l_light
            } else {
                l_dark
            }
        }
        (true, false) => search_lighter(),
        (false, true) => search_darker(),
        (false, false) => {
            let white_ratio = (lum_at_white.max(bg_lum) + 0.05) / (lum_at_white.min(bg_lum) + 0.05);
            let black_ratio = (lum_at_black.max(bg_lum) + 0.05) / (lum_at_black.min(bg_lum) + 0.05);
            if white_ratio >= black_ratio { 1.0 } else { 0.0 }
        }
    };

    hsla(fg.h, fg.s, chosen_l, fg_alpha)
}

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
