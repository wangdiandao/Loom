//! Unit and GPUI integration tests for the App History vertical feature slice.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{AppContext as _, TestAppContext, VisualTestContext};
use loom_config::{AppSettings, ConfigStore};
use loom_engine::EngineCoordinator;
use loom_pal::{NativeBackend, PlatformBackend};
use loom_types::{
    AppHistoryRecord, AppHistorySlice, AppHistorySortColumn, MonitorDomain, SortDirection,
};

use crate::bridge::UiStore;

use super::AppHistoryView;
use super::state::{
    CoalesceDecision, ScrollCoalescer, compute_app_history_viewport_fingerprint,
    format_epoch_datetime,
};

fn sample_record(
    app_id: &str,
    name: &str,
    cpu_secs: u64,
    net_bytes: u64,
    metered_bytes: u64,
) -> AppHistoryRecord {
    AppHistoryRecord {
        app_id: Arc::from(app_id),
        name: Arc::from(name),
        cpu_time_100ns: cpu_secs * 10_000_000,
        network_bytes: net_bytes,
        metered_network_bytes: metered_bytes,
    }
}

#[test]
fn format_epoch_datetime_formats_standard_and_leap_dates() {
    assert_eq!(format_epoch_datetime(0), "1970-01-01 00:00");
    // 2024-02-29 12:34:00 UTC
    assert_eq!(format_epoch_datetime(1_709_210_040), "2024-02-29 12:34");
    // 2026-11-27 06:58:30 UTC
    assert_eq!(format_epoch_datetime(1_795_762_710), "2026-11-27 06:58");
}

#[test]
fn app_history_viewport_fingerprint_quantizes_metrics_and_ignores_offscreen() {
    let mut slice = AppHistorySlice {
        revision: 1,
        sort_column: AppHistorySortColumn::CpuTime,
        sort_direction: SortDirection::Descending,
        items: vec![
            sample_record("app.alpha", "Alpha", 120, 10 * 1024 * 1024, 0),
            sample_record("app.beta", "Beta", 3_600, 500_000, 10_000),
            sample_record("app.gamma", "Gamma", 30, 2_000_000, 0),
        ],
        visible_indices: vec![1, 0, 2],
        ..AppHistorySlice::default()
    };

    let fp_initial = compute_app_history_viewport_fingerprint(&slice, &(0..2), Some(0));

    // Sub-second CPU tick delta stays in the same whole-second bucket.
    slice.items[1].cpu_time_100ns += 5_000_000;
    assert_eq!(
        fp_initial,
        compute_app_history_viewport_fingerprint(&slice, &(0..2), Some(0))
    );

    // Off-screen row update (visible index 2 -> item 2) does not invalidate fingerprint.
    slice.items[2].network_bytes += 100 * 1024 * 1024;
    assert_eq!(
        fp_initial,
        compute_app_history_viewport_fingerprint(&slice, &(0..2), Some(0))
    );

    // Whole-second visible CPU delta invalidates the fingerprint.
    slice.items[1].cpu_time_100ns += 10_000_000;
    assert_ne!(
        fp_initial,
        compute_app_history_viewport_fingerprint(&slice, &(0..2), Some(0))
    );
}

#[test]
fn app_history_scroll_coalescer_enforces_quiet_window_cap_and_bypass() {
    let mut coalescer = ScrollCoalescer::default();
    let t0 = Instant::now();
    assert_eq!(
        coalescer.decide(t0 + Duration::from_millis(30), Some(t0), false),
        CoalesceDecision::Defer(Duration::from_millis(120))
    );
    assert_eq!(
        coalescer.decide(t0 + Duration::from_millis(180), Some(t0), false),
        CoalesceDecision::ApplyNow
    );
    assert_eq!(
        coalescer.decide(t0 + Duration::from_millis(10), Some(t0), true),
        CoalesceDecision::ApplyNow
    );
}

#[gpui_kit::test]
fn app_history_view_acquires_lease_dispatches_sort_and_releases_on_drop(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let dir = tempfile::tempdir().expect("tempdir");
    let config_store = ConfigStore::new(dir.path().join("settings.toml"));
    let settings = AppSettings::default();
    let backend = NativeBackend::initialize().expect("initialize native backend");
    let coordinator =
        Arc::new(EngineCoordinator::new(backend, settings.clone()).expect("start coordinator"));

    assert!(!coordinator.leases().is_active(MonitorDomain::AppHistory));

    let store = cx.new(|cx| UiStore::new(coordinator.clone(), config_store, settings, cx));

    let mut view_handle = None;
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |window, cx| {
            let view = cx.new(|cx| AppHistoryView::new(store.clone(), window, cx));
            view_handle = Some(view.clone());
            cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
        })
        .expect("open app history test window")
    });
    let view = view_handle.expect("AppHistoryView constructed");

    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    vcx.update(|window, cx| window.draw(cx).clear(cx));

    assert!(
        coordinator.leases().is_active(MonitorDomain::AppHistory),
        "mounting AppHistoryView must acquire MonitorDomain::AppHistory lease"
    );

    view.update(&mut vcx, |view, cx| {
        view.table.update(cx, |table, cx| {
            table.delegate_mut().trigger_sort(2, cx);
        });
    });
    vcx.run_until_parked();

    drop(view);
    vcx.update(|window, _| window.remove_window());
    vcx.run_until_parked();

    assert!(
        !coordinator.leases().is_active(MonitorDomain::AppHistory),
        "dropping AppHistoryView must release MonitorDomain::AppHistory lease"
    );

    coordinator.shutdown();
}
