//! Semantic color tokens derived from [`gpui_kit::component::Theme`] and
//! pure-Rust WCAG 2.1 contrast verification utilities.

mod accent;
mod palette;
#[cfg(test)]
mod tests;
mod wcag;

pub use accent::{ACCENT_COLOR_PRESETS, DEFAULT_ACCENT_RGB, apply_accent_to_theme};
pub use palette::{CachedSemanticColors, LoomSemanticColors};
pub use wcag::{
    WCAG_AA_TEXT_CONTRAST, WCAG_AA_UI_CONTRAST, WCAG_AAA_TEXT_CONTRAST, ensure_wcag_contrast,
    hsla_to_rgb_u8, hsla_to_srgb, rgb_to_hsla, srgb_relative_luminance, wcag_contrast_ratio,
};
