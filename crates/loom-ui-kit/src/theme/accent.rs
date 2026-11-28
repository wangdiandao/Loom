//! Accent color presets and WCAG AA-compliant theme accent derivation.

use gpui_kit::component::Theme;
use gpui_kit::{Hsla, hsla};

use super::wcag::{
    WCAG_AA_TEXT_CONTRAST, WCAG_AA_UI_CONTRAST, alpha_composite_srgb, ensure_wcag_contrast,
    hsla_to_srgb, rgb_to_hsla, srgb_to_hsla, wcag_contrast_ratio,
};

/// Default fallback sRGB accent color (`#0078D4`) used when the operating system
/// does not expose a system accent color.
pub const DEFAULT_ACCENT_RGB: (u8, u8, u8) = (0x00, 0x78, 0xD4);

/// Curated quick-pick accent color presets `(slug, (r, g, b))`.
pub const ACCENT_COLOR_PRESETS: [(&str, (u8, u8, u8)); 8] = [
    ("blue", (0x00, 0x78, 0xD4)),
    ("indigo", (0x5B, 0x5F, 0xC7)),
    ("violet", (0x87, 0x64, 0xB8)),
    ("rose", (0xE3, 0x00, 0x8C)),
    ("red", (0xD1, 0x34, 0x38)),
    ("orange", (0xCA, 0x50, 0x10)),
    ("green", (0x0F, 0x7B, 0x0F)),
    ("teal", (0x03, 0x83, 0x87)),
];

#[inline]
fn shift_lightness(color: Hsla, delta: f32) -> Hsla {
    hsla(
        color.h,
        color.s,
        (color.l + delta).clamp(0.04, 0.96),
        color.a,
    )
}

/// Solves a `(primary_bg, primary_fg)` pair from `seed` over `surface` such that:
/// - `primary_bg` meets [`WCAG_AA_UI_CONTRAST`] (`>= 3.0:1`) against `surface`
/// - `primary_fg` (white `#FFFFFF` or near-black `#0A0A0A`) meets
///   [`WCAG_AA_TEXT_CONTRAST`] (`>= 4.5:1`) against `primary_bg`
///
/// Chooses the valid pair with the smallest lightness shift from `seed`, slightly
/// favoring crisp white labels on rich mid-to-dark accents.
fn resolve_primary_pair(seed: Hsla, surface: Hsla) -> (Hsla, Hsla) {
    let white = hsla(0.0, 0.0, 1.0, 1.0);
    let dark_ink = hsla(0.0, 0.0, 0.04, 1.0);
    let surface_rgb = hsla_to_srgb(surface);

    let evaluate = |fg: Hsla| -> (Hsla, Hsla, bool, f32) {
        let step1 = ensure_wcag_contrast(seed, surface, WCAG_AA_UI_CONTRAST);
        let bg = ensure_wcag_contrast(step1, fg, WCAG_AA_TEXT_CONTRAST);
        let bg_rgb = hsla_to_srgb(bg);
        let fg_rgb = hsla_to_srgb(fg);
        let ok = wcag_contrast_ratio(bg_rgb, surface_rgb) >= WCAG_AA_UI_CONTRAST - 1e-3
            && wcag_contrast_ratio(fg_rgb, bg_rgb) >= WCAG_AA_TEXT_CONTRAST - 1e-3;
        let dist = (bg.l - seed.l).abs();
        (bg, fg, ok, dist)
    };

    let (w_bg, w_fg, w_ok, w_dist) = evaluate(white);
    let (d_bg, d_fg, d_ok, d_dist) = evaluate(dark_ink);

    match (w_ok, d_ok) {
        (true, true) => {
            if w_dist <= d_dist + 0.06 {
                (w_bg, w_fg)
            } else {
                (d_bg, d_fg)
            }
        }
        (true, false) => (w_bg, w_fg),
        (false, true) => (d_bg, d_fg),
        (false, false) => {
            let w_min = wcag_contrast_ratio(hsla_to_srgb(w_bg), surface_rgb)
                .min(wcag_contrast_ratio(hsla_to_srgb(w_fg), hsla_to_srgb(w_bg)));
            let d_min = wcag_contrast_ratio(hsla_to_srgb(d_bg), surface_rgb)
                .min(wcag_contrast_ratio(hsla_to_srgb(d_fg), hsla_to_srgb(d_bg)));
            if w_min >= d_min {
                (w_bg, w_fg)
            } else {
                (d_bg, d_fg)
            }
        }
    }
}

/// Applies an sRGB accent color (`(r, g, b)`) across `theme`'s accent, primary,
/// focus, selection, sidebar, table, progress, and link tokens while enforcing
/// WCAG 2.1 AA contrast ratios in both light and dark modes.
pub fn apply_accent_to_theme(theme: &mut Theme, accent_rgb: (u8, u8, u8)) {
    let is_dark = theme.is_dark();
    let surface = theme.background;
    let sidebar_surface = theme.sidebar;
    let seed = rgb_to_hsla(accent_rgb);

    let (primary, primary_fg) = resolve_primary_pair(seed, surface);
    let primary_hover = if is_dark {
        shift_lightness(primary, 0.06)
    } else {
        shift_lightness(primary, -0.06)
    };
    let primary_active = if is_dark {
        shift_lightness(primary, 0.12)
    } else {
        shift_lightness(primary, -0.12)
    };

    let accent_ui = ensure_wcag_contrast(seed, surface, WCAG_AA_UI_CONTRAST);
    let (tint_subtle, tint_selection, tint_drop) = if is_dark {
        (0.22_f32, 0.30_f32, 0.24_f32)
    } else {
        (0.14_f32, 0.22_f32, 0.16_f32)
    };

    let list_active = accent_ui.opacity(tint_subtle);
    let list_active_composited = srgb_to_hsla(alpha_composite_srgb(
        hsla_to_srgb(list_active),
        list_active.a,
        hsla_to_srgb(surface),
    ));
    let accent_text_surface = ensure_wcag_contrast(seed, surface, WCAG_AA_TEXT_CONTRAST);
    let accent_text = ensure_wcag_contrast(
        accent_text_surface,
        list_active_composited,
        WCAG_AA_TEXT_CONTRAST,
    );

    let sidebar_accent = accent_ui.opacity(tint_subtle);
    let sidebar_active_composited = srgb_to_hsla(alpha_composite_srgb(
        hsla_to_srgb(sidebar_accent),
        sidebar_accent.a,
        hsla_to_srgb(sidebar_surface),
    ));
    let sidebar_accent_text_base =
        ensure_wcag_contrast(seed, sidebar_surface, WCAG_AA_TEXT_CONTRAST);
    let sidebar_accent_text = ensure_wcag_contrast(
        sidebar_accent_text_base,
        sidebar_active_composited,
        WCAG_AA_TEXT_CONTRAST,
    );

    let link_hover = if is_dark {
        shift_lightness(accent_text, 0.06)
    } else {
        shift_lightness(accent_text, -0.06)
    };
    let link_active = if is_dark {
        shift_lightness(accent_text, -0.06)
    } else {
        shift_lightness(accent_text, -0.10)
    };

    let colors = &mut theme.colors;
    colors.primary = primary;
    colors.primary_hover = primary_hover;
    colors.primary_active = primary_active;
    colors.primary_foreground = primary_fg;

    colors.button_primary = primary;
    colors.button_primary_hover = primary_hover;
    colors.button_primary_active = primary_active;
    colors.button_primary_foreground = primary_fg;

    colors.ring = accent_ui;
    colors.caret = accent_ui;
    colors.selection = accent_ui.opacity(tint_selection);
    colors.drag_border = accent_ui;
    colors.drop_target = accent_ui.opacity(tint_drop);

    colors.list_active = list_active;
    colors.list_active_border = accent_ui;
    colors.list_highlight_foreground = accent_text;

    colors.table_active = list_active;
    colors.table_active_border = accent_ui;

    colors.sidebar_primary = primary;
    colors.sidebar_primary_foreground = primary_fg;
    colors.sidebar_accent = sidebar_accent;
    colors.sidebar_accent_foreground = sidebar_accent_text;

    colors.progress_bar = primary;
    colors.slider_bar = primary;

    colors.link = accent_text;
    colors.link_hover = link_hover;
    colors.link_active = link_active;
}
