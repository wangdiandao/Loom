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
pub mod sparkline;
pub mod status_badge;
pub mod theme;

pub use format::{
    HeatmapLevel, UNAVAILABLE_LABEL, adaptive_network_scale, adaptive_throughput_scale,
    format_bits_per_sec, format_bytes, format_bytes_shared, format_cpu_time,
    format_cpu_time_shared, format_disk_cell, format_disk_cell_shared, format_freq_ghz,
    format_latency_us, format_memory_cell, format_memory_cell_shared, format_network_cell,
    format_network_cell_shared, format_network_rate_mbps, format_network_rate_mbps_shared,
    format_opt_percent_permille, format_opt_temp_celsius, format_permille, format_permille_shared,
    format_rate_bytes_per_sec, format_rate_bytes_per_sec_shared, format_uptime,
};
pub use heatmap::heatmap_cell;
pub use icon::rgba_icon_to_render_image;
pub use logic::{
    CSV_UTF8_BOM, CoreCardHeaderMode, CoreGridLayout, GPU_ENGINE_ACTIVE_THRESHOLD_PERMILLE,
    build_csv_document, compute_core_grid_layout, format_csv_cell, merge_stable_order,
    select_active_gpu_engines, stepped_network_ceiling, stepped_throughput_ceiling,
};
pub use memory_bar::{MemoryBarLabels, MemoryBarSegments, memory_composition_bar};
pub use sparkline::{
    GapAwareSparklinePoint, PERMILLE_GAP_SENTINEL, SparklineCardProps, SparklinePoint,
    sparkline_card,
};
pub use status_badge::{ECO_LEAF_ICON_PATH, ECO_LEAF_SVG, StatusLabels, status_badges};
pub use theme::{
    LoomSemanticColors, WCAG_AA_TEXT_CONTRAST, WCAG_AA_UI_CONTRAST, WCAG_AAA_TEXT_CONTRAST,
    ensure_wcag_contrast, hsla_to_srgb, srgb_relative_luminance, wcag_contrast_ratio,
};

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

        assert_eq!(format_permille_shared(0).as_ref(), "0.0%");
        assert_eq!(format_permille_shared(125).as_ref(), "12.5%");
        assert_eq!(format_permille_shared(1000).as_ref(), "100.0%");
        assert_eq!(format_bytes_shared(0).as_ref(), "0 B");
        assert_eq!(format_rate_bytes_per_sec_shared(0).as_ref(), "0 MB/s");
        assert_eq!(format_network_rate_mbps_shared(0).as_ref(), "0 Mbps");
        assert_eq!(format_cpu_time_shared(0).as_ref(), "00:00:00");
        assert_eq!(format_cpu_time_shared(10_000_000).as_ref(), "00:00:01");
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

        let padded = SparklinePoint::from_permille_padded([250, 500], 4);
        assert_eq!(
            padded,
            vec![
                SparklinePoint::new(0, 0.0),
                SparklinePoint::new(1, 0.0),
                SparklinePoint::new(2, 25.0),
                SparklinePoint::new(3, 50.0),
            ]
        );

        let truncated = SparklinePoint::from_f32_padded([1.0, 2.0, 3.0, 4.0], 2);
        assert_eq!(
            truncated,
            vec![SparklinePoint::new(0, 3.0), SparklinePoint::new(1, 4.0)]
        );
    }

    #[test]
    fn format_performance_metrics_and_adaptive_scales() {
        const KIB: u64 = 1024;
        const MIB: u64 = 1024 * KIB;
        const GIB: u64 = 1024 * MIB;

        assert_eq!(format_freq_ghz(0), "0.00 GHz");
        assert_eq!(format_freq_ghz(800), "0.80 GHz");
        assert_eq!(format_freq_ghz(3_820), "3.82 GHz");

        assert_eq!(format_bits_per_sec(0), "0 bps");
        assert_eq!(format_bits_per_sec(500), "500 bps");
        assert_eq!(format_bits_per_sec(100_000), "100.0 Kbps");
        assert_eq!(format_bits_per_sec(100_000_000), "100.0 Mbps");
        assert_eq!(format_bits_per_sec(2_500_000_000), "2.5 Gbps");

        assert_eq!(format_latency_us(0), "0.0 ms");
        assert_eq!(format_latency_us(250), "250 µs");
        assert_eq!(format_latency_us(1_000), "1.0 ms");
        assert_eq!(format_latency_us(12_450), "12.4 ms");
        assert_eq!(format_latency_us(1_500_000), "1.50 s");

        let (y_idle, lbl_idle) = adaptive_throughput_scale(0);
        assert_eq!(y_idle, (100 * KIB) as f32);
        assert_eq!(lbl_idle.as_ref(), "100 KB/s");

        let (y_mb, lbl_mb) = adaptive_throughput_scale(15 * MIB);
        assert_eq!(y_mb, (50 * MIB) as f32);
        assert_eq!(lbl_mb.as_ref(), "50 MB/s");

        let (y_gb, lbl_gb) = adaptive_throughput_scale(3 * GIB);
        assert_eq!(y_gb, (5 * GIB) as f32);
        assert_eq!(lbl_gb.as_ref(), "5 GB/s");

        let (y_huge, lbl_huge) = adaptive_throughput_scale(12 * GIB + 1);
        assert_eq!(y_huge, (13 * GIB) as f32);
        assert_eq!(lbl_huge.as_ref(), "13 GB/s");

        let (ny_idle, nlbl_idle) = adaptive_network_scale(0);
        assert_eq!(ny_idle, 100_000.0);
        assert_eq!(nlbl_idle.as_ref(), "100 Kbps");

        let (ny_mbps, nlbl_mbps) = adaptive_network_scale(5_000_000); // 40 Mbps
        assert_eq!(ny_mbps, 50_000_000.0);
        assert_eq!(nlbl_mbps.as_ref(), "50 Mbps");

        let (ny_gbps, nlbl_gbps) = adaptive_network_scale(200_000_000); // 1.6 Gbps
        assert_eq!(ny_gbps, 2_000_000_000.0);
        assert_eq!(nlbl_gbps.as_ref(), "2 Gbps");
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

    #[test]
    fn stepped_throughput_ceiling_covers_all_clean_steps_and_high_rates() {
        const KIB: u64 = 1024;
        const MIB: u64 = 1024 * KIB;
        const GIB: u64 = 1024 * MIB;

        assert_eq!(stepped_throughput_ceiling(0), (100.0, "KB/s", KIB as f64));
        assert_eq!(
            stepped_throughput_ceiling(100 * KIB),
            (100.0, "KB/s", KIB as f64)
        );
        assert_eq!(
            stepped_throughput_ceiling(100 * KIB + 1),
            (200.0, "KB/s", KIB as f64)
        );
        assert_eq!(
            stepped_throughput_ceiling(350 * KIB),
            (500.0, "KB/s", KIB as f64)
        );
        assert_eq!(
            stepped_throughput_ceiling(500 * KIB + 1),
            (1.0, "MB/s", MIB as f64)
        );
        assert_eq!(
            stepped_throughput_ceiling(15 * MIB),
            (20.0, "MB/s", MIB as f64)
        );
        assert_eq!(
            stepped_throughput_ceiling(150 * MIB),
            (200.0, "MB/s", MIB as f64)
        );
        assert_eq!(
            stepped_throughput_ceiling(600 * MIB),
            (1.0, "GB/s", GIB as f64)
        );
        assert_eq!(
            stepped_throughput_ceiling(3 * GIB),
            (5.0, "GB/s", GIB as f64)
        );
        assert_eq!(
            stepped_throughput_ceiling(50 * GIB),
            (50.0, "GB/s", GIB as f64)
        );

        // Above 50 GB/s extends cleanly along 1-2-5 steps without overflowing.
        let (y_max, unit, div) = stepped_throughput_ceiling(50 * GIB + 1);
        assert_eq!((y_max, unit, div), (100.0, "GB/s", GIB as f64));
        assert!(y_max * div >= (50 * GIB + 1) as f64);

        let (y_max_hi, unit_hi, div_hi) = stepped_throughput_ceiling(120 * GIB);
        assert_eq!((y_max_hi, unit_hi, div_hi), (200.0, "GB/s", GIB as f64));
    }

    #[test]
    fn stepped_network_ceiling_snaps_bytes_to_clean_bit_steps() {
        // 0 bytes/s -> minimum 100 Kbps floor.
        assert_eq!(stepped_network_ceiling(0), (100.0, "Kbps", 125.0));
        // 12_500 B/s == 100 Kbps exact boundary.
        assert_eq!(stepped_network_ceiling(12_500), (100.0, "Kbps", 125.0));
        // 12_501 B/s > 100 Kbps -> snaps to 250 Kbps.
        assert_eq!(stepped_network_ceiling(12_501), (250.0, "Kbps", 125.0));
        // 40_000 B/s == 320 Kbps -> 500 Kbps.
        assert_eq!(stepped_network_ceiling(40_000), (500.0, "Kbps", 125.0));
        // 125_000 B/s == 1 Mbps.
        assert_eq!(stepped_network_ceiling(125_000), (1.0, "Mbps", 125_000.0));
        // 200_000 B/s == 1.6 Mbps -> 2 Mbps.
        assert_eq!(stepped_network_ceiling(200_000), (2.0, "Mbps", 125_000.0));
        // 2_000_000 B/s == 16 Mbps -> 25 Mbps.
        assert_eq!(
            stepped_network_ceiling(2_000_000),
            (25.0, "Mbps", 125_000.0)
        );
        // 200_000_000 B/s == 1.6 Gbps -> 2.5 Gbps.
        assert_eq!(
            stepped_network_ceiling(200_000_000),
            (2.5, "Gbps", 125_000_000.0)
        );
        // 4_000_000_000 B/s == 32 Gbps -> 40 Gbps.
        assert_eq!(
            stepped_network_ceiling(4_000_000_000),
            (40.0, "Gbps", 125_000_000.0)
        );
        // 12_500_000_000 B/s == 100 Gbps exact.
        assert_eq!(
            stepped_network_ceiling(12_500_000_000),
            (100.0, "Gbps", 125_000_000.0)
        );
        // > 100 Gbps extends cleanly and never overflows even near u64::MAX.
        let (y_hi, unit_hi, div_hi) = stepped_network_ceiling(15_000_000_000);
        assert_eq!((y_hi, unit_hi, div_hi), (200.0, "Gbps", 125_000_000.0));
        assert!(15_000_000_000.0 / div_hi <= y_hi);

        let (y_max, unit_max, div_max) = stepped_network_ceiling(u64::MAX);
        assert_eq!(unit_max, "Gbps");
        assert!(y_max.is_finite() && y_max * div_max >= u64::MAX as f64);
    }

    #[test]
    fn core_grid_layout_scoring_hysteresis_and_header_degradation() {
        // Zero cores edge case.
        let empty = compute_core_grid_layout(0, 800.0, 500.0, None);
        assert_eq!(empty.columns, 0);
        assert_eq!(empty.rows, 0);

        // Clean rectangular fits on standard desktop viewports.
        let l16 = compute_core_grid_layout(16, 1000.0, 600.0, None);
        assert_eq!(l16.columns, 4);
        assert_eq!(l16.rows, 4);
        assert_eq!(l16.header_mode, CoreCardHeaderMode::Full);

        let l24 = compute_core_grid_layout(24, 1000.0, 600.0, None);
        assert_eq!(l24.columns, 6);
        assert_eq!(l24.rows, 4);
        assert_eq!(l24.header_mode, CoreCardHeaderMode::Full);

        // High-core-count capped at <= 16 columns.
        let l128 = compute_core_grid_layout(128, 1200.0, 500.0, None);
        assert!(l128.columns <= 16 && l128.columns >= 8);
        assert_eq!(l128.rows, 128usize.div_ceil(l128.columns));

        // Progressive header degradation Full -> Short -> Hidden as card height shrinks.
        let tall = compute_core_grid_layout(64, 1600.0, 460.0, None);
        let medium = compute_core_grid_layout(64, 1600.0, 260.0, Some(tall.columns));
        let tight = compute_core_grid_layout(64, 1600.0, 180.0, Some(medium.columns));
        assert_eq!(tall.columns, 16);
        assert_eq!(tall.header_mode, CoreCardHeaderMode::Full);
        assert_eq!(medium.header_mode, CoreCardHeaderMode::Short);
        assert_eq!(tight.header_mode, CoreCardHeaderMode::Hidden);
        assert!(tall.card_height_px > medium.card_height_px);
        assert!(medium.card_height_px > tight.card_height_px);

        // 8% hysteresis dead-band prevents boundary oscillation during edge drags.
        // Locate a width transition for 8 cores at height 500 without hysteresis.
        let mut transition_w = None;
        let mut prev_cold = compute_core_grid_layout(8, 300.0, 500.0, None).columns;
        for w in (302..=1200).step_by(2) {
            let cur = compute_core_grid_layout(8, w as f32, 500.0, None).columns;
            if cur != prev_cold {
                transition_w = Some((w as f32, prev_cold, cur));
                break;
            }
            prev_cold = cur;
        }
        let (cross_w, left_cols, right_cols) =
            transition_w.expect("8 cores should have a column crossover between 300px and 1200px");

        // Just past the cold crossover, passing `prev_columns = Some(left_cols)` retains `left_cols`.
        let held_right = compute_core_grid_layout(8, cross_w, 500.0, Some(left_cols));
        assert_eq!(held_right.columns, left_cols);

        // Just before the cold crossover, passing `prev_columns = Some(right_cols)` retains `right_cols`.
        let held_left = compute_core_grid_layout(8, cross_w - 2.0, 500.0, Some(right_cols));
        assert_eq!(held_left.columns, right_cols);
    }

    #[test]
    fn select_active_gpu_engines_filters_sorts_and_falls_back() {
        assert!(select_active_gpu_engines(&[], 4).is_empty());
        assert!(select_active_gpu_engines(&[(0, 500)], 0).is_empty());

        // Active engines (>= 10 permille) ordered by peak desc, then index asc.
        let active =
            select_active_gpu_engines(&[(0, 5), (1, 120), (2, 10), (3, 120), (4, 850), (5, 9)], 3);
        assert_eq!(active, vec![4, 1, 3]);

        // Includes exact threshold boundary (10 permille = 1.0%).
        let boundary = select_active_gpu_engines(&[(0, 9), (2, 10)], 4);
        assert_eq!(boundary, vec![2]);

        // All idle with zero peaks falls back to lowest index (0).
        let all_zero = select_active_gpu_engines(&[(2, 0), (0, 0), (1, 0)], 4);
        assert_eq!(all_zero, vec![0]);

        // All below threshold (< 10) keeps single highest-peak engine.
        let sub_threshold = select_active_gpu_engines(&[(0, 2), (1, 8), (2, 8)], 4);
        assert_eq!(sub_threshold, vec![1]);
    }

    #[test]
    fn csv_helpers_prevent_formula_injection_and_follow_rfc4180() {
        // Plain values untouched.
        assert_eq!(format_csv_cell("loom.exe"), "loom.exe");
        assert_eq!(format_csv_cell("1234"), "1234");
        assert_eq!(format_csv_cell("a=b"), "a=b");

        // Direct formula triggers prefixed with `'`.
        assert_eq!(format_csv_cell("=1+1"), "'=1+1");
        assert_eq!(format_csv_cell("+cmd|' /C calc'!A0"), "'+cmd|' /C calc'!A0");
        assert_eq!(format_csv_cell("-2+3"), "'-2+3");
        assert_eq!(format_csv_cell("@SUM(A1:A2)"), "'@SUM(A1:A2)");
        assert_eq!(format_csv_cell("\t=1+1"), "'\t=1+1");
        assert_eq!(format_csv_cell("\rfoo"), "\"'\rfoo\"");

        // Leading whitespace followed by formula trigger is also neutralized.
        assert_eq!(
            format_csv_cell("   =HYPERLINK(\"http://x\",\"y\")"),
            "\"'   =HYPERLINK(\"\"http://x\"\",\"\"y\"\")\""
        );
        assert_eq!(format_csv_cell("  +1"), "'  +1");
        assert_eq!(format_csv_cell(" \n -bad"), "\"' \n -bad\"");

        // RFC 4180 quoting & quote doubling.
        assert_eq!(format_csv_cell("a, b"), "\"a, b\"");
        assert_eq!(format_csv_cell("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(format_csv_cell("line1\nline2"), "\"line1\nline2\"");

        // Complete document includes UTF-8 BOM + CRLF lines.
        let doc = build_csv_document(
            &["Name", "PID", "Cmd"],
            &[
                vec!["app.exe".into(), "42".into(), "=cmd, \"arg\"".into()],
                vec!["safe.exe".into(), "99".into(), "normal".into()],
            ],
        );
        assert!(doc.starts_with(CSV_UTF8_BOM));
        assert_eq!(
            doc,
            "\u{FEFF}Name,PID,Cmd\r\napp.exe,42,\"'=cmd, \"\"arg\"\"\"\r\nsafe.exe,99,normal\r\n"
        );
    }

    #[test]
    fn merge_stable_order_preserves_open_order_appends_new_and_omits_removed() {
        let mut pinned: Vec<&str> = Vec::new();

        // Open time: establishes [A, B, C].
        let first = merge_stable_order(&mut pinned, &["A", "B", "C"]);
        assert_eq!(first, vec![0, 1, 2]);
        assert_eq!(pinned, vec!["A", "B", "C"]);

        // OS reorders [C, A], removes B -> returns indices for A then C.
        let second = merge_stable_order(&mut pinned, &["C", "A"]);
        assert_eq!(second, vec![1, 0]);

        // New device D arrives at front of current slice -> appended after A, C.
        let third = merge_stable_order(&mut pinned, &["D", "C", "A"]);
        assert_eq!(third, vec![2, 1, 0]);
        assert_eq!(pinned, vec!["A", "B", "C", "D"]);

        // B reconnects -> returns to its original slot between A and C.
        let fourth = merge_stable_order(&mut pinned, &["B", "D", "C", "A"]);
        assert_eq!(fourth, vec![3, 0, 2, 1]);
    }

    #[test]
    fn wcag_contrast_math_and_solver_targets() {
        // Exact WCAG 2.1 extremes.
        assert!((srgb_relative_luminance(0.0, 0.0, 0.0) - 0.0).abs() < 1e-6);
        assert!((srgb_relative_luminance(1.0, 1.0, 1.0) - 1.0).abs() < 1e-6);
        assert!((wcag_contrast_ratio((0.0, 0.0, 0.0), (1.0, 1.0, 1.0)) - 21.0).abs() < 1e-4);
        assert!((wcag_contrast_ratio((0.5, 0.5, 0.5), (0.5, 0.5, 0.5)) - 1.0).abs() < 1e-6);

        let dark_bg = gpui_kit::hsla(220.0 / 360.0, 0.14, 0.10, 1.0);
        let light_bg = gpui_kit::hsla(0.0, 0.0, 0.98, 1.0);
        let low_contrast_blue = gpui_kit::hsla(210.0 / 360.0, 0.70, 0.45, 1.0);
        let low_contrast_yellow = gpui_kit::hsla(48.0 / 360.0, 0.90, 0.72, 1.0);

        for bg in [dark_bg, light_bg] {
            for fg in [low_contrast_blue, low_contrast_yellow] {
                let aa = ensure_wcag_contrast(fg, bg, WCAG_AA_TEXT_CONTRAST);
                assert!(
                    wcag_contrast_ratio(hsla_to_srgb(aa), hsla_to_srgb(bg))
                        >= WCAG_AA_TEXT_CONTRAST - 1e-3,
                    "AA (4.5:1) failed for fg={fg:?} on bg={bg:?} -> {aa:?}"
                );
                assert!((aa.h - fg.h).abs() < 1e-6);
                assert!((aa.s - fg.s).abs() < 1e-6);

                let aaa = ensure_wcag_contrast(fg, bg, WCAG_AAA_TEXT_CONTRAST);
                assert!(
                    wcag_contrast_ratio(hsla_to_srgb(aaa), hsla_to_srgb(bg))
                        >= WCAG_AAA_TEXT_CONTRAST - 1e-3,
                    "AAA (7.0:1) failed for fg={fg:?} on bg={bg:?} -> {aaa:?}"
                );
                assert!((aaa.h - fg.h).abs() < 1e-6);
                assert!((aaa.s - fg.s).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn semantic_palette_meets_wcag_contrast_in_light_and_dark_themes() {
        for palette in [
            LoomSemanticColors::from_theme(&light_theme()),
            LoomSemanticColors::from_theme(&dark_theme()),
        ] {
            let bg = hsla_to_srgb(palette.card_bg);

            // Text & status foreground tokens must meet WCAG AA text (>= 4.5:1).
            let text_tokens = [
                ("text_primary", palette.text_primary),
                ("text_muted", palette.text_muted),
                ("status_eco_fg", palette.status_eco_fg),
                ("status_suspended_fg", palette.status_suspended_fg),
                ("status_hung_fg", palette.status_hung_fg),
                ("leak_warning", palette.leak_warning),
                ("sort_paused", palette.sort_paused),
            ];
            for (name, color) in text_tokens {
                let ratio = wcag_contrast_ratio(hsla_to_srgb(color), bg);
                assert!(
                    ratio >= WCAG_AA_TEXT_CONTRAST - 1e-3,
                    "{name} contrast {ratio:.2}:1 < 4.5:1 (is_dark={})",
                    palette.is_dark
                );
            }

            // Chart & heterogeneous core accents must meet WCAG AA UI (>= 3.0:1).
            let ui_tokens = [
                ("chart_cpu", palette.chart_cpu),
                ("chart_cpu_kernel", palette.chart_cpu_kernel),
                ("cpu_core_p", palette.cpu_core_p),
                ("cpu_core_e", palette.cpu_core_e),
                ("cpu_core_lp_e", palette.cpu_core_lp_e),
                ("chart_memory", palette.chart_memory),
                ("chart_disk", palette.chart_disk),
                ("chart_disk_write", palette.chart_disk_write),
                ("chart_network", palette.chart_network),
                ("chart_network_send", palette.chart_network_send),
                ("chart_gpu", palette.chart_gpu),
                ("chart_gpu_shared", palette.chart_gpu_shared),
            ];
            for (name, color) in ui_tokens {
                let ratio = wcag_contrast_ratio(hsla_to_srgb(color), bg);
                assert!(
                    ratio >= WCAG_AA_UI_CONTRAST - 1e-3,
                    "{name} contrast {ratio:.2}:1 < 3.0:1 (is_dark={})",
                    palette.is_dark
                );
            }

            // Heterogeneous core semantic colors must be mutually distinct.
            assert_ne!(palette.cpu_core_p, palette.cpu_core_e);
            assert_ne!(palette.cpu_core_e, palette.cpu_core_lp_e);
            assert_ne!(palette.cpu_core_p, palette.cpu_core_lp_e);
        }
    }

    #[test]
    fn honest_optional_formatters_and_gap_aware_sparkline_helpers() {
        assert_eq!(UNAVAILABLE_LABEL, "—");
        assert_eq!(format_opt_percent_permille(None).as_ref(), "—");
        assert_eq!(format_opt_percent_permille(Some(0)).as_ref(), "0.0%");
        assert_eq!(format_opt_percent_permille(Some(425)).as_ref(), "42.5%");
        assert_eq!(format_opt_percent_permille(Some(1000)).as_ref(), "100.0%");

        assert_eq!(format_opt_temp_celsius(None, false).as_ref(), "—");
        assert_eq!(format_opt_temp_celsius(Some(58), true).as_ref(), "—");
        assert_eq!(format_opt_temp_celsius(Some(58), false).as_ref(), "58°C");
        assert_eq!(format_opt_temp_celsius(Some(0), false).as_ref(), "0°C");

        // Gap-aware right-aligned window preserves leading gaps as None (not 0.0).
        let window =
            SparklinePoint::from_opt_f32_window([Some(10.0), None, Some(f32::NAN), Some(40.0)], 6);
        assert_eq!(window.len(), 6);
        assert!(window[0].is_gap());
        assert!(window[1].is_gap());
        assert_eq!(window[2], GapAwareSparklinePoint::new(2, Some(10.0)));
        assert!(window[3].is_gap());
        assert!(window[4].is_gap());
        assert_eq!(window[5], GapAwareSparklinePoint::new(5, Some(40.0)));

        // Contiguous segments preserve true horizontal indices across gaps.
        let segments = SparklinePoint::from_opt_f32_segments(
            [
                Some(12.0),
                Some(18.0),
                None,
                Some(f32::INFINITY),
                Some(45.0),
            ],
            6,
        );
        assert_eq!(
            segments,
            vec![
                vec![SparklinePoint::new(1, 12.0), SparklinePoint::new(2, 18.0)],
                vec![SparklinePoint::new(5, 45.0)],
            ]
        );

        let permille_segs = SparklinePoint::from_permille_sentinel_segments(
            [100, PERMILLE_GAP_SENTINEL, 250, 500],
            4,
        );
        assert_eq!(
            permille_segs,
            vec![
                vec![SparklinePoint::new(0, 10.0)],
                vec![SparklinePoint::new(2, 25.0), SparklinePoint::new(3, 50.0)],
            ]
        );

        let finite_segs = SparklinePoint::from_f32_gap_safe_segments([5.0, f32::NAN, 15.0], 3);
        assert_eq!(
            finite_segs,
            vec![
                vec![SparklinePoint::new(0, 5.0)],
                vec![SparklinePoint::new(2, 15.0)],
            ]
        );

        let padded = SparklinePoint::from_opt_f32_padded([Some(10.0), None, Some(30.0)], 5);
        assert_eq!(
            padded,
            vec![SparklinePoint::new(2, 10.0), SparklinePoint::new(4, 30.0)]
        );
    }
}
