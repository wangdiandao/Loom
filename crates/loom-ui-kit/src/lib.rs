//! Thin composite UI widget layer built on top of `gpui-kit` for Loom.
//!
//! All widgets compose upstream `gpui-kit` (`gpui-component`) primitives and
//! Flexbox layout without custom `Canvas` or GPU shader rendering.

#![warn(missing_docs)]

pub use gpui_kit;

use loom_types::ResourceDisplayMode;

/// Semantic intensity bucket for process resource heatmap cells (`0..=1000` permille).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeatmapLevel {
    /// Negligible utilization (`< 5%`).
    None,
    /// Light utilization (`5%..25%`).
    Low,
    /// Moderate utilization (`25%..60%`).
    Medium,
    /// High utilization (`60%..85%`).
    High,
    /// Critical utilization (`>= 85%`).
    Critical,
}

impl HeatmapLevel {
    /// Maps a quantized permille value (`0..=1000`) to a [`HeatmapLevel`].
    #[inline]
    pub const fn from_permille(permille: u16) -> Self {
        match permille {
            0..=49 => Self::None,
            50..=249 => Self::Low,
            250..=599 => Self::Medium,
            600..=849 => Self::High,
            _ => Self::Critical,
        }
    }
}

/// Formats a memory value according to the active [`ResourceDisplayMode`].
pub fn format_memory_cell(bytes: u64, permille: u16, mode: ResourceDisplayMode) -> String {
    match mode {
        ResourceDisplayMode::Percent => format!("{:.1}%", f64::from(permille) / 10.0),
        ResourceDisplayMode::Values => {
            let mib = bytes as f64 / (1024.0 * 1024.0);
            format!("{mib:.1} MB")
        }
    }
}
