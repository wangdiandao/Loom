//! Unit and GPUI integration tests for the Startup Apps vertical feature slice.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{AppContext as _, TestAppContext, VisualTestContext};
use loom_config::{AppSettings, ConfigStore};
use loom_engine::EngineCoordinator;
use loom_pal::{NativeBackend, PlatformBackend};
use loom_types::{
    MonitorDomain, SortDirection, StartupEntry, StartupImpact, StartupSlice, StartupSortColumn,
};

use crate::bridge::UiStore;

use super::StartupView;
use super::state::{
    CoalesceDecision, ScrollCoalescer, compute_startup_viewport_fingerprint,
    format_bios_time_seconds,
};

fn sample_entry(
    id: &str,
    name: &str,
    publisher: Option<&str>,
    enabled: bool,
    impact: StartupImpact,
) -> StartupEntry {
    StartupEntry {
        id: Arc::from(id),
        name: Arc::from(name),
        publisher: publisher.map(Arc::from),
        command: None,
        file_path: None,
        icon: None,
        enabled,
        impact,
    }
}

#[test]
fn bios_time_formats_to_one_decimal_second() {
    assert_eq!(format_bios_time_seconds(0), "0.0");
    assert_eq!(format_bios_time_seconds(49), "0.0");
    assert_eq!(format_bios_time_seconds(50), "0.1");
    assert_eq!(format_bios_time_seconds(4_200), "4.2");
    assert_eq!(format_bios_time_seconds(4_250), "4.3");
    assert_eq!(format_bios_time_seconds(12_940), "12.9");
    assert_eq!(format_bios_time_seconds(12_950), "13.0");
}

#[test]
fn startup_viewport_fingerprint_detects_visible_changes_and_ignores_offscreen() {
    let mut slice = StartupSlice {
        revision: 1,
        sort_column: StartupSortColumn::Name,
        sort_direction: SortDirection::Ascending,
        items: vec![
            sample_entry(
                "id:1",
                "Discord",
                Some("Discord Inc."),
                false,
                StartupImpact::Low,
            ),
            sample_entry(
                "id:2",
                "OneDrive",
                Some("Microsoft"),
                true,
                StartupImpact::High,
            ),
            sample_entry(
                "id:3",
                "Security",
                Some("Microsoft"),
                true,
                StartupImpact::NotMeasured,
            ),
        ],
        visible_indices: vec![0, 1, 2],
        ..StartupSlice::default()
    };

    let fp_initial = compute_startup_viewport_fingerprint(&slice, &(0..2), Some(0));

    // Off-screen row toggle (index 2) keeps visible fingerprint identical.
    slice.items[2].enabled = false;
    assert_eq!(
        fp_initial,
        compute_startup_viewport_fingerprint(&slice, &(0..2), Some(0))
    );

    // Visible row toggle (index 0) updates the fingerprint.
    slice.items[0].enabled = true;
    assert_ne!(
        fp_initial,
        compute_startup_viewport_fingerprint(&slice, &(0..2), Some(0))
    );
}

#[test]
fn startup_scroll_coalescer_enforces_quiet_window_cap_and_bypass() {
    let mut coalescer = ScrollCoalescer::default();
    let t0 = Instant::now();
    assert_eq!(
        coalescer.decide(t0 + Duration::from_millis(50), Some(t0), false),
        CoalesceDecision::Defer(Duration::from_millis(100))
    );
    assert_eq!(
        coalescer.decide(
            t0 + Duration::from_millis(550),
            Some(t0 + Duration::from_millis(540)),
            false
        ),
        CoalesceDecision::ApplyNow
    );
    assert_eq!(
        coalescer.decide(t0 + Duration::from_millis(10), Some(t0), true),
        CoalesceDecision::ApplyNow
    );
}

#[gpui_kit::test]
fn startup_view_acquires_lease_dispatches_sort_and_releases_on_drop(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let dir = tempfile::tempdir().expect("tempdir");
    let config_store = ConfigStore::new(dir.path().join("settings.toml"));
    let settings = AppSettings::default();
    let backend = NativeBackend::initialize().expect("initialize native backend");
    let coordinator =
        Arc::new(EngineCoordinator::new(backend, settings.clone()).expect("start coordinator"));

    assert!(!coordinator.leases().is_active(MonitorDomain::Startup));

    let store = cx.new(|cx| UiStore::new(coordinator.clone(), config_store, settings, cx));

    let mut view_handle = None;
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |window, cx| {
            let view = cx.new(|cx| StartupView::new(store.clone(), window, cx));
            view_handle = Some(view.clone());
            cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
        })
        .expect("open startup test window")
    });
    let view = view_handle.expect("StartupView constructed");

    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    vcx.update(|window, cx| window.draw(cx).clear(cx));

    assert!(
        coordinator.leases().is_active(MonitorDomain::Startup),
        "mounting StartupView must acquire MonitorDomain::Startup lease"
    );

    view.update(&mut vcx, |view, cx| {
        view.table.update(cx, |table, cx| {
            table.delegate_mut().trigger_sort(3, cx);
        });
    });
    vcx.run_until_parked();

    drop(view);
    vcx.update(|window, _| window.remove_window());
    vcx.run_until_parked();

    assert!(
        !coordinator.leases().is_active(MonitorDomain::Startup),
        "dropping StartupView must release MonitorDomain::Startup lease"
    );

    coordinator.shutdown();
}
