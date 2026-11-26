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
pub mod memory_bar;
pub mod sparkline;
pub mod status_badge;
pub mod theme;

pub use format::{
    HeatmapLevel, format_bytes, format_cpu_time, format_disk_cell, format_memory_cell,
    format_network_cell, format_network_rate_mbps, format_permille, format_rate_bytes_per_sec,
    format_uptime,
};
pub use heatmap::heatmap_cell;
pub use icon::rgba_icon_to_render_image;
pub use memory_bar::{MemoryBarLabels, MemoryBarSegments, memory_composition_bar};
pub use sparkline::{SparklineCardProps, SparklinePoint, sparkline_card};
pub use status_badge::{StatusLabels, status_badges};
pub use theme::LoomSemanticColors;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui_kit::component::{Theme, ThemeColor, ThemeMode};
    use gpui_kit::{DevicePixels, Size};
    use loom_types::{ExeIconRgba, ProcessStatusFlags, ResourceDisplayMode};

    use super::*;

    fn light_theme() -> Theme {
        let mut theme = Theme::from(ThemeColor::light().as_ref());
        theme.mode = ThemeMode::Light;
        theme
    }

    fn dark_theme() -> Theme {
        let mut theme = Theme::from(ThemeColor::dark().as_ref());
        theme.mode = ThemeMode::Dark;
        theme
    }

    #[test]
    fn heatmap_level_from_permille_boundaries() {
        assert_eq!(HeatmapLevel::from_permille(0), HeatmapLevel::None);
        assert_eq!(HeatmapLevel::from_permille(49), HeatmapLevel::None);
        assert_eq!(HeatmapLevel::from_permille(50), HeatmapLevel::Low);
        assert_eq!(HeatmapLevel::from_permille(249), HeatmapLevel::Low);
        assert_eq!(HeatmapLevel::from_permille(250), HeatmapLevel::Medium);
        assert_eq!(HeatmapLevel::from_permille(599), HeatmapLevel::Medium);
        assert_eq!(HeatmapLevel::from_permille(600), HeatmapLevel::High);
        assert_eq!(HeatmapLevel::from_permille(849), HeatmapLevel::High);
        assert_eq!(HeatmapLevel::from_permille(850), HeatmapLevel::Critical);
        assert_eq!(HeatmapLevel::from_permille(1000), HeatmapLevel::Critical);
        assert_eq!(
            HeatmapLevel::from_permille(u16::MAX),
            HeatmapLevel::Critical
        );
    }

    #[test]
    fn heatmap_level_from_bytes_per_sec_boundaries() {
        const KIB: u64 = 1024;
        const MIB: u64 = 1024 * KIB;

        assert_eq!(HeatmapLevel::from_bytes_per_sec(0), HeatmapLevel::None);
        assert_eq!(
            HeatmapLevel::from_bytes_per_sec(100 * KIB - 1),
            HeatmapLevel::None
        );
        assert_eq!(
            HeatmapLevel::from_bytes_per_sec(100 * KIB),
            HeatmapLevel::Low
        );
        assert_eq!(
            HeatmapLevel::from_bytes_per_sec(2 * MIB - 1),
            HeatmapLevel::Low
        );
        assert_eq!(
            HeatmapLevel::from_bytes_per_sec(2 * MIB),
            HeatmapLevel::Medium
        );
        assert_eq!(
            HeatmapLevel::from_bytes_per_sec(20 * MIB - 1),
            HeatmapLevel::Medium
        );
        assert_eq!(
            HeatmapLevel::from_bytes_per_sec(20 * MIB),
            HeatmapLevel::High
        );
        assert_eq!(
            HeatmapLevel::from_bytes_per_sec(100 * MIB - 1),
            HeatmapLevel::High
        );
        assert_eq!(
            HeatmapLevel::from_bytes_per_sec(100 * MIB),
            HeatmapLevel::Critical
        );
    }

    #[test]
    fn format_permille_values() {
        assert_eq!(format_permille(0), "0.0%");
        assert_eq!(format_permille(5), "0.5%");
        assert_eq!(format_permille(125), "12.5%");
        assert_eq!(format_permille(1000), "100.0%");
    }

    #[test]
    fn format_bytes_units_and_precision() {
        const KIB: u64 = 1024;
        const MIB: u64 = 1024 * KIB;
        const GIB: u64 = 1024 * MIB;
        const TIB: u64 = 1024 * GIB;

        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(KIB - 1), "1023 B");
        assert_eq!(format_bytes(KIB), "1 KB");
        assert_eq!(format_bytes(512 * KIB), "512 KB");
        assert_eq!(format_bytes(MIB), "1.0 MB");
        assert_eq!(format_bytes(1536 * KIB), "1.5 MB");
        assert_eq!(format_bytes(GIB), "1.0 GB");
        assert_eq!(format_bytes(16 * GIB + 512 * MIB), "16.5 GB");
        assert_eq!(format_bytes(TIB), "1.0 TB");
        assert_eq!(format_bytes(2 * TIB + 512 * GIB), "2.5 TB");
    }

    #[test]
    fn format_rates_and_resource_cells() {
        const KIB: u64 = 1024;
        const MIB: u64 = 1024 * KIB;
        const GIB: u64 = 1024 * MIB;

        assert_eq!(format_rate_bytes_per_sec(0), "0 MB/s");
        assert_eq!(format_rate_bytes_per_sec(512), "512 B/s");
        assert_eq!(format_rate_bytes_per_sec(512 * KIB), "512.0 KB/s");
        assert_eq!(format_rate_bytes_per_sec(1_468_006), "1.4 MB/s");
        assert_eq!(format_rate_bytes_per_sec(2 * GIB), "2.0 GB/s");

        assert_eq!(format_network_rate_mbps(0), "0 Mbps");
        assert_eq!(format_network_rate_mbps(100), "800 bps");
        assert_eq!(format_network_rate_mbps(1_250), "10.0 Kbps");
        assert_eq!(format_network_rate_mbps(1_550_000), "12.4 Mbps");
        assert_eq!(format_network_rate_mbps(125_000_000), "1.0 Gbps");

        assert_eq!(
            format_memory_cell(128 * MIB, 42, ResourceDisplayMode::Values),
            "128.0 MB"
        );
        assert_eq!(
            format_memory_cell(128 * MIB, 42, ResourceDisplayMode::Percent),
            "4.2%"
        );

        assert_eq!(
            format_disk_cell(0, 0, ResourceDisplayMode::Values),
            "0 MB/s"
        );
        assert_eq!(
            format_disk_cell(MIB, 155, ResourceDisplayMode::Percent),
            "15.5%"
        );

        assert_eq!(
            format_network_cell(1_550_000, 124, ResourceDisplayMode::Values),
            "12.4 Mbps"
        );
        assert_eq!(
            format_network_cell(1_550_000, 124, ResourceDisplayMode::Percent),
            "12.4%"
        );
    }

    #[test]
    fn format_uptime_and_cpu_time_strings() {
        assert_eq!(format_uptime(0), "0:00:00:00");
        assert_eq!(format_uptime(59), "0:00:00:59");
        assert_eq!(format_uptime(3_661), "0:01:01:01");
        assert_eq!(format_uptime(86_400 + 3_661), "1:01:01:01");
        assert_eq!(
            format_uptime(12 * 86_400 + 23 * 3_600 + 59 * 60 + 59),
            "12:23:59:59"
        );

        assert_eq!(format_cpu_time(0), "00:00:00");
        assert_eq!(format_cpu_time(9_999_999), "00:00:00");
        assert_eq!(format_cpu_time(10_000_000), "00:00:01");
        assert_eq!(format_cpu_time(36_610_000_000), "01:01:01");
        assert_eq!(format_cpu_time(3_600_000_000_000), "100:00:00");
    }

    #[test]
    fn semantic_colors_adapt_to_light_and_dark_themes() {
        let light = LoomSemanticColors::from_theme(&light_theme());
        let dark = LoomSemanticColors::from_theme(&dark_theme());

        assert!(!light.is_dark);
        assert!(dark.is_dark);

        for palette in [&light, &dark] {
            assert_eq!(palette.heatmap_bg(HeatmapLevel::None).a, 0.0);
            let low = palette.heatmap_bg(HeatmapLevel::Low).a;
            let med = palette.heatmap_bg(HeatmapLevel::Medium).a;
            let high = palette.heatmap_bg(HeatmapLevel::High).a;
            let crit = palette.heatmap_bg(HeatmapLevel::Critical).a;
            assert!(0.0 < low && low < med && med < high && high < crit);
        }

        // Light and dark semantic tokens should tune luminance distinctly.
        assert_ne!(light.status_eco_fg, dark.status_eco_fg);
        assert_ne!(light.status_hung_fg, dark.status_hung_fg);
        assert_ne!(light.chart_cpu, dark.chart_cpu);
        assert_ne!(light.chart_cpu, light.chart_cpu_kernel);
    }

    #[test]
    fn memory_bar_segment_proportions() {
        let empty = MemoryBarSegments::default();
        assert_eq!(empty.total_bytes(), 0);
        assert_eq!(empty.normalized_weights(), [0.0, 0.0, 0.0, 1.0]);

        let segments = MemoryBarSegments {
            in_use_bytes: 50,
            modified_bytes: 10,
            standby_bytes: 25,
            free_bytes: 15,
        };
        assert_eq!(segments.total_bytes(), 100);
        let weights = segments.normalized_weights();
        assert!((weights[0] - 0.50).abs() < 1e-6);
        assert!((weights[1] - 0.10).abs() < 1e-6);
        assert!((weights[2] - 0.25).abs() < 1e-6);
        assert!((weights[3] - 0.15).abs() < 1e-6);
        let sum: f32 = weights.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6);

        // Saturating addition guards against u64 overflow.
        let huge = MemoryBarSegments {
            in_use_bytes: u64::MAX,
            modified_bytes: 100,
            standby_bytes: 0,
            free_bytes: 0,
        };
        assert_eq!(huge.total_bytes(), u64::MAX);
    }

    #[test]
    fn sparkline_points_helpers() {
        let from_f32 = SparklinePoint::from_slice(&[10.0, 25.5, 80.0]);
        assert_eq!(
            from_f32,
            vec![
                SparklinePoint::new(0, 10.0),
                SparklinePoint::new(1, 25.5),
                SparklinePoint::new(2, 80.0),
            ]
        );

        let from_permille = SparklinePoint::from_permille_iter([0, 125, 1000]);
        assert_eq!(
            from_permille,
            vec![
                SparklinePoint::new(0, 0.0),
                SparklinePoint::new(1, 12.5),
                SparklinePoint::new(2, 100.0),
            ]
        );
    }

    #[test]
    fn rgba_icon_to_render_image_validates_and_swaps_rb_channels() {
        // 2x1 top-down RGBA8 image:
        // pixel 0: R=0x11, G=0x22, B=0x33, A=0xFF -> BGRA: [0x33, 0x22, 0x11, 0xFF]
        // pixel 1: R=0xAA, G=0xBB, B=0xCC, A=0x80 -> BGRA: [0xCC, 0xBB, 0xAA, 0x80]
        let icon = ExeIconRgba {
            width: 2,
            height: 1,
            rgba: Arc::from([0x11, 0x22, 0x33, 0xFF, 0xAA, 0xBB, 0xCC, 0x80]),
        };
        let rendered = rgba_icon_to_render_image(&icon).expect("valid 2x1 RGBA icon");
        assert_eq!(rendered.frame_count(), 1);
        assert_eq!(
            rendered.size(0),
            Size {
                width: DevicePixels(2),
                height: DevicePixels(1),
            }
        );
        assert_eq!(
            rendered.as_bytes(0),
            Some(&[0x33, 0x22, 0x11, 0xFF, 0xCC, 0xBB, 0xAA, 0x80][..])
        );

        // Zero dimensions are rejected.
        assert!(
            rgba_icon_to_render_image(&ExeIconRgba {
                width: 0,
                height: 16,
                rgba: Arc::from([]),
            })
            .is_none()
        );
        assert!(
            rgba_icon_to_render_image(&ExeIconRgba {
                width: 16,
                height: 0,
                rgba: Arc::from([]),
            })
            .is_none()
        );

        // Length mismatch (too short or too long) is rejected.
        assert!(
            rgba_icon_to_render_image(&ExeIconRgba {
                width: 2,
                height: 2,
                rgba: Arc::from([0u8; 12]),
            })
            .is_none()
        );
        assert!(
            rgba_icon_to_render_image(&ExeIconRgba {
                width: 1,
                height: 1,
                rgba: Arc::from([0u8; 8]),
            })
            .is_none()
        );
    }

    #[test]
    fn widgets_construct_cleanly() {
        let colors = LoomSemanticColors::from_theme(&dark_theme());
        let _cell = heatmap_cell("42.5%", HeatmapLevel::Medium, &colors);

        let status_labels = StatusLabels::new("Efficiency mode", "Suspended", "Not responding");
        let _badges = status_badges(
            42,
            ProcessStatusFlags::EFFICIENCY_MODE
                | ProcessStatusFlags::SUSPENDED
                | ProcessStatusFlags::NOT_RESPONDING,
            &status_labels,
            &colors,
        );

        let _card = sparkline_card(
            SparklineCardProps {
                id: "cpu".into(),
                title: "CPU".into(),
                subtitle: "3.82 GHz".into(),
                primary_metric: "14%".into(),
                points: SparklinePoint::from_slice(&[5.0, 12.0, 14.0]),
                y_max: 100.0,
                accent_color: colors.chart_cpu,
                selected: true,
            },
            &colors,
        );

        let mem_labels = MemoryBarLabels::new("In use", "Modified", "Standby", "Free");
        let _bar = memory_composition_bar(
            "mem-bar",
            MemoryBarSegments {
                in_use_bytes: 8 * 1024 * 1024 * 1024,
                modified_bytes: 512 * 1024 * 1024,
                standby_bytes: 4 * 1024 * 1024 * 1024,
                free_bytes: 3 * 1024 * 1024 * 1024,
            },
            &mem_labels,
            &colors,
        );
    }
}
