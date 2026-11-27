//! Disk detail pane for the Performance view (active time %, adaptive Read/Write chart, latency, specs).

use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::{IntoElement, ParentElement as _, SharedString, Styled as _, div};
use loom_types::DiskDeviceSlice;
use loom_ui_kit::{
    LoomSemanticColors, adaptive_throughput_scale, format_bytes, format_latency_us,
    format_permille, format_rate_bytes_per_sec,
};

use crate::features::performance::charts::{
    MetricStat, SpecRow, build_dual_throughput_area_chart, build_percent_area_chart, chart_frame,
    device_header, dual_throughput_rings_to_points, history_ring_to_points, legend_swatch,
    metrics_two_column_panel,
};
use crate::i18n;

/// Renders the physical Disk (`Disk 0..N`) detail pane.
pub(crate) fn render_disk_detail(
    disk: &DiskDeviceSlice,
    colors: &LoomSemanticColors,
) -> impl IntoElement {
    let unavailable = i18n::tr("perf.unavailable");
    let title = i18n::perf_disk_title(disk.index, disk.drive_letters.as_ref());
    let media_label = i18n::disk_media_type_label(disk.media_type);

    let model: SharedString = if disk.model.trim().is_empty() {
        unavailable.clone().into()
    } else {
        SharedString::from(disk.model.as_ref().to_owned())
    };

    let mut badges = h_flex().items_center().gap_1p5().child(
        div()
            .px_2()
            .py_0p5()
            .rounded_sm()
            .bg(colors.chart_disk.opacity(0.16))
            .text_xs()
            .font_medium()
            .text_color(colors.chart_disk)
            .child(media_label.clone()),
    );
    if disk.circuit_open {
        badges = badges.child(
            div()
                .px_2()
                .py_0p5()
                .rounded_sm()
                .bg(colors.status_hung_bg)
                .text_xs()
                .font_medium()
                .text_color(colors.status_hung_fg)
                .child(i18n::tr("perf.disk_circuit_open")),
        );
    }

    let active_points = history_ring_to_points(&disk.active_history);
    let active_chart = build_percent_area_chart(
        SharedString::from(format!("perf-disk-active-{}", disk.index)),
        active_points,
        i18n::tr("perf.disk_active_time"),
        colors.chart_disk,
        true,
    );

    let max_rw_bps = disk
        .read_history
        .max_value()
        .max(disk.write_history.max_value())
        .max(disk.read_bytes_per_sec)
        .max(disk.write_bytes_per_sec);
    let (y_max_bps, scale_label) = adaptive_throughput_scale(max_rw_bps);

    let read_label = i18n::tr("perf.disk_read");
    let write_label = i18n::tr("perf.disk_write");
    let transfer_points =
        dual_throughput_rings_to_points(&disk.read_history, &disk.write_history, 1.0);
    let transfer_chart = build_dual_throughput_area_chart(
        SharedString::from(format!("perf-disk-transfer-{}", disk.index)),
        transfer_points,
        y_max_bps,
        read_label.clone(),
        colors.chart_disk,
        write_label.clone(),
        colors.chart_disk_write,
        |val| format_rate_bytes_per_sec(val.max(0.0) as u64),
    );
    let transfer_legend = vec![
        legend_swatch(read_label, colors.chart_disk, colors).into_any_element(),
        legend_swatch(write_label, colors.chart_disk_write, colors).into_any_element(),
    ];

    let capacity_str = if disk.capacity_bytes > 0 {
        format_bytes(disk.capacity_bytes)
    } else {
        unavailable.clone()
    };
    let drive_letters_str = if disk.drive_letters.trim().is_empty() {
        unavailable
    } else {
        disk.drive_letters.as_ref().to_owned()
    };

    let left_stats = vec![
        MetricStat::new(
            i18n::tr("perf.disk_active_time"),
            format_permille(disk.active_time_permille),
        ),
        MetricStat::new(
            i18n::tr("perf.disk_avg_response"),
            format_latency_us(disk.avg_response_us),
        ),
        MetricStat::new(
            i18n::tr("perf.disk_read_speed"),
            format_rate_bytes_per_sec(disk.read_bytes_per_sec),
        ),
        MetricStat::new(
            i18n::tr("perf.disk_write_speed"),
            format_rate_bytes_per_sec(disk.write_bytes_per_sec),
        ),
    ];

    let right_specs = vec![
        SpecRow::new(i18n::tr("perf.disk_capacity"), capacity_str),
        SpecRow::new(i18n::tr("perf.disk_drive_letters"), drive_letters_str),
        SpecRow::new(i18n::tr("perf.disk_type"), media_label),
    ];

    v_flex()
        .w_full()
        .gap_4()
        .child(device_header(
            title,
            model,
            Some(badges.into_any_element()),
            colors,
        ))
        .child(chart_frame(
            i18n::tr("perf.disk_active_chart"),
            "100%",
            Vec::new(),
            colors.chart_disk,
            175.0,
            active_chart,
            colors,
        ))
        .child(chart_frame(
            i18n::tr("perf.disk_transfer_chart"),
            scale_label,
            transfer_legend,
            colors.chart_disk,
            155.0,
            transfer_chart,
            colors,
        ))
        .child(metrics_two_column_panel(left_stats, right_specs, colors))
}
