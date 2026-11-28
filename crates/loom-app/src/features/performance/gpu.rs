//! GPU detail pane for the Performance view (`2×2` multi-engine / single-engine, VRAM charts, honest D3 sleep & specs).

use gpui_kit::component::button::{Button, ButtonVariants as _};
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
use crate::features::performance::state::GpuViewMode;
use crate::i18n;

/// Selectable GPU execution engine option backing each [`SelectState`].
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
    view_mode: GpuViewMode,
    grid_indices: [usize; 4],
    single_index: usize,
    grid_selects: &[Entity<SelectState<Vec<GpuEngineOption>>>; 4],
    single_select: &Entity<SelectState<Vec<GpuEngineOption>>>,
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

    let grid_btn = Button::new("perf-gpu-mode-grid")
        .small()
        .label(i18n::tr("perf.gpu_view_grid"))
        .map(|btn| {
            if view_mode == GpuViewMode::Grid2x2 {
                btn.primary()
            } else {
                btn.outline()
            }
        })
        .on_click(cx.listener(|view, _, _window, cx| {
            view.set_gpu_view_mode(GpuViewMode::Grid2x2, cx);
        }));

    let single_btn = Button::new("perf-gpu-mode-single")
        .small()
        .label(i18n::tr("perf.gpu_view_single"))
        .map(|btn| {
            if view_mode == GpuViewMode::SingleEngine {
                btn.primary()
            } else {
                btn.outline()
            }
        })
        .on_click(cx.listener(|view, _, _window, cx| {
            view.set_gpu_view_mode(GpuViewMode::SingleEngine, cx);
        }));

    let controls_bar = h_flex()
        .w_full()
        .items_center()
        .gap_1p5()
        .child(grid_btn)
        .child(single_btn);

    let engine_charts_area = div()
        .id(SharedString::from(format!("perf-gpu-engines-{}", gpu.luid)))
        .w_full()
        .child(if gpu.engines.is_empty() {
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
        } else {
            match view_mode {
                GpuViewMode::Grid2x2 => {
                    render_gpu_2x2_grid(gpu, grid_indices, grid_selects, colors).into_any_element()
                }
                GpuViewMode::SingleEngine => {
                    render_gpu_single_engine(gpu, single_index, single_select, colors)
                        .into_any_element()
                }
            }
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
        .child(controls_bar)
        .child(engine_charts_area)
        .child(vram_row)
        .child(metrics_two_column_panel(left_stats, right_specs, colors))
        .context_menu(move |menu, _window, _cx| {
            let weak_grid = weak_ctx.clone();
            let weak_single = weak_ctx.clone();
            let weak_copy = weak_ctx.clone();
            menu.item(
                PopupMenuItem::new(i18n::tr("perf.gpu_view_grid"))
                    .checked(view_mode == GpuViewMode::Grid2x2)
                    .on_click(move |_, _, cx| {
                        let _ = weak_grid.update(cx, |view, cx| {
                            view.set_gpu_view_mode(GpuViewMode::Grid2x2, cx);
                        });
                    }),
            )
            .item(
                PopupMenuItem::new(i18n::tr("perf.gpu_view_single"))
                    .checked(view_mode == GpuViewMode::SingleEngine)
                    .on_click(move |_, _, cx| {
                        let _ = weak_single.update(cx, |view, cx| {
                            view.set_gpu_view_mode(GpuViewMode::SingleEngine, cx);
                        });
                    }),
            )
            .separator()
            .item(
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

fn render_gpu_2x2_grid(
    gpu: &GpuAdapterSlice,
    grid_indices: [usize; 4],
    grid_selects: &[Entity<SelectState<Vec<GpuEngineOption>>>; 4],
    colors: &LoomSemanticColors,
) -> impl IntoElement {
    let select_a11y = i18n::tr("perf.gpu_select_engine");
    let make_pane = |slot: usize| {
        let engine_ix = grid_indices[slot].min(gpu.engines.len().saturating_sub(1));
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
                            Select::new(&grid_selects[slot])
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
                    .h(px(110.))
                    .rounded_md()
                    .border_1()
                    .border_color(colors.chart_gpu.opacity(0.45))
                    .bg(colors.card_bg)
                    .overflow_hidden()
                    .child(chart),
            )
    };

    v_flex()
        .w_full()
        .gap_3()
        .child(
            h_flex()
                .w_full()
                .gap_3()
                .child(make_pane(0))
                .child(make_pane(1)),
        )
        .child(
            h_flex()
                .w_full()
                .gap_3()
                .child(make_pane(2))
                .child(make_pane(3)),
        )
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
