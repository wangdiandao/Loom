//! Network adapter detail pane for the Performance view (stepped Send/Receive chart + IP/DNS specs).

use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::v_flex;
use gpui_kit::{
    Context, InteractiveElement as _, IntoElement, ParentElement as _, SharedString, Styled as _,
};
use loom_types::NetAdapterSlice;
use loom_ui_kit::{LoomSemanticColors, format_bits_per_sec, format_network_rate_mbps};

use crate::features::performance::PerformanceView;
use crate::features::performance::cache::PerformanceSeriesCache;
use crate::features::performance::charts::{
    MetricStat, SpecRow, build_dual_throughput_area_chart, chart_frame, copy_summary_menu_item,
    device_header, format_stepped_ceiling_label, legend_swatch, metrics_two_column_panel,
    non_empty_arc_shared, opt_arc_str_or_fallback,
};
use crate::i18n;

/// Renders the Network adapter (`Ethernet` / `Wi-Fi`) detail pane.
pub(crate) fn render_network_detail(
    adapter: &NetAdapterSlice,
    cache: &mut PerformanceSeriesCache,
    colors: &LoomSemanticColors,
    cx: &mut Context<PerformanceView>,
) -> impl IntoElement {
    let unavailable = i18n::tr("perf.unavailable");
    let conn_label = i18n::net_connection_type_label(adapter.connection_type);

    let description: SharedString = if !adapter.description.trim().is_empty() {
        non_empty_arc_shared(&adapter.description, &unavailable)
    } else {
        non_empty_arc_shared(&adapter.name, &unavailable)
    };

    let throughput = cache.network_throughput(adapter);
    let scale_label = format_stepped_ceiling_label(throughput.y_max_unit, throughput.unit_label);
    let y_max_unit = throughput.y_max_unit as f32;
    let divisor = throughput.divisor;
    let throughput_points = throughput.points.clone();

    let recv_label = i18n::tr("perf.net_receive");
    let send_label = i18n::tr("perf.net_send");

    // Plot in the stepped ceiling's unit (Kbps / Mbps / Gbps) via `divisor`.
    let throughput_chart = build_dual_throughput_area_chart(
        SharedString::from(format!("perf-net-throughput-{}", adapter.luid)),
        throughput_points,
        y_max_unit,
        recv_label.clone(),
        colors.chart_network,
        send_label.clone(),
        colors.chart_network_send,
        move |val| format_bits_per_sec(((val.max(0.0) * divisor) * 8.0) as u64),
    );
    let legend = vec![
        legend_swatch(recv_label.clone(), colors.chart_network, colors).into_any_element(),
        legend_swatch(send_label.clone(), colors.chart_network_send, colors).into_any_element(),
    ];

    let link_speed_str = if adapter.link_speed_bps > 0 {
        format_bits_per_sec(adapter.link_speed_bps)
    } else {
        unavailable.clone()
    };
    let adapter_name = non_empty_arc_shared(&adapter.name, &unavailable);

    let left_stats = vec![
        MetricStat::new(
            send_label,
            format_network_rate_mbps(adapter.sent_bytes_per_sec),
        ),
        MetricStat::new(
            recv_label,
            format_network_rate_mbps(adapter.recv_bytes_per_sec),
        ),
    ];

    let right_specs = vec![
        SpecRow::new(i18n::tr("perf.net_adapter_name"), adapter_name),
        SpecRow::new(i18n::tr("perf.net_connection_type"), conn_label.clone()),
        SpecRow::new(i18n::tr("perf.net_link_speed"), link_speed_str),
        SpecRow::new(
            i18n::tr("perf.net_ipv4"),
            opt_arc_str_or_fallback(&adapter.ipv4, &unavailable),
        ),
        SpecRow::new(
            i18n::tr("perf.net_ipv6"),
            opt_arc_str_or_fallback(&adapter.ipv6, &unavailable),
        ),
        SpecRow::new(
            i18n::tr("perf.net_dns_suffix"),
            opt_arc_str_or_fallback(&adapter.dns_suffix, &unavailable),
        ),
    ];

    let weak_ctx = cx.entity().downgrade();
    v_flex()
        .id(SharedString::from(format!(
            "perf-net-detail-{}",
            adapter.luid
        )))
        .w_full()
        .gap_4()
        .child(device_header(conn_label, description, None, colors))
        .child(chart_frame(
            i18n::tr("perf.net_throughput_chart"),
            scale_label,
            legend,
            colors.chart_network,
            240.0,
            throughput_chart,
            colors,
        ))
        .child(metrics_two_column_panel(left_stats, right_specs, colors))
        .context_menu(move |menu, _window, _cx| menu.item(copy_summary_menu_item(weak_ctx.clone())))
}
