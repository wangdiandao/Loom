//! Unit and GPUI integration tests for the Users vertical feature slice.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{AppContext as _, TestAppContext, VisualTestContext};
use loom_config::{AppSettings, ConfigStore};
use loom_engine::EngineCoordinator;
use loom_pal::{NativeBackend, PlatformBackend};
use loom_types::{
    MonitorDomain, ProcessKey, ProcessStatusFlags, SortDirection, UserSessionProcess,
    UserSessionState, UsersRowProjection, UsersSlice, UsersSortColumn,
};

use crate::bridge::UiStore;
use crate::shell::ShellHandle;

use super::UsersView;
use super::state::{CoalesceDecision, ScrollCoalescer, compute_users_viewport_fingerprint};

fn sample_proc(pid: u32, name: &str, cpu_permille: u16, mem: u64) -> UserSessionProcess {
    UserSessionProcess {
        key: ProcessKey::new(pid, u64::from(pid) * 100),
        name: Arc::from(name),
        display_name: Arc::from(name),
        status: ProcessStatusFlags::empty(),
        cpu_permille,
        memory_bytes: mem,
        disk_bytes_per_sec: 0,
        net_bytes_per_sec: 0,
    }
}

#[test]
fn users_viewport_fingerprint_tracks_visible_quantized_metrics_and_ignores_offscreen() {
    let mut slice = UsersSlice {
        revision: 1,
        sort_column: UsersSortColumn::Cpu,
        sort_direction: SortDirection::Descending,
        rows: vec![
            UsersRowProjection::Session {
                session_id: 1,
                user_name: Arc::from("Alice"),
                state: UserSessionState::Active,
                process_count: 2,
                cpu_permille: 120,
                memory_bytes: 128 * 1024 * 1024,
                disk_bytes_per_sec: 0,
                net_bytes_per_sec: 0,
                has_children: true,
                is_expanded: true,
            },
            UsersRowProjection::Process {
                session_id: 1,
                process: sample_proc(101, "code.exe", 100, 64 * 1024 * 1024),
            },
            UsersRowProjection::Process {
                session_id: 1,
                process: sample_proc(102, "notepad.exe", 20, 32 * 1024 * 1024),
            },
        ],
        ..UsersSlice::default()
    };

    let ram = 16 * 1024 * 1024 * 1024;
    let fp_initial = compute_users_viewport_fingerprint(&slice, ram, &(0..2), Some(0));

    // Sub-tenth memory jitter (4 KB on 128.0 MB) stays in the same quantized display bucket.
    if let UsersRowProjection::Session { memory_bytes, .. } = &mut slice.rows[0] {
        *memory_bytes += 4_096;
    }
    assert_eq!(
        fp_initial,
        compute_users_viewport_fingerprint(&slice, ram, &(0..2), Some(0))
    );

    // Off-screen row change (index 2) leaves visible fingerprint unchanged.
    if let UsersRowProjection::Process { process, .. } = &mut slice.rows[2] {
        process.cpu_permille = 450;
    }
    assert_eq!(
        fp_initial,
        compute_users_viewport_fingerprint(&slice, ram, &(0..2), Some(0))
    );

    // Visible CPU change (index 1) invalidates the fingerprint.
    if let UsersRowProjection::Process { process, .. } = &mut slice.rows[1] {
        process.cpu_permille = 250;
    }
    assert_ne!(
        fp_initial,
        compute_users_viewport_fingerprint(&slice, ram, &(0..2), Some(0))
    );
}

#[test]
fn users_scroll_coalescer_enforces_quiet_window_cap_and_bypass() {
    let mut coalescer = ScrollCoalescer::default();
    let t0 = Instant::now();
    assert_eq!(
        coalescer.decide(t0 + Duration::from_millis(20), Some(t0), false),
        CoalesceDecision::Defer(Duration::from_millis(130))
    );
    assert_eq!(
        coalescer.decide(t0 + Duration::from_millis(150), Some(t0), false),
        CoalesceDecision::ApplyNow
    );
    // Bypass immediately flushes.
    assert_eq!(
        coalescer.decide(t0 + Duration::from_millis(10), Some(t0), true),
        CoalesceDecision::ApplyNow
    );
}

#[gpui_kit::test]
fn users_view_acquires_lease_dispatches_sort_and_expand_and_releases_on_drop(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let dir = tempfile::tempdir().expect("tempdir");
    let config_store = ConfigStore::new(dir.path().join("settings.toml"));
    let settings = AppSettings::default();
    let backend = NativeBackend::initialize().expect("initialize native backend");
    let coordinator =
        Arc::new(EngineCoordinator::new(backend, settings.clone()).expect("start coordinator"));

    assert!(!coordinator.leases().is_active(MonitorDomain::Users));

    let store = cx.new(|cx| UiStore::new(coordinator.clone(), config_store, settings, cx));
    let shell_handle = ShellHandle::new(|_, _, _| {});

    let mut view_handle = None;
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |window, cx| {
            let view = cx.new(|cx| UsersView::new(store.clone(), shell_handle.clone(), window, cx));
            view_handle = Some(view.clone());
            cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
        })
        .expect("open users test window")
    });
    let view = view_handle.expect("UsersView constructed");

    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    vcx.update(|window, cx| window.draw(cx).clear(cx));

    assert!(
        coordinator.leases().is_active(MonitorDomain::Users),
        "mounting UsersView must acquire MonitorDomain::Users lease"
    );

    view.update(&mut vcx, |view, cx| {
        view.table.update(cx, |table, cx| {
            table.delegate_mut().trigger_sort(3, cx);
            table.delegate_mut().trigger_toggle_session(1, cx);
        });
    });
    vcx.run_until_parked();

    drop(view);
    vcx.update(|window, _| window.remove_window());
    vcx.run_until_parked();

    assert!(
        !coordinator.leases().is_active(MonitorDomain::Users),
        "dropping UsersView must release MonitorDomain::Users lease"
    );

    coordinator.shutdown();
}
