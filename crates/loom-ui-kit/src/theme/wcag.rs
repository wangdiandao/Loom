//! Pure-Rust sRGB/HSLA color math and W3C WCAG 2.1 contrast verification solvers.

use gpui_kit::{Hsla, hsla};

/// Minimum WCAG 2.1 AA contrast ratio for graphical objects and UI components
/// (`3.0:1`).
pub const WCAG_AA_UI_CONTRAST: f32 = 3.0;

/// Minimum WCAG 2.1 AA contrast ratio for normal-sized text (`4.5:1`).
pub const WCAG_AA_TEXT_CONTRAST: f32 = 4.5;

/// Minimum WCAG 2.1 AAA contrast ratio for enhanced text readability (`7.0:1`).
pub const WCAG_AAA_TEXT_CONTRAST: f32 = 7.0;

/// Converts a normalized sRGB `(r, g, b)` color (`0.0..=1.0`) to an opaque [`Hsla`].
pub(super) fn srgb_to_hsla(rgb: (f32, f32, f32)) -> Hsla {
    let r = rgb.0.clamp(0.0, 1.0);
    let g = rgb.1.clamp(0.0, 1.0);
    let b = rgb.2.clamp(0.0, 1.0);

    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let l = 0.5 * (max + min);

    if delta <= 1e-6 {
        return hsla(0.0, 0.0, l, 1.0);
    }

    let s = if l > 0.5 {
        delta / (2.0 - max - min)
    } else {
        delta / (max + min)
    };

    let h_deg = if (max - r).abs() <= 1e-6 {
        60.0 * (((g - b) / delta).rem_euclid(6.0))
    } else if (max - g).abs() <= 1e-6 {
        60.0 * (((b - r) / delta) + 2.0)
    } else {
        60.0 * (((r - g) / delta) + 4.0)
    };

    hsla(
        (h_deg / 360.0).rem_euclid(1.0),
        s.clamp(0.0, 1.0),
        l.clamp(0.0, 1.0),
        1.0,
    )
}

/// Converts an 8-bit sRGB `(r, g, b)` tuple to an opaque GPUI [`Hsla`] color.
#[inline]
pub fn rgb_to_hsla(rgb: (u8, u8, u8)) -> Hsla {
    srgb_to_hsla((
        f32::from(rgb.0) / 255.0,
        f32::from(rgb.1) / 255.0,
        f32::from(rgb.2) / 255.0,
    ))
}

/// Converts a GPUI [`Hsla`] color (ignoring alpha) to an 8-bit sRGB `(r, g, b)` tuple.
pub fn hsla_to_rgb_u8(color: Hsla) -> (u8, u8, u8) {
    let (r, g, b) = hsla_to_srgb(color);
    (
        (r * 255.0).round().clamp(0.0, 255.0) as u8,
        (g * 255.0).round().clamp(0.0, 255.0) as u8,
        (b * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

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
pub(super) fn alpha_composite_srgb(
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
