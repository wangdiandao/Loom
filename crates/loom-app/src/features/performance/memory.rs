//! Memory detail pane for the Performance view (60s usage chart, 4-segment composition bar, SMBIOS specs).

use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::{
    Context, InteractiveElement as _, IntoElement, ParentElement as _, SharedString, Styled as _,
    div,
};
use loom_types::PerformanceSlice;
use loom_ui_kit::{
    LoomSemanticColors, MemoryBarLabels, MemoryBarSegments, format_bytes, memory_composition_bar,
};

use crate::features::performance::PerformanceView;
use crate::features::performance::cache::PerformanceSeriesCache;
use crate::features::performance::charts::{
    MetricStat, SpecRow, build_percent_area_chart, chart_frame, copy_summary_menu_item,
    device_header, legend_swatch, metrics_two_column_panel, opt_arc_str_or_fallback,
};
use crate::i18n;

/// Renders the Memory hardware detail pane.
pub(crate) fn render_memory_detail(
    perf: &PerformanceSlice,
    cache: &mut PerformanceSeriesCache,
    colors: &LoomSemanticColors,
    cx: &mut Context<PerformanceView>,
) -> impl IntoElement {
    let latest = &perf.latest;
    let info = &perf.memory_info;
    let unavailable = i18n::tr("perf.unavailable");

    let total_str = format_bytes(latest.mem_total_bytes);
    let mut header_parts = vec![total_str.clone()];
    if let Some(speed) = info.speed_mt_s
        && speed > 0
    {
        header_parts.push(format!("{speed} MT/s"));
    }
    if let Some(ref ff) = info.form_factor
        && !ff.trim().is_empty()
    {
        header_parts.push(ff.as_ref().to_owned());
    }
    let header_subtitle = SharedString::from(header_parts.join(" · "));

    let points = cache.memory_usage(perf).to_vec();
    let usage_chart = build_percent_area_chart(
        "perf-memory-usage-chart",
        points,
        i18n::tr("perf.mem_usage_chart"),
        colors.chart_memory,
        true,
    );

    let (modified, standby, free) = if latest.mem_modified_bytes == 0
        && latest.mem_standby_bytes == 0
        && latest.mem_free_bytes == 0
        && latest.mem_available_bytes > 0
    {
        (0, 0, latest.mem_available_bytes)
    } else {
        (
            latest.mem_modified_bytes,
            latest.mem_standby_bytes,
            latest.mem_free_bytes,
        )
    };

    let segments = MemoryBarSegments {
        in_use_bytes: latest.mem_used_bytes,
        modified_bytes: modified,
        standby_bytes: standby,
        free_bytes: free,
    };

    let in_use_label = i18n::tr("perf.mem_in_use");
    let modified_label = i18n::tr("perf.mem_modified");
    let standby_label = i18n::tr("perf.mem_standby");
    let free_label = i18n::tr("perf.mem_free");

    let bar_labels = MemoryBarLabels::new(
        in_use_label.clone(),
        modified_label.clone(),
        standby_label.clone(),
        free_label.clone(),
    );

    let composition_section = v_flex()
        .w_full()
        .gap_2()
        .child(
            div()
                .text_xs()
                .font_medium()
                .text_color(colors.text_muted)
                .child(i18n::tr("perf.mem_composition")),
        )
        .child(memory_composition_bar(
            "perf-memory-composition-bar",
            segments,
            &bar_labels,
            colors,
        ))
        .child(
            h_flex()
                .w_full()
                .flex_wrap()
                .items_center()
                .gap_4()
                .child(legend_swatch(
                    format!("{in_use_label} ({})", format_bytes(segments.in_use_bytes)),
                    colors.mem_in_use,
                    colors,
                ))
                .child(legend_swatch(
                    format!(
                        "{modified_label} ({})",
                        format_bytes(segments.modified_bytes)
                    ),
                    colors.mem_modified,
                    colors,
                ))
                .child(legend_swatch(
                    format!("{standby_label} ({})", format_bytes(segments.standby_bytes)),
                    colors.mem_standby,
                    colors,
                ))
                .child(legend_swatch(
                    format!("{free_label} ({})", format_bytes(segments.free_bytes)),
                    colors.mem_free,
                    colors,
                )),
        );

    let cached_bytes = latest
        .mem_standby_bytes
        .saturating_add(latest.mem_modified_bytes);
    let committed_str = format!(
        "{} / {}",
        format_bytes(latest.mem_committed_bytes),
        format_bytes(latest.mem_commit_limit_bytes)
    );

    let speed_str = match info.speed_mt_s {
        Some(speed) if speed > 0 => format!("{speed} MT/s"),
        _ => unavailable.clone(),
    };

    let slots_str = match (info.slots_used, info.slots_total) {
        (Some(used), Some(total)) if total > 0 => format!("{used} / {total}"),
        (Some(used), _) => used.to_string(),
        _ => unavailable.clone(),
    };

    let form_factor_str = opt_arc_str_or_fallback(&info.form_factor, &unavailable);

    let left_stats = vec![
        MetricStat::new(in_use_label, format_bytes(latest.mem_used_bytes)),
        MetricStat::new(
            i18n::tr("perf.mem_available"),
            format_bytes(latest.mem_available_bytes),
        ),
        MetricStat::new(i18n::tr("perf.mem_committed"), committed_str),
        MetricStat::new(i18n::tr("perf.mem_cached"), format_bytes(cached_bytes)),
        MetricStat::new(
            i18n::tr("perf.mem_paged_pool"),
            format_bytes(latest.mem_paged_pool_bytes),
        ),
        MetricStat::new(
            i18n::tr("perf.mem_nonpaged_pool"),
            format_bytes(latest.mem_nonpaged_pool_bytes),
        ),
    ];

    let right_specs = vec![
        SpecRow::new(i18n::tr("perf.mem_speed"), speed_str),
        SpecRow::new(i18n::tr("perf.mem_slots_used"), slots_str),
        SpecRow::new(i18n::tr("perf.mem_form_factor"), form_factor_str),
        SpecRow::new(
            i18n::tr("perf.mem_hardware_reserved"),
            format_bytes(info.hardware_reserved_bytes),
        ),
    ];

    let weak_ctx = cx.entity().downgrade();
    v_flex()
        .id("perf-memory-detail")
        .w_full()
        .gap_4()
        .child(device_header(
            i18n::tr("perf.memory"),
            header_subtitle,
            None,
            colors,
        ))
        .child(chart_frame(
            i18n::tr("perf.mem_usage_chart"),
            total_str,
            Vec::new(),
            colors.chart_memory,
            220.0,
            usage_chart,
            colors,
        ))
        .child(composition_section)
        .child(metrics_two_column_panel(left_stats, right_specs, colors))
        .context_menu(move |menu, _window, _cx| menu.item(copy_summary_menu_item(weak_ctx.clone())))
}
