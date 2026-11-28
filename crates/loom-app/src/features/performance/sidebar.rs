//! Left-hand hardware device sidebar with cached 60s sparkline cards for [`super::PerformanceView`].

use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::v_flex;
use gpui_kit::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, div,
};
use loom_types::{GpuAdapterSlice, NetAdapterSlice, PerformanceSlice};
use loom_ui_kit::{
    LoomSemanticColors, SparklineCardProps, format_bytes, format_freq_ghz,
    format_network_rate_mbps, format_opt_temp_celsius, sparkline_card,
};

use crate::i18n;

use super::PerformanceView;
use super::cache::PerformanceSeriesCache;
use super::charts::non_empty_arc_shared;
use super::state::{PerformanceViewState, SelectedDevice};

/// Renders the scrollable left device list using cached per-revision sparkline series.
pub(crate) fn render_device_sidebar(
    perf: &PerformanceSlice,
    selected: SelectedDevice,
    state: &mut PerformanceViewState,
    cache: &PerformanceSeriesCache,
    colors: &LoomSemanticColors,
    cx: &mut Context<PerformanceView>,
) -> AnyElement {
    let latest = &perf.latest;
    let mut list = v_flex().w_full().gap_2().p_3();

    // 1. CPU card
    let cpu_pct = format!("{:.0}%", f32::from(latest.cpu_total_permille) / 10.0);
    let cpu_subtitle: SharedString = if latest.cpu_freq_mhz > 0 {
        format_freq_ghz(latest.cpu_freq_mhz).into()
    } else if !perf.cpu_info.brand_name.trim().is_empty() {
        non_empty_arc_shared(&perf.cpu_info.brand_name, "")
    } else {
        SharedString::new_static("")
    };
    list = list.child(
        sparkline_card(
            SparklineCardProps {
                id: "perf-card-cpu".into(),
                title: i18n::tr("perf.cpu").into(),
                subtitle: cpu_subtitle,
                primary_metric: cpu_pct.into(),
                points: cache.cpu_sparkline().to_vec(),
                y_max: 100.0,
                accent_color: colors.chart_cpu,
                selected: selected == SelectedDevice::Cpu,
            },
            colors,
        )
        .on_click(cx.listener(|view, _, window, cx| {
            view.select_device(SelectedDevice::Cpu, window, cx);
        })),
    );

    // 2. Memory card
    let mem_permille = latest.mem_util_permille();
    let mem_pct = format!("{:.0}%", f32::from(mem_permille) / 10.0);
    let mem_subtitle = format!(
        "{} / {}",
        format_bytes(latest.mem_used_bytes),
        format_bytes(latest.mem_total_bytes)
    );
    list = list.child(
        sparkline_card(
            SparklineCardProps {
                id: "perf-card-memory".into(),
                title: i18n::tr("perf.memory").into(),
                subtitle: mem_subtitle.into(),
                primary_metric: mem_pct.into(),
                points: cache.mem_sparkline().to_vec(),
                y_max: 100.0,
                accent_color: colors.chart_memory,
                selected: selected == SelectedDevice::Memory,
            },
            colors,
        )
        .on_click(cx.listener(|view, _, window, cx| {
            view.select_device(SelectedDevice::Memory, window, cx);
        })),
    );

    // 3. Disk 0..N cards (stable open-time ordering via merge_stable_order)
    let disk_order = state.ordered_disk_indices(&perf.disks);
    for disk_slice_idx in disk_order {
        let disk = &perf.disks[disk_slice_idx];
        let disk_index = disk.index;
        let title = i18n::perf_disk_title(disk.index, disk.drive_letters.as_ref());
        let active_pct = format!("{:.0}%", f32::from(disk.active_time_permille) / 10.0);
        let media = i18n::disk_media_type_label(disk.media_type);
        list = list.child(
            sparkline_card(
                SparklineCardProps {
                    id: SharedString::from(format!("perf-card-disk-{disk_index}")),
                    title: title.into(),
                    subtitle: media.into(),
                    primary_metric: active_pct.into(),
                    points: cache.disk_sparkline(disk_index).to_vec(),
                    y_max: 100.0,
                    accent_color: colors.chart_disk,
                    selected: selected == SelectedDevice::Disk(disk_index),
                },
                colors,
            )
            .on_click(cx.listener(move |view, _, window, cx| {
                view.select_device(SelectedDevice::Disk(disk_index), window, cx);
            })),
        );
    }

    // 4. Network 0..N cards (stable open-time ordering + stepped ceiling + gap-safe max)
    let send_short = i18n::tr("perf.send_short");
    let recv_short = i18n::tr("perf.recv_short");
    let net_order = state.ordered_network_indices(&perf.networks);
    for net_slice_idx in net_order {
        let net = &perf.networks[net_slice_idx];
        let luid = net.luid;
        let title = format_network_card_title(net);
        let subtitle = format!(
            "{send_short}: {}  {recv_short}: {}",
            format_network_rate_mbps(net.sent_bytes_per_sec),
            format_network_rate_mbps(net.recv_bytes_per_sec)
        );
        let (combined_points, y_max) = cache
            .network_sparkline(luid)
            .map(|c| (c.points.clone(), c.y_max_unit))
            .unwrap_or_else(|| (Vec::new(), 1.0));

        list = list.child(
            sparkline_card(
                SparklineCardProps {
                    id: SharedString::from(format!("perf-card-net-{luid}")),
                    title: title.into(),
                    subtitle: subtitle.into(),
                    primary_metric: SharedString::new_static(""),
                    points: combined_points,
                    y_max,
                    accent_color: colors.chart_network,
                    selected: selected == SelectedDevice::Network(luid),
                },
                colors,
            )
            .on_click(cx.listener(move |view, _, window, cx| {
                view.select_device(SelectedDevice::Network(luid), window, cx);
            })),
        );
    }

    // 5. GPU 0..N cards (honest D3-asleep readout without fake 0°C)
    let unavailable = i18n::tr("perf.unavailable");
    for gpu in &perf.gpus {
        let luid = gpu.luid;
        let title = i18n::perf_gpu_title(gpu.index);
        let primary_metric = format_gpu_sidebar_metric(gpu);
        let subtitle = non_empty_arc_shared(&gpu.name, &unavailable);
        list = list.child(
            sparkline_card(
                SparklineCardProps {
                    id: SharedString::from(format!("perf-card-gpu-{luid}")),
                    title: title.into(),
                    subtitle,
                    primary_metric,
                    points: cache.gpu_sparkline(luid).to_vec(),
                    y_max: 100.0,
                    accent_color: colors.chart_gpu,
                    selected: selected == SelectedDevice::Gpu(luid),
                },
                colors,
            )
            .on_click(cx.listener(move |view, _, window, cx| {
                view.select_device(SelectedDevice::Gpu(luid), window, cx);
            })),
        );
    }

    div()
        .id("perf-device-sidebar")
        .size_full()
        .overflow_y_scrollbar()
        .child(list)
        .into_any_element()
}

fn format_network_card_title(net: &NetAdapterSlice) -> String {
    let conn = i18n::net_connection_type_label(net.connection_type);
    let name = net.name.trim();
    if name.is_empty() || name.eq_ignore_ascii_case(&conn) {
        conn
    } else {
        format!("{conn} ({name})")
    }
}

/// Formats the primary metric displayed on a sidebar GPU sparkline card,
/// rendering an honest localized `"Sleeping (D3)"` label when powered down.
pub(crate) fn format_gpu_sidebar_metric(gpu: &GpuAdapterSlice) -> SharedString {
    if gpu.d3_asleep {
        return i18n::tr("perf.perf_polish_gpu_sleeping").into();
    }
    let util_pct = format!("{:.0}%", f32::from(gpu.util_permille) / 10.0);
    if gpu.temperature_celsius.is_some() {
        let temp = format_opt_temp_celsius(gpu.temperature_celsius, gpu.d3_asleep);
        format!("{util_pct} · {temp}").into()
    } else {
        util_pct.into()
    }
}
