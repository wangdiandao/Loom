//! Unit and GPUI integration tests for the Performance vertical feature slice.

use std::sync::Arc;

use gpui_kit::component::{ActiveTheme as _, IndexPath};
use gpui_kit::{AppContext as _, TestAppContext, VisualTestContext};
use loom_config::{AppSettings, ConfigStore};
use loom_engine::EngineCoordinator;
use loom_pal::{NativeBackend, PlatformBackend};
use loom_types::{
    CpuCoreKind, CpuStaticInfo, GpuAdapterSlice, GpuEngineSlice, HistoryRing, LanguagePreference,
    MonitorDomain, PerformanceSlice,
};
use loom_ui_kit::{LoomSemanticColors, compute_core_grid_layout};

use crate::bridge::UiStore;
use crate::i18n;

use super::cache::PerformanceSeriesCache;
use super::sidebar::format_gpu_sidebar_metric;
use super::state::{CpuGraphMode, MAX_GPU_ENGINE_PANES, SelectedDevice};
use super::{PerformanceView, cpu, gpu};

#[test]
fn performance_series_cache_reuses_series_within_revision_and_invalidates_on_advance() {
    let mut perf = PerformanceSlice::default();
    perf.cpu_history.push(350);
    perf.mem_history.push(500);

    let mut cache = PerformanceSeriesCache::default();
    cache.sync(1, &perf);
    assert_eq!(cache.sidebar_rebuilds(), 1);
    assert_eq!(cache.cpu_sparkline().len(), 60);

    // Unrelated redraw at the same revision reuses cached sidebar & detail series.
    let _ = cache.cpu_overall(&perf);
    assert_eq!(cache.detail_builds(), 1);

    cache.sync(1, &perf);
    let _ = cache.cpu_overall(&perf);
    assert_eq!(cache.sidebar_rebuilds(), 1);
    assert_eq!(cache.detail_builds(), 1);

    // Advancing revision invalidates and rebuilds once.
    perf.cpu_history.push(420);
    cache.sync(2, &perf);
    assert_eq!(cache.sidebar_rebuilds(), 2);
    let latest_val = cache.cpu_overall(&perf)[59].value;
    assert_eq!(cache.detail_builds(), 2);
    assert_eq!(latest_val, Some(42.0));
}

#[gpui_kit::test]
fn performance_view_acquires_and_releases_performance_and_per_core_leases(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);

    let dir = tempfile::tempdir().expect("create temp config dir");
    let config_store = ConfigStore::new(dir.path().join("settings.toml"));
    let settings = AppSettings::default();
    let backend = NativeBackend::initialize().expect("initialize native backend");
    let coordinator =
        Arc::new(EngineCoordinator::new(backend, settings.clone()).expect("start coordinator"));

    assert!(!coordinator.leases().is_active(MonitorDomain::Performance));
    assert!(!coordinator.leases().is_active(MonitorDomain::PerCoreCpu));

    let store = cx.new(|cx| UiStore::new(coordinator.clone(), config_store, settings, cx));

    let mut perf_view_handle = None;
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |window, cx| {
            let view = cx.new(|cx| PerformanceView::new(store.clone(), window, cx));
            perf_view_handle = Some(view.clone());
            cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
        })
        .expect("open performance test window")
    });
    let perf_view = perf_view_handle.expect("PerformanceView constructed");

    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    vcx.update(|window, cx| window.draw(cx).clear(cx));

    // Mount acquires Performance lease, NOT PerCoreCpu.
    assert!(coordinator.leases().is_active(MonitorDomain::Performance));
    assert!(!coordinator.leases().is_active(MonitorDomain::PerCoreCpu));

    // Toggling Logical Processors acquires PerCoreCpu lease dynamically.
    vcx.update(|_window, cx| {
        perf_view.update(cx, |view, cx| {
            view.set_cpu_graph_mode(CpuGraphMode::LogicalProcessors, cx);
        });
    });
    assert!(coordinator.leases().is_active(MonitorDomain::Performance));
    assert!(coordinator.leases().is_active(MonitorDomain::PerCoreCpu));

    // Switching away from CPU to Memory drops PerCoreCpu lease while keeping Performance.
    vcx.update(|window, cx| {
        perf_view.update(cx, |view, cx| {
            view.select_device(SelectedDevice::Memory, window, cx);
        });
    });
    assert!(coordinator.leases().is_active(MonitorDomain::Performance));
    assert!(
        !coordinator.leases().is_active(MonitorDomain::PerCoreCpu),
        "PerCoreCpu lease must drop when navigating away from CPU"
    );

    // Switching back to CPU while still in LogicalProcessors mode re-acquires PerCoreCpu.
    vcx.update(|window, cx| {
        perf_view.update(cx, |view, cx| {
            view.select_device(SelectedDevice::Cpu, window, cx);
        });
    });
    assert!(coordinator.leases().is_active(MonitorDomain::PerCoreCpu));

    // Switching back to Overall drops PerCoreCpu lease immediately while keeping Performance.
    vcx.update(|_window, cx| {
        perf_view.update(cx, |view, cx| {
            view.set_cpu_graph_mode(CpuGraphMode::Overall, cx);
        });
    });
    assert!(coordinator.leases().is_active(MonitorDomain::Performance));
    assert!(!coordinator.leases().is_active(MonitorDomain::PerCoreCpu));

    // Re-acquire PerCoreCpu, then close window & drop handles -> all leases released.
    vcx.update(|_window, cx| {
        perf_view.update(cx, |view, cx| {
            view.set_cpu_graph_mode(CpuGraphMode::LogicalProcessors, cx);
        });
    });
    assert!(coordinator.leases().is_active(MonitorDomain::PerCoreCpu));

    drop(perf_view);
    vcx.update(|window, _| window.remove_window());
    vcx.run_until_parked();

    assert!(
        !coordinator.leases().is_active(MonitorDomain::Performance),
        "Performance lease must be released when PerformanceView drops"
    );
    assert!(
        !coordinator.leases().is_active(MonitorDomain::PerCoreCpu),
        "PerCoreCpu lease must be released when PerformanceView drops"
    );

    coordinator.shutdown();
}

#[gpui_kit::test]
fn performance_view_copy_summary_writes_clipboard_and_renders_hybrid_and_d3_gpu(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    i18n::apply_locale(LanguagePreference::EnUs);

    let dir = tempfile::tempdir().expect("create temp config dir");
    let config_store = ConfigStore::new(dir.path().join("settings.toml"));
    let settings = AppSettings::default();
    let backend = NativeBackend::initialize().expect("initialize native backend");
    let coordinator =
        Arc::new(EngineCoordinator::new(backend, settings.clone()).expect("start coordinator"));

    let store = cx.new(|cx| UiStore::new(coordinator.clone(), config_store, settings, cx));

    let mut perf_view_handle = None;
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |window, cx| {
            let view = cx.new(|cx| PerformanceView::new(store.clone(), window, cx));
            perf_view_handle = Some(view.clone());
            cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
        })
        .expect("open performance test window")
    });
    let perf_view = perf_view_handle.expect("PerformanceView constructed");

    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    vcx.update(|window, cx| window.draw(cx).clear(cx));

    // Verify Copy summary copies CPU summary to clipboard cleanly.
    vcx.update(|window, cx| {
        i18n::apply_locale(LanguagePreference::EnUs);
        perf_view.update(cx, |view, cx| {
            view.copy_selected_summary(window, cx);
        });
    });
    let copied_cpu = vcx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("clipboard should contain CPU summary");
    assert!(copied_cpu.contains("CPU"));
    assert!(
        copied_cpu.contains("Utilization:") || copied_cpu.contains("利用率:"),
        "unexpected CPU summary: {copied_cpu}"
    );

    // Switch to Memory and verify Copy summary updates clipboard.
    vcx.update(|window, cx| {
        i18n::apply_locale(LanguagePreference::EnUs);
        perf_view.update(cx, |view, cx| {
            view.select_device(SelectedDevice::Memory, window, cx);
            view.copy_selected_summary(window, cx);
        });
    });
    let copied_mem = vcx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("clipboard should contain Memory summary");
    assert!(
        copied_mem.contains("Memory") || copied_mem.contains("内存"),
        "unexpected Memory summary: {copied_mem}"
    );
    assert!(
        copied_mem.contains("In use:") || copied_mem.contains("使用中:"),
        "unexpected Memory summary: {copied_mem}"
    );

    // Exercise rendering synthetic hybrid P/E/LP-E logical core grid and sleeping D3 GPU pane.
    vcx.update(|_window, cx| {
        let colors = LoomSemanticColors::from_theme(cx.theme());

        let mut hybrid_perf = PerformanceSlice {
            cpu_info: CpuStaticInfo {
                brand_name: Arc::from("Hybrid Test CPU"),
                core_kinds: vec![
                    CpuCoreKind::Performance,
                    CpuCoreKind::Performance,
                    CpuCoreKind::Efficiency,
                    CpuCoreKind::LowPowerEfficiency,
                ],
                ..Default::default()
            },
            per_core_permille: vec![420, 180, 95, HistoryRing::<60>::GAP_SENTINEL],
            per_core_history: vec![
                {
                    let mut r = HistoryRing::<60>::new();
                    r.push(420);
                    r.push_gap();
                    r.push(350);
                    r
                },
                HistoryRing::<60>::new(),
                HistoryRing::<60>::new(),
                HistoryRing::<60>::new(),
            ],
            ..Default::default()
        };
        let mut cache = PerformanceSeriesCache::default();
        cache.sync(1, &hybrid_perf);
        let layout = compute_core_grid_layout(4, 900.0, 500.0, None);
        assert!(layout.columns >= 1);
        let grid_el = cpu::render_logical_cpu_grid(&hybrid_perf, layout, &mut cache, &colors);
        drop(grid_el);

        let sleeping_gpu = GpuAdapterSlice {
            index: 1,
            luid: 9999,
            name: Arc::from("Discrete RTX Test GPU"),
            temperature_celsius: Some(0),
            util_permille: 0,
            d3_asleep: true,
            engines: vec![GpuEngineSlice {
                name: Arc::from("3D"),
                util_permille: 0,
                history: HistoryRing::<60>::new(),
            }],
            ..Default::default()
        };
        let sleep_label = format_gpu_sidebar_metric(&sleeping_gpu);
        assert!(
            sleep_label.as_ref() == "Sleeping (D3)" || sleep_label.as_ref() == "休眠 (D3)",
            "unexpected D3 sleep label: {sleep_label}"
        );
        hybrid_perf.gpus.push(sleeping_gpu);

        perf_view.update(cx, |view, cx| {
            for count in [1usize, 3, 4, 8] {
                let _gpu_el = gpu::render_gpu_detail(
                    &hybrid_perf.gpus[0],
                    count.min(hybrid_perf.gpus[0].engines.len()),
                    hybrid_perf.gpus[0].engines.len().min(MAX_GPU_ENGINE_PANES),
                    [0; MAX_GPU_ENGINE_PANES],
                    &view.gpu_pane_count_select,
                    &view.gpu_engine_selects,
                    &mut view.series_cache,
                    &colors,
                    cx,
                );
            }
        });
    });

    drop(perf_view);
    vcx.update(|window, _| window.remove_window());
    vcx.run_until_parked();
    coordinator.shutdown();
}

#[gpui_kit::test]
fn gpu_engine_count_dropdown_syncs_caps_and_relocalizes(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);

    let dir = tempfile::tempdir().expect("create temp config dir");
    let config_store = ConfigStore::new(dir.path().join("settings.toml"));
    let settings = AppSettings {
        language: LanguagePreference::EnUs,
        ..Default::default()
    };
    let backend = NativeBackend::initialize().expect("initialize native backend");
    let coordinator =
        Arc::new(EngineCoordinator::new(backend, settings.clone()).expect("start coordinator"));

    let store = cx.new(|cx| UiStore::new(coordinator.clone(), config_store, settings, cx));

    let mut perf_view_handle = None;
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |window, cx| {
            let view = cx.new(|cx| PerformanceView::new(store.clone(), window, cx));
            perf_view_handle = Some(view.clone());
            cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
        })
        .expect("open performance test window")
    });
    let perf_view = perf_view_handle.expect("PerformanceView constructed");
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    vcx.update(|window, cx| window.draw(cx).clear(cx));

    let make_engine = |name: &str| GpuEngineSlice {
        name: Arc::from(name),
        util_permille: 250,
        history: HistoryRing::<60>::new(),
    };
    let ten_engine_gpu = GpuAdapterSlice {
        index: 0,
        luid: 4242,
        name: Arc::from("Test 10-Engine GPU"),
        engines: vec![
            make_engine("3D"),
            make_engine("Copy"),
            make_engine("Copy 1"),
            make_engine("Video Decode"),
            make_engine("Video Encode"),
            make_engine("Video Processing"),
            make_engine("Compute_0"),
            make_engine("Compute_1"),
            make_engine("High Priority 3D"),
            make_engine("VR"),
        ],
        ..Default::default()
    };
    let two_engine_gpu = GpuAdapterSlice {
        index: 1,
        luid: 5252,
        name: Arc::from("Test 2-Engine GPU"),
        engines: vec![make_engine("3D"), make_engine("Copy")],
        ..Default::default()
    };
    let perf = PerformanceSlice {
        gpus: vec![ten_engine_gpu, two_engine_gpu],
        ..Default::default()
    };

    vcx.update(|window, cx| {
        let colors = LoomSemanticColors::from_theme(cx.theme());

        // Selecting a 10-engine GPU caps dropdown options to 1..=MAX_GPU_ENGINE_PANES (8), defaulting to 4.
        perf_view.update(cx, |view, cx| {
            view.state.selected_device = SelectedDevice::Gpu(4242);
            view.sync_gpu_selects(&perf, window, cx);
            assert_eq!(view.synced_gpu_max_panes, MAX_GPU_ENGINE_PANES);
            assert_eq!(view.synced_gpu_effective_count, 4);
            let count_select = view.gpu_pane_count_select.read(cx);
            assert_eq!(count_select.selected_value(), Some(&4));
            assert_eq!(
                count_select.selected_index(cx),
                Some(IndexPath::default().row(3))
            );
            assert_eq!(view.synced_gpu_count_locale, Some(LanguagePreference::EnUs));

            // Changing count to 1 and 8 updates state & renders both layouts cleanly.
            view.set_gpu_engine_count(1, window, cx);
            view.sync_gpu_selects(&perf, window, cx);
            assert_eq!(
                view.gpu_pane_count_select.read(cx).selected_value(),
                Some(&1)
            );
            drop(gpu::render_gpu_detail(
                &perf.gpus[0],
                1,
                MAX_GPU_ENGINE_PANES,
                view.synced_gpu_engine_indices,
                &view.gpu_pane_count_select,
                &view.gpu_engine_selects,
                &mut view.series_cache,
                &colors,
                cx,
            ));

            view.set_gpu_engine_count(8, window, cx);
            view.sync_gpu_selects(&perf, window, cx);
            assert_eq!(
                view.gpu_pane_count_select.read(cx).selected_value(),
                Some(&8)
            );
            drop(gpu::render_gpu_detail(
                &perf.gpus[0],
                8,
                MAX_GPU_ENGINE_PANES,
                view.synced_gpu_engine_indices,
                &view.gpu_pane_count_select,
                &view.gpu_engine_selects,
                &mut view.series_cache,
                &colors,
                cx,
            ));

            // Switching to a 2-engine GPU clamps effective count to 2 without losing user's desired 8.
            view.state.selected_device = SelectedDevice::Gpu(5252);
            view.sync_gpu_selects(&perf, window, cx);
            assert_eq!(view.synced_gpu_max_panes, 2);
            assert_eq!(view.synced_gpu_effective_count, 2);
            assert_eq!(view.state.gpu_engine_count, 8);
            assert_eq!(
                view.gpu_pane_count_select.read(cx).selected_value(),
                Some(&2)
            );

            // Re-localizing updates dropdown state cleanly and preserves selection.
            view.last_locale = LanguagePreference::ZhCn;
            view.sync_gpu_selects(&perf, window, cx);
            assert_eq!(view.synced_gpu_count_locale, Some(LanguagePreference::ZhCn));
            assert_eq!(
                view.gpu_pane_count_select.read(cx).selected_value(),
                Some(&2)
            );
        });
    });

    drop(perf_view);
    vcx.update(|window, _| window.remove_window());
    vcx.run_until_parked();
    coordinator.shutdown();
}
