//! Unit and GPUI integration tests for the Services vertical feature slice.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{AppContext as _, TestAppContext, VisualTestContext};
use loom_config::{AppSettings, ConfigStore};
use loom_engine::EngineCoordinator;
use loom_pal::{NativeBackend, PlatformBackend};
use loom_types::{
    AppSnapshot, MonitorDomain, ProcessCategory, ProcessKey, ProcessRowKind, ProcessRowProjection,
    ProcessStatusFlags, ServiceRecord, ServiceState, ServicesSlice, ServicesSortColumn,
    SortDirection,
};
use loom_ui_kit::UNAVAILABLE_LABEL;

use crate::bridge::UiStore;
use crate::shell::ShellHandle;

use super::ServicesView;
use super::state::{
    CoalesceDecision, ScrollCoalescer, compute_services_viewport_fingerprint,
    resolve_process_key_by_pid, service_description_shared,
};

fn sample_service(
    name: &str,
    display_name: &str,
    description: Option<&str>,
    pid: Option<u32>,
    state: ServiceState,
    group: Option<&str>,
) -> ServiceRecord {
    ServiceRecord {
        name: Arc::from(name),
        display_name: Arc::from(display_name),
        description: description.map(Arc::from),
        pid,
        state,
        group: group.map(Arc::from),
    }
}

#[test]
fn service_description_shared_reuses_trimmed_arc_and_falls_back() {
    let with_desc = sample_service(
        "Spooler",
        "Print Spooler",
        Some("Loads files to memory"),
        Some(2040),
        ServiceState::Running,
        None,
    );
    assert_eq!(
        service_description_shared(&with_desc).as_ref(),
        "Loads files to memory"
    );

    let padded_desc = sample_service(
        "Spooler",
        "Print Spooler",
        Some("  Trimmed description  "),
        None,
        ServiceState::Stopped,
        None,
    );
    assert_eq!(
        service_description_shared(&padded_desc).as_ref(),
        "Trimmed description"
    );

    let fallback_display = sample_service(
        "Dhcp",
        "DHCP Client",
        Some("   "),
        Some(1200),
        ServiceState::Running,
        None,
    );
    assert_eq!(
        service_description_shared(&fallback_display).as_ref(),
        "DHCP Client"
    );

    let empty = sample_service("Empty", "   ", None, None, ServiceState::Unknown, None);
    assert_eq!(
        service_description_shared(&empty).as_ref(),
        UNAVAILABLE_LABEL
    );
}

#[test]
fn services_viewport_fingerprint_detects_visible_changes_and_ignores_offscreen_edits() {
    let mut slice = ServicesSlice {
        revision: 1,
        sort_column: ServicesSortColumn::Name,
        sort_direction: SortDirection::Ascending,
        filter_pid: None,
        items: vec![
            sample_service(
                "Alpha",
                "Alpha",
                None,
                Some(100),
                ServiceState::Running,
                None,
            ),
            sample_service("Beta", "Beta", None, Some(200), ServiceState::Stopped, None),
            sample_service(
                "Gamma",
                "Gamma",
                None,
                Some(300),
                ServiceState::Running,
                None,
            ),
        ],
        visible_indices: vec![0, 1, 2],
        ..ServicesSlice::default()
    };

    let fp_initial = compute_services_viewport_fingerprint(&slice, &(0..2), Some(0));

    // Mutating an off-screen row (index 2) keeps the visible fingerprint identical.
    slice.items[2].state = ServiceState::Stopped;
    let fp_offscreen = compute_services_viewport_fingerprint(&slice, &(0..2), Some(0));
    assert_eq!(fp_initial, fp_offscreen);

    // Mutating a visible row (index 1) changes the fingerprint.
    slice.items[1].state = ServiceState::Running;
    let fp_visible = compute_services_viewport_fingerprint(&slice, &(0..2), Some(0));
    assert_ne!(fp_initial, fp_visible);

    // Changing sort direction changes the fingerprint even if visible rows match.
    let fp_before_sort = fp_visible;
    slice.sort_direction = SortDirection::Descending;
    let fp_after_sort = compute_services_viewport_fingerprint(&slice, &(0..2), Some(0));
    assert_ne!(fp_before_sort, fp_after_sort);
}

#[test]
fn services_scroll_coalescer_enforces_quiet_window_max_defer_and_bypass() {
    let mut coalescer = ScrollCoalescer::default();
    let t0 = Instant::now();

    // No scroll history -> apply immediately.
    assert_eq!(
        coalescer.decide(t0, None, false),
        CoalesceDecision::ApplyNow
    );

    // Active scroll (40 ms ago) -> defer by remaining 110 ms quiet window.
    let t1 = t0 + Duration::from_millis(40);
    assert_eq!(
        coalescer.decide(t1, Some(t0), false),
        CoalesceDecision::Defer(Duration::from_millis(110))
    );

    // User-initiated bypass applies immediately and clears deferred timer.
    assert_eq!(
        coalescer.decide(t1, Some(t0), true),
        CoalesceDecision::ApplyNow
    );

    // Continuous scroll hits the 500 ms max defer cap.
    let start = t0 + Duration::from_secs(1);
    assert!(matches!(
        coalescer.decide(start, Some(start), false),
        CoalesceDecision::Defer(_)
    ));
    let near_cap = start + Duration::from_millis(450);
    assert_eq!(
        coalescer.decide(near_cap, Some(near_cap), false),
        CoalesceDecision::Defer(Duration::from_millis(50))
    );
    let at_cap = start + Duration::from_millis(500);
    assert_eq!(
        coalescer.decide(at_cap, Some(at_cap), false),
        CoalesceDecision::ApplyNow
    );
}

#[test]
fn resolve_process_key_by_pid_finds_live_process_row() {
    let mut snapshot = AppSnapshot::default();
    let target_key = ProcessKey::new(4321, 99_000);
    Arc::make_mut(&mut snapshot.processes)
        .rows
        .push(ProcessRowProjection {
            kind: ProcessRowKind::Process,
            key: target_key,
            depth: 0,
            has_children: false,
            is_expanded: false,
            child_count: 0,
            name: Arc::from("svchost.exe"),
            display_name: Arc::from("Host Process for Windows Services"),
            window_title: None,
            publisher: None,
            exe_path: None,
            command_line: None,
            icon: None,
            category: ProcessCategory::System,
            status: ProcessStatusFlags::empty(),
            cpu_permille: 0,
            memory_bytes: 0,
            memory_permille: 0,
            disk_bytes_per_sec: 0,
            disk_permille: 0,
            net_bytes_per_sec: 0,
            net_permille: 0,
            gpu_util_permille: 0,
            gpu_dedicated_bytes: 0,
            parent_key: None,
            leak_warning: None,
            webview2_role: None,
        });

    assert_eq!(
        resolve_process_key_by_pid(&snapshot, 4321),
        Some(target_key)
    );
    assert_eq!(resolve_process_key_by_pid(&snapshot, 0), None);
    assert_eq!(resolve_process_key_by_pid(&snapshot, 9999), None);
}

#[gpui_kit::test]
fn services_view_acquires_lease_dispatches_sort_and_clears_pid_filter_on_drop(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let dir = tempfile::tempdir().expect("tempdir");
    let config_store = ConfigStore::new(dir.path().join("settings.toml"));
    let settings = AppSettings::default();
    let backend = NativeBackend::initialize().expect("initialize native backend");
    let coordinator =
        Arc::new(EngineCoordinator::new(backend, settings.clone()).expect("start coordinator"));

    assert!(!coordinator.leases().is_active(MonitorDomain::Services));

    let store = cx.new(|cx| UiStore::new(coordinator.clone(), config_store, settings, cx));
    let shell_handle = ShellHandle::new(|_, _, _| {});

    let mut view_handle = None;
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |window, cx| {
            let view = cx.new(|cx| {
                ServicesView::new(store.clone(), Some(1234), shell_handle.clone(), window, cx)
            });
            view_handle = Some(view.clone());
            cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
        })
        .expect("open services test window")
    });
    let view = view_handle.expect("ServicesView constructed");

    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    vcx.update(|window, cx| window.draw(cx).clear(cx));

    assert!(
        coordinator.leases().is_active(MonitorDomain::Services),
        "mounting ServicesView must acquire MonitorDomain::Services lease"
    );

    // Trigger backend sort dispatch and clear PID filter cleanly.
    view.update(&mut vcx, |view, cx| {
        view.table.update(cx, |table, cx| {
            table.delegate_mut().trigger_sort(1, cx);
        });
        view.set_filter_pid(Some(5678), cx);
    });
    vcx.run_until_parked();

    drop(view);
    vcx.update(|window, _| window.remove_window());
    vcx.run_until_parked();

    assert!(
        !coordinator.leases().is_active(MonitorDomain::Services),
        "dropping ServicesView must release MonitorDomain::Services lease"
    );

    coordinator.shutdown();
}
