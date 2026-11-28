//! Thin composite UI widget layer built on top of `gpui-kit` for Loom.
//!
//! All widgets compose upstream `gpui-kit` (`gpui-component`) primitives and
//! Flexbox layout without custom `Canvas` or GPU shader rendering. No domain or
//! business state is held inside this crate.

#![warn(missing_docs)]

pub use gpui_kit;

pub mod format;
pub mod heatmap;
pub mod icon;
pub mod logic;
pub mod memory_bar;
pub mod row_cache;
pub mod sparkline;
pub mod status_badge;
pub mod theme;

pub use format::{
    CHILD_COUNT_TABLE_MAX, DEFAULT_CELL_PREVIEW_CHARS, HeatmapLevel, MetricDisplay,
    SMALL_U32_TABLE_MAX, TENTHS_TABLE_MAX, UNAVAILABLE_LABEL, adaptive_network_scale,
    adaptive_throughput_scale, cap_single_line_preview, display_preview_shared,
    format_bits_per_sec, format_bytes, format_bytes_metric, format_bytes_shared,
    format_child_count_shared, format_cpu_time, format_cpu_time_shared, format_disk_cell,
    format_disk_cell_metric, format_disk_cell_shared, format_freq_ghz, format_latency_us,
    format_memory_cell, format_memory_cell_metric, format_memory_cell_shared, format_network_cell,
    format_network_cell_metric, format_network_cell_shared, format_network_rate_mbps,
    format_network_rate_mbps_metric, format_network_rate_mbps_shared, format_opt_clock_mhz,
    format_opt_fan_percent, format_opt_percent_permille, format_opt_power_mw,
    format_opt_temp_celsius, format_permille, format_permille_metric, format_permille_shared,
    format_rate_bytes_per_sec, format_rate_bytes_per_sec_metric, format_rate_bytes_per_sec_shared,
    format_signed_bytes, format_signed_bytes_metric, format_tenths_shared, format_u32_shared,
    format_uptime, quantize_bytes_display, quantize_cpu_time_secs, quantize_net_bps_display,
    quantize_rate_bps_display, quantize_signed_bytes_display,
};
pub use heatmap::{heatmap_cell, heatmap_cell_metric, heatmap_cell_mono};
pub use icon::rgba_icon_to_render_image;
pub use logic::{
    CSV_UTF8_BOM, CoreCardHeaderMode, CoreGridLayout, GPU_ENGINE_ACTIVE_THRESHOLD_PERMILLE,
    build_csv_document, compute_core_grid_layout, format_csv_cell, merge_stable_order,
    select_active_gpu_engines, stepped_network_ceiling, stepped_throughput_ceiling,
};
pub use memory_bar::{MemoryBarLabels, MemoryBarSegments, memory_composition_bar};
pub use row_cache::{DEFAULT_ROW_TEXT_CACHE_CAPACITY, RowTextCache};
pub use sparkline::{
    GapAwareSparklinePoint, PERMILLE_GAP_SENTINEL, SparklineCardProps, SparklinePoint,
    sparkline_card,
};
pub use status_badge::{
    ECO_LEAF_ICON_PATH, ECO_LEAF_SVG, StatusLabels, has_visible_status_badges, status_badges,
    status_badges_with_a11y,
};
pub use theme::{
    ACCENT_COLOR_PRESETS, CachedSemanticColors, DEFAULT_ACCENT_RGB, LoomSemanticColors,
    WCAG_AA_TEXT_CONTRAST, WCAG_AA_UI_CONTRAST, WCAG_AAA_TEXT_CONTRAST, apply_accent_to_theme,
    ensure_wcag_contrast, hsla_to_rgb_u8, hsla_to_srgb, rgb_to_hsla, srgb_relative_luminance,
    wcag_contrast_ratio,
};
