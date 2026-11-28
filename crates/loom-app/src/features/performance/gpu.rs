//! GPU detail pane for the Performance view (configurable `1..=8` engine charts, VRAM charts, honest D3 sleep & specs).

use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::select::{Select, SelectItem, SelectState};
use gpui_kit::component::{Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::{
    Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    Styled as _, div, prelude::FluentBuilder as _, px,
};
use loom_types::GpuAdapterSlice;
use loom_ui_kit::{
    LoomSemanticColors, format_bytes, format_opt_percent_permille, format_opt_temp_celsius,
};

use crate::features::performance::PerformanceView;
use crate::features::performance::charts::{
    MetricStat, SpecRow, build_percent_area_chart, chart_frame, device_header,
    history_ring_to_points, metrics_two_column_panel,
};
use crate::features::performance::state::MAX_GPU_ENGINE_PANES;
use crate::i18n;

/// Selectable GPU engine-pane count option (`1..=N`) backing the count dropdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GpuPaneCountOption {
    pub(crate) count: usize,
    pub(crate) label: SharedString,
}

impl SelectItem for GpuPaneCountOption {
    type Value = usize;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.count
    }
}

/// Selectable GPU execution engine option backing each per-pane [`SelectState`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GpuEngineOption {
    pub(crate) index: usize,
    pub(crate) label: SharedString,
}

impl SelectItem for GpuEngineOption {
    type Value = usize;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.index
    }
}

/// Renders the GPU (`GPU 0..N`) detail pane.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_gpu_detail(
    gpu: &GpuAdapterSlice,
    effective_count: usize,
    max_panes: usize,
    engine_indices: [usize; MAX_GPU_ENGINE_PANES],
    pane_count_select: &Entity<SelectState<Vec<GpuPaneCountOption>>>,
    engine_selects: &[Entity<SelectState<Vec<GpuEngineOption>>>; MAX_GPU_ENGINE_PANES],
    colors: &LoomSemanticColors,
    cx: &mut Context<PerformanceView>,
) -> impl IntoElement {
    let unavailable = i18n::tr("perf.unavailable");
    let title = i18n::perf_gpu_title(gpu.index);
    let name: SharedString = if gpu.name.trim().is_empty() {
        unavailable.clone().into()
    } else {
        SharedString::from(gpu.name.as_ref().to_owned())
    };

    let sleep_badge = gpu.d3_asleep.then(|| {
        div()
            .px_2()
            .py_0p5()
            .rounded_sm()
            .bg(colors.status_suspended_bg)
            .text_xs()
            .font_medium()
            .text_color(colors.status_suspended_fg)
            .child(i18n::tr("perf.perf_polish_gpu_sleeping"))
            .into_any_element()
    });

    let controls_bar = (max_panes > 0).then(|| {
        h_flex().w_full().items_center().child(
            div().w(px(148.)).child(
                Select::new(pane_count_select)
                    .small()
                    .accessibility_label(i18n::tr("perf.gpu_engine_count")),
            ),
        )
    });

    let engine_charts_area = div()
        .id(SharedString::from(format!("perf-gpu-engines-{}", gpu.luid)))
        .w_full()
        .child(if gpu.engines.is_empty() || effective_count == 0 {
            let points = history_ring_to_points(&gpu.util_history);
            let chart = build_percent_area_chart(
                SharedString::from(format!("perf-gpu-overall-{}", gpu.luid)),
                points,
                i18n::tr("perf.gpu_utilization"),
                colors.chart_gpu,
                true,
            );
            chart_frame(
                i18n::tr("perf.gpu_utilization"),
                "100%",
                Vec::new(),
                colors.chart_gpu,
                210.0,
                chart,
                colors,
            )
            .into_any_element()
        } else if effective_count == 1 {
            render_gpu_single_engine(gpu, engine_indices[0], &engine_selects[0], colors)
                .into_any_element()
        } else {
            render_gpu_engine_grid(gpu, effective_count, engine_indices, engine_selects, colors)
                .into_any_element()
        });

    // Dedicated & Shared VRAM 60s charts side by side.
    let ded_title = i18n::tr("perf.gpu_dedicated_memory");
    let shared_title = i18n::tr("perf.gpu_shared_memory");
    let ded_scale = if gpu.dedicated_total_bytes > 0 {
        format_bytes(gpu.dedicated_total_bytes)
    } else {
        "100%".to_owned()
    };
    let shared_scale = if gpu.shared_total_bytes > 0 {
        format_bytes(gpu.shared_total_bytes)
    } else {
        "100%".to_owned()
    };

    let ded_chart = build_percent_area_chart(
        SharedString::from(format!("perf-gpu-ded-mem-{}", gpu.luid)),
        history_ring_to_points(&gpu.dedicated_mem_history),
        ded_title.clone(),
        colors.chart_gpu,
        true,
    );
    let shared_chart = build_percent_area_chart(
        SharedString::from(format!("perf-gpu-shared-mem-{}", gpu.luid)),
        history_ring_to_points(&gpu.shared_mem_history),
        shared_title.clone(),
        colors.chart_gpu_shared,
        true,
    );

    let vram_row = h_flex()
        .w_full()
        .flex_wrap()
        .gap_3()
        .child(div().flex_1().min_w(px(240.)).child(chart_frame(
            ded_title.clone(),
            ded_scale,
            Vec::new(),
            colors.chart_gpu,
            110.0,
            ded_chart,
            colors,
        )))
        .child(div().flex_1().min_w(px(240.)).child(chart_frame(
            shared_title.clone(),
            shared_scale,
            Vec::new(),
            colors.chart_gpu_shared,
            110.0,
            shared_chart,
            colors,
        )));

    let ded_metric = format!(
        "{} / {}",
        format_bytes(gpu.dedicated_used_bytes),
        format_bytes(gpu.dedicated_total_bytes)
    );
    let shared_metric = format!(
        "{} / {}",
        format_bytes(gpu.shared_used_bytes),
        format_bytes(gpu.shared_total_bytes)
    );
    let util_metric = format_opt_percent_permille((!gpu.d3_asleep).then_some(gpu.util_permille));
    let temp_metric = format_opt_temp_celsius(gpu.temperature_celsius, gpu.d3_asleep);

    let opt_str = |opt: &Option<std::sync::Arc<str>>| {
        opt.as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| unavailable.clone())
    };

    let left_stats = vec![
        MetricStat::new(i18n::tr("perf.gpu_utilization"), util_metric),
        MetricStat::new(ded_title, ded_metric),
        MetricStat::new(shared_title, shared_metric),
        MetricStat::new(i18n::tr("perf.gpu_temperature"), temp_metric),
    ];

    let right_specs = vec![
        SpecRow::new(
            i18n::tr("perf.gpu_driver_version"),
            opt_str(&gpu.driver_version),
        ),
        SpecRow::new(
            i18n::tr("perf.gpu_directx_version"),
            opt_str(&gpu.directx_version),
        ),
        SpecRow::new(
            i18n::tr("perf.gpu_pci_location"),
            opt_str(&gpu.pci_location),
        ),
    ];

    let weak_ctx = cx.entity().downgrade();
    v_flex()
        .id(SharedString::from(format!("perf-gpu-detail-{}", gpu.luid)))
        .w_full()
        .gap_4()
        .child(device_header(title, name, sleep_badge, colors))
        .when_some(controls_bar, |this, bar| this.child(bar))
        .child(engine_charts_area)
        .child(vram_row)
        .child(metrics_two_column_panel(left_stats, right_specs, colors))
        .context_menu(move |mut menu, window, cx| {
            let weak_count = weak_ctx.clone();
            let weak_copy = weak_ctx.clone();
            if max_panes > 0 {
                menu = menu
                    .submenu(
                        i18n::tr("perf.gpu_engine_count"),
                        window,
                        cx,
                        move |mut sub, _window, _cx| {
                            for count in 1..=max_panes {
                                let weak_item = weak_count.clone();
                                sub = sub.item(
                                    PopupMenuItem::new(i18n::perf_gpu_engine_count_label(count))
                                        .checked(count == effective_count)
                                        .on_click(move |_, window, cx| {
                                            let _ = weak_item.update(cx, |view, cx| {
                                                view.set_gpu_engine_count(count, window, cx);
                                            });
                                        }),
                                );
                            }
                            sub
                        },
                    )
                    .separator();
            }
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

fn render_gpu_engine_grid(
    gpu: &GpuAdapterSlice,
    pane_count: usize,
    engine_indices: [usize; MAX_GPU_ENGINE_PANES],
    engine_selects: &[Entity<SelectState<Vec<GpuEngineOption>>>; MAX_GPU_ENGINE_PANES],
    colors: &LoomSemanticColors,
) -> impl IntoElement {
    let visible_count = pane_count.min(MAX_GPU_ENGINE_PANES);
    let chart_height = if visible_count <= 2 { 150.0 } else { 110.0 };
    let select_a11y = i18n::tr("perf.gpu_select_engine");

    let make_pane = |slot: usize| {
        let engine_ix = engine_indices[slot].min(gpu.engines.len().saturating_sub(1));
        let engine = &gpu.engines[engine_ix];
        let chart = build_percent_area_chart(
            SharedString::from(format!("perf-gpu-{}-pane-{slot}", gpu.luid)),
            history_ring_to_points(&engine.history),
            SharedString::from(engine.name.as_ref().to_owned()),
            colors.chart_gpu,
            true,
        );
        let util_label =
            format_opt_percent_permille((!gpu.d3_asleep).then_some(engine.util_permille));

        v_flex()
            .flex_1()
            .min_w_0()
            .gap_1()
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div().w(px(160.)).max_w_full().child(
                            Select::new(&engine_selects[slot])
                                .xsmall()
                                .accessibility_label(select_a11y.clone()),
                        ),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .font_semibold()
                            .text_color(colors.text_primary)
                            .child(util_label),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .h(px(chart_height))
                    .rounded_md()
                    .border_1()
                    .border_color(colors.chart_gpu.opacity(0.45))
                    .bg(colors.card_bg)
                    .overflow_hidden()
                    .child(chart),
            )
    };

    let mut grid = v_flex().w_full().gap_3();
    for row_start in (0..visible_count).step_by(2) {
        let mut row = h_flex().w_full().gap_3().child(make_pane(row_start));
        if row_start + 1 < visible_count {
            row = row.child(make_pane(row_start + 1));
        } else {
            // Pad incomplete trailing row so pane widths remain uniform.
            row = row.child(div().flex_1().min_w_0());
        }
        grid = grid.child(row);
    }
    grid
}

fn render_gpu_single_engine(
    gpu: &GpuAdapterSlice,
    single_index: usize,
    single_select: &Entity<SelectState<Vec<GpuEngineOption>>>,
    colors: &LoomSemanticColors,
) -> impl IntoElement {
    let engine_ix = single_index.min(gpu.engines.len().saturating_sub(1));
    let engine = &gpu.engines[engine_ix];
    let chart = build_percent_area_chart(
        SharedString::from(format!("perf-gpu-{}-single", gpu.luid)),
        history_ring_to_points(&engine.history),
        SharedString::from(engine.name.as_ref().to_owned()),
        colors.chart_gpu,
        true,
    );
    let util_label = format_opt_percent_permille((!gpu.d3_asleep).then_some(engine.util_permille));

    v_flex()
        .w_full()
        .gap_1p5()
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    div().w(px(200.)).child(
                        Select::new(single_select)
                            .small()
                            .accessibility_label(i18n::tr("perf.gpu_select_engine")),
                    ),
                )
                .child(
                    div()
                        .text_xs()
                        .font_semibold()
                        .text_color(colors.text_primary)
                        .child(format!("{util_label} · 100%")),
                ),
        )
        .child(
            div()
                .w_full()
                .h(px(210.))
                .rounded_md()
                .border_1()
                .border_color(colors.chart_gpu.opacity(0.45))
                .bg(colors.card_bg)
                .overflow_hidden()
                .child(chart),
        )
        .child(
            h_flex()
                .w_full()
                .justify_between()
                .text_xs()
                .text_color(colors.text_muted)
                .child(i18n::tr("perf.time_axis_start"))
                .child(i18n::tr("perf.time_axis_end")),
        )
}
