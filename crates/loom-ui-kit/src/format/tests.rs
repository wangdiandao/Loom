use std::sync::Arc;

use loom_types::ResourceDisplayMode;

use super::*;

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

    assert_eq!(format_opt_power_mw(Some(42_500), false).as_ref(), "42.5 W");
    assert_eq!(format_opt_power_mw(Some(42_500), true).as_ref(), "—");
    assert_eq!(format_opt_power_mw(Some(0), false).as_ref(), "—");
    assert_eq!(format_opt_power_mw(None, false).as_ref(), "—");

    assert_eq!(format_opt_fan_percent(Some(0), false).as_ref(), "0%");
    assert_eq!(format_opt_fan_percent(Some(48), false).as_ref(), "48%");
    assert_eq!(format_opt_fan_percent(Some(48), true).as_ref(), "—");
    assert_eq!(format_opt_fan_percent(Some(101), false).as_ref(), "—");

    assert_eq!(format_opt_clock_mhz(Some(2130), false).as_ref(), "2130 MHz");
    assert_eq!(format_opt_clock_mhz(Some(2130), true).as_ref(), "—");
    assert_eq!(format_opt_clock_mhz(Some(0), false).as_ref(), "—");

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
fn honest_optional_formatters() {
    assert_eq!(UNAVAILABLE_LABEL, "—");
    assert_eq!(format_opt_percent_permille(None).as_ref(), "—");
    assert_eq!(format_opt_percent_permille(Some(0)).as_ref(), "0.0%");
    assert_eq!(format_opt_percent_permille(Some(425)).as_ref(), "42.5%");
    assert_eq!(format_opt_percent_permille(Some(1000)).as_ref(), "100.0%");

    assert_eq!(format_opt_temp_celsius(None, false).as_ref(), "—");
    assert_eq!(format_opt_temp_celsius(Some(58), true).as_ref(), "—");
    assert_eq!(format_opt_temp_celsius(Some(58), false).as_ref(), "58°C");
    assert_eq!(format_opt_temp_celsius(Some(0), false).as_ref(), "0°C");
}

#[test]
fn static_lookup_tables_cover_full_ranges_with_pointer_reuse() {
    assert_eq!(
        format_permille_shared(0).as_ptr(),
        format_permille_shared(0).as_ptr()
    );
    assert_eq!(
        format_permille_shared(1_000).as_ptr(),
        format_permille_shared(1_000).as_ptr()
    );

    for p in 0..=1_000u16 {
        let a = format_permille_shared(p);
        let b = format_permille_shared(p);
        assert_eq!(a.as_ref(), format_permille(p));
        assert_eq!(a, b);
    }
    assert_eq!(format_permille_shared(1_001).as_ref(), "100.1%");
    assert_eq!(format_permille_shared(2_500).as_ref(), "250.0%");

    assert_eq!(format_u32_shared(0).as_ptr(), format_u32_shared(0).as_ptr());
    for n in 0..=SMALL_U32_TABLE_MAX {
        let a = format_u32_shared(n);
        let b = format_u32_shared(n);
        assert_eq!(a.as_ref(), n.to_string());
        assert_eq!(a, b);
    }
    assert_eq!(
        format_u32_shared(SMALL_U32_TABLE_MAX + 1).as_ref(),
        (SMALL_U32_TABLE_MAX + 1).to_string()
    );
    assert_eq!(format_u32_shared(u32::MAX).as_ref(), u32::MAX.to_string());

    for c in 0..=CHILD_COUNT_TABLE_MAX {
        let a = format_child_count_shared(c);
        let b = format_child_count_shared(c);
        assert_eq!(a.as_ref(), format!("({c})"));
        assert_eq!(a, b);
    }
    assert_eq!(
        format_child_count_shared(CHILD_COUNT_TABLE_MAX + 1).as_ref(),
        "(257)"
    );
}

#[test]
fn metric_display_values_obey_number_shaping_ascii_invariant() {
    const ALLOWED: &[u8] = b"0123456789.,+-%";
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;

    let assert_number_shaping = |m: &MetricDisplay, ctx: &str| {
        assert!(
            !m.value.is_empty(),
            "{ctx}: MetricDisplay.value must not be empty"
        );
        for &b in m.value.as_bytes() {
            assert!(
                ALLOWED.contains(&b),
                "{ctx}: byte {:?} in {:?} violates gpui-fast::NumberShaping invariant",
                b as char,
                m.value.as_ref()
            );
        }
    };

    let samples: &[u64] = &[
        0,
        1,
        512,
        513,
        KIB - 1,
        KIB,
        124,
        125,
        1_250,
        12_500,
        512 * KIB,
        MIB - 1,
        MIB,
        1_468_006,
        1_550_000,
        1536 * KIB,
        125_000_000,
        GIB - 1,
        GIB,
        16 * GIB + 512 * MIB,
        TIB - 1,
        TIB,
        2 * TIB + 512 * GIB,
        u64::MAX,
    ];

    for &s in samples {
        assert_number_shaping(&format_bytes_metric(s), "bytes");
        assert_number_shaping(&format_rate_bytes_per_sec_metric(s), "rate");
        assert_number_shaping(&format_network_rate_mbps_metric(s), "network");

        for mode in [ResourceDisplayMode::Values, ResourceDisplayMode::Percent] {
            assert_number_shaping(&format_memory_cell_metric(s, 425, mode), "memory_cell");
            assert_number_shaping(&format_disk_cell_metric(s, 425, mode), "disk_cell");
            assert_number_shaping(&format_network_cell_metric(s, 425, mode), "network_cell");
        }
    }

    for p in [0u16, 5, 125, 999, 1_000, 1_250] {
        let pm = format_permille_metric(p);
        assert_number_shaping(&pm, "permille");
        assert_eq!(pm.unit, "");
    }

    assert_eq!(
        format_bytes_metric(0),
        MetricDisplay::from_static("0", " B")
    );
    assert_eq!(format_bytes_metric(512), MetricDisplay::new("512", " B"));
    assert_eq!(format_bytes_metric(KIB), MetricDisplay::new("1", " KB"));
    assert_eq!(
        format_bytes_metric(1536 * KIB),
        MetricDisplay::new("1.5", " MB")
    );
    assert_eq!(
        format_bytes_metric(16 * GIB + 512 * MIB),
        MetricDisplay::new("16.5", " GB")
    );
    assert_eq!(
        format_bytes_metric(2 * TIB + 512 * GIB),
        MetricDisplay::new("2.5", " TB")
    );

    let signed_samples = [
        i64::MIN,
        -((2 * TIB + 512 * GIB) as i64),
        -((16 * GIB + 512 * MIB) as i64),
        -((1536 * KIB) as i64),
        -(4 * KIB as i64),
        -512,
        -1,
        0,
        1,
        512,
        4 * KIB as i64,
        (1536 * KIB) as i64,
        (16 * GIB + 512 * MIB) as i64,
        (2 * TIB + 512 * GIB) as i64,
        i64::MAX,
    ];
    for s in signed_samples {
        assert_number_shaping(&format_signed_bytes_metric(s), "signed_bytes");
    }
    assert_eq!(format_signed_bytes(0), "0 B");
    assert_eq!(format_signed_bytes(512), "+512 B");
    assert_eq!(format_signed_bytes(-512), "-512 B");
    assert_eq!(format_signed_bytes(4 * KIB as i64), "+4 KB");
    assert_eq!(format_signed_bytes(-(4 * KIB as i64)), "-4 KB");
    assert_eq!(format_signed_bytes((1536 * KIB) as i64), "+1.5 MB");
    assert_eq!(format_signed_bytes(-((1536 * KIB) as i64)), "-1.5 MB");
    assert_eq!(
        format_signed_bytes_metric(0),
        MetricDisplay::from_static("0", " B")
    );
    assert_eq!(
        format_signed_bytes_metric(512),
        MetricDisplay::new("+512", " B")
    );
    assert_eq!(
        format_signed_bytes_metric(-512),
        MetricDisplay::new("-512", " B")
    );
    assert_eq!(
        format_signed_bytes_metric(4 * KIB as i64),
        MetricDisplay::new("+4", " KB")
    );
    assert_eq!(
        format_signed_bytes_metric(-(4 * KIB as i64)),
        MetricDisplay::new("-4", " KB")
    );
    assert_eq!(
        format_signed_bytes_metric((1536 * KIB) as i64),
        MetricDisplay::new("+1.5", " MB")
    );
    assert_eq!(
        format_signed_bytes_metric(-((1536 * KIB) as i64)),
        MetricDisplay::new("-1.5", " MB")
    );

    assert_eq!(
        format_rate_bytes_per_sec_metric(0),
        MetricDisplay::from_static("0", " MB/s")
    );
    assert_eq!(
        format_rate_bytes_per_sec_metric(512),
        MetricDisplay::new("512", " B/s")
    );
    assert_eq!(
        format_rate_bytes_per_sec_metric(512 * KIB),
        MetricDisplay::new("512.0", " KB/s")
    );
    assert_eq!(
        format_rate_bytes_per_sec_metric(1_468_006),
        MetricDisplay::new("1.4", " MB/s")
    );
    assert_eq!(
        format_rate_bytes_per_sec_metric(2 * GIB),
        MetricDisplay::new("2.0", " GB/s")
    );

    assert_eq!(
        format_network_rate_mbps_metric(0),
        MetricDisplay::from_static("0", " Mbps")
    );
    assert_eq!(
        format_network_rate_mbps_metric(100),
        MetricDisplay::new("800", " bps")
    );
    assert_eq!(
        format_network_rate_mbps_metric(1_250),
        MetricDisplay::new("10.0", " Kbps")
    );
    assert_eq!(
        format_network_rate_mbps_metric(1_550_000),
        MetricDisplay::new("12.4", " Mbps")
    );
    assert_eq!(
        format_network_rate_mbps_metric(125_000_000),
        MetricDisplay::new("1.0", " Gbps")
    );
}

#[test]
fn quantization_keys_match_display_resolution_buckets() {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;

    assert_eq!(
        quantize_bytes_display(MIB),
        quantize_bytes_display(MIB + 50)
    );
    assert_eq!(format_bytes_metric(MIB), format_bytes_metric(MIB + 50));

    assert_ne!(
        quantize_bytes_display(MIB),
        quantize_bytes_display(MIB + 120 * KIB)
    );
    assert_ne!(
        format_bytes_metric(MIB),
        format_bytes_metric(MIB + 120 * KIB)
    );

    assert_ne!(quantize_bytes_display(0), quantize_bytes_display(1));
    assert_ne!(quantize_bytes_display(KIB - 1), quantize_bytes_display(KIB));
    assert_ne!(quantize_bytes_display(MIB - 1), quantize_bytes_display(MIB));

    assert_eq!(quantize_signed_bytes_display(0), 0);
    assert_ne!(
        quantize_signed_bytes_display(0),
        quantize_signed_bytes_display(1)
    );
    assert_ne!(
        quantize_signed_bytes_display(0),
        quantize_signed_bytes_display(-1)
    );
    assert_ne!(
        quantize_signed_bytes_display(4 * KIB as i64),
        quantize_signed_bytes_display(-(4 * KIB as i64))
    );
    assert_eq!(
        quantize_signed_bytes_display(MIB as i64),
        quantize_signed_bytes_display(MIB as i64 + 50)
    );
    assert_eq!(
        quantize_signed_bytes_display(-(MIB as i64)),
        quantize_signed_bytes_display(-(MIB as i64 + 50))
    );
    assert_ne!(
        quantize_signed_bytes_display(-(MIB as i64)),
        quantize_signed_bytes_display(-(MIB as i64 + 120 * KIB as i64))
    );

    assert_eq!(
        quantize_rate_bps_display(MIB),
        quantize_rate_bps_display(MIB + 100)
    );
    assert_ne!(quantize_rate_bps_display(0), quantize_rate_bps_display(1));

    assert_eq!(
        quantize_net_bps_display(1_550_000),
        quantize_net_bps_display(1_550_100)
    );
    assert_ne!(
        quantize_net_bps_display(1_550_000),
        quantize_net_bps_display(1_565_000)
    );

    assert_eq!(quantize_cpu_time_secs(0), 0);
    assert_eq!(quantize_cpu_time_secs(9_999_999), 0);
    assert_eq!(quantize_cpu_time_secs(10_000_000), 1);
    assert_eq!(quantize_cpu_time_secs(19_999_999), 1);
    assert_eq!(quantize_cpu_time_secs(20_000_000), 2);
}

#[test]
fn single_line_preview_capping_handles_ascii_cjk_newlines_and_boundaries() {
    assert_eq!(DEFAULT_CELL_PREVIEW_CHARS, 256);

    assert_eq!(cap_single_line_preview("", 0), None);
    assert_eq!(cap_single_line_preview("", 256), None);
    assert_eq!(cap_single_line_preview("abcd", 4), None);
    assert_eq!(cap_single_line_preview("你好世界", 4), None);

    assert_eq!(cap_single_line_preview("abcde", 4), Some("abcd…".into()));
    assert_eq!(
        cap_single_line_preview("你好世界啊", 4),
        Some("你好世界…".into())
    );
    assert_eq!(cap_single_line_preview("a", 0), Some("…".into()));

    assert_eq!(
        cap_single_line_preview("line1\nline2", 256),
        Some("line1 line2".into())
    );
    assert_eq!(
        cap_single_line_preview("a\r\nb\tc", 256),
        Some("a  b c".into())
    );
    assert_eq!(cap_single_line_preview("a\nbcd", 3), Some("a b…".into()));

    let clean: Arc<str> = Arc::from("C:\\Windows\\System32\\svchost.exe");
    let reused = display_preview_shared(&clean, DEFAULT_CELL_PREVIEW_CHARS);
    assert_eq!(reused.as_ptr(), clean.as_ptr());

    let long: Arc<str> = Arc::from("x".repeat(300));
    let capped = display_preview_shared(&long, DEFAULT_CELL_PREVIEW_CHARS);
    assert_eq!(capped.chars().count(), DEFAULT_CELL_PREVIEW_CHARS + 1);
    assert!(capped.ends_with('…'));
}

#[test]
fn chunked_tenths_and_signed_delta_tables_cover_ranges_and_fallbacks() {
    let u1 = format_u32_shared(1024);
    let u2 = format_u32_shared(1024);
    assert_eq!(u1.as_ref(), "1024");
    assert_eq!(u1, u2);

    for sample in [0u64, 9, 10, 99, 100, 124, 3670, 9999, TENTHS_TABLE_MAX] {
        let expected = format!("{}.{}", sample / 10, sample % 10);
        let a = format_tenths_shared(sample);
        let b = format_tenths_shared(sample);
        assert_eq!(a.as_ref(), expected);
        assert_eq!(a, b);
    }

    assert_eq!(
        format_tenths_shared(TENTHS_TABLE_MAX + 1).as_ref(),
        "1030.0"
    );

    let p_kb1 = format_signed_bytes_metric(512 * 1024);
    let p_kb2 = format_signed_bytes_metric(512 * 1024);
    assert_eq!(p_kb1.value.as_ref(), "+512");
    assert_eq!(p_kb1.unit, " KB");
    assert_eq!(p_kb1, p_kb2);

    let n_mb1 = format_signed_bytes_metric(-(15 * 1024 * 1024) / 10);
    let n_mb2 = format_signed_bytes_metric(-(15 * 1024 * 1024) / 10);
    assert_eq!(n_mb1.value.as_ref(), "-1.5");
    assert_eq!(n_mb1.unit, " MB");
    assert_eq!(n_mb1, n_mb2);

    let huge = format_signed_bytes_metric(250 * 1024 * 1024);
    assert_eq!(huge.value.as_ref(), "+250.0");
    assert_eq!(huge.unit, " MB");
}
