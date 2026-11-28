//! Network adapter detail pane for the Performance view (stepped Send/Receive chart + IP/DNS specs).

use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::v_flex;
use gpui_kit::{
    Context, InteractiveElement as _, IntoElement, ParentElement as _, SharedString, Styled as _,
};
use loom_types::NetAdapterSlice;
use loom_ui_kit::{
    LoomSemanticColors, format_bits_per_sec, format_network_rate_mbps, stepped_network_ceiling,
};

use crate::features::performance::PerformanceView;
use crate::features::performance::charts::{
    MetricStat, SpecRow, build_dual_throughput_area_chart, chart_frame, device_header,
    dual_throughput_rings_to_points, format_stepped_ceiling_label, legend_swatch,
    metrics_two_column_panel,
};
use crate::i18n;

/// Renders the Network adapter (`Ethernet` / `Wi-Fi`) detail pane.
pub(crate) fn render_network_detail(
    adapter: &NetAdapterSlice,
    colors: &LoomSemanticColors,
    cx: &mut Context<PerformanceView>,
) -> impl IntoElement {
    let unavailable = i18n::tr("perf.unavailable");
    let conn_label = i18n::net_connection_type_label(adapter.connection_type);

    let description: SharedString = if !adapter.description.trim().is_empty() {
        SharedString::from(adapter.description.as_ref().to_owned())
    } else if !adapter.name.trim().is_empty() {
        SharedString::from(adapter.name.as_ref().to_owned())
    } else {
        unavailable.clone().into()
    };

    let max_bytes_per_sec = adapter
        .recv_history
        .max_value()
        .max(adapter.sent_history.max_value())
        .max(adapter.recv_bytes_per_sec)
        .max(adapter.sent_bytes_per_sec);
    let (y_max_unit, unit_label, divisor) = stepped_network_ceiling(max_bytes_per_sec);
    let scale_label = format_stepped_ceiling_label(y_max_unit, unit_label);

    let recv_label = i18n::tr("perf.net_receive");
    let send_label = i18n::tr("perf.net_send");

    // Plot in the stepped ceiling's unit (Kbps / Mbps / Gbps) via `divisor`.
    let throughput_points =
        dual_throughput_rings_to_points(&adapter.recv_history, &adapter.sent_history, divisor);
    let throughput_chart = build_dual_throughput_area_chart(
        SharedString::from(format!("perf-net-throughput-{}", adapter.luid)),
        throughput_points,
        y_max_unit as f32,
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

    let adapter_name = if adapter.name.trim().is_empty() {
        unavailable.clone()
    } else {
        adapter.name.as_ref().to_owned()
    };

    let opt_str = |opt: &Option<std::sync::Arc<str>>| {
        opt.as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| unavailable.clone())
    };

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
        SpecRow::new(i18n::tr("perf.net_ipv4"), opt_str(&adapter.ipv4)),
        SpecRow::new(i18n::tr("perf.net_ipv6"), opt_str(&adapter.ipv6)),
        SpecRow::new(
            i18n::tr("perf.net_dns_suffix"),
            opt_str(&adapter.dns_suffix),
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
        .context_menu(move |menu, _window, _cx| {
            let weak_copy = weak_ctx.clone();
            menu.item(
                PopupMenuItem::new(i18n::tr("perf.perf_polish_copy_summary")).on_click(
                    move |_, window, cx| {
                        let _ = weak_copy.update(cx, |view, cx| {
                            view.copy_selected_summary(window, cx);
                        });
                    },
                ),
            )
        })
}
