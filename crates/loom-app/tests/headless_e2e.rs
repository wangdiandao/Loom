//! Real-OS headless end-to-end integration test for `loom-pal` + `loom-engine`.
//!
//! Exercises the live Windows native backend (`NativeBackend` -> `WindowsBackend`)
//! wired into `EngineCoordinator` without any UI framework or mock backend.

#![cfg(windows)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use loom_config::AppSettings;
use loom_engine::{EngineCommand, EngineCoordinator};
use loom_pal::{NativeBackend, PlatformBackend};
use loom_types::{MonitorDomain, ProcessKey, ProcessRowKind, ProcessViewMode};

fn wait_until<F>(timeout: Duration, mut cond: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    cond()
}

fn send_and_ack(coordinator: &EngineCoordinator<NativeBackend>, cmd: EngineCommand) {
    let reply = coordinator
        .send_command_with_reply(cmd)
        .expect("enqueue engine command");
    let event = reply
        .recv_timeout(Duration::from_secs(3))
        .expect("receive engine command ack");
    assert!(
        event.is_ok(),
        "engine command {:?} failed: {:?}",
        event.kind,
        event.error()
    );
}

#[test]
fn headless_windows_e2e_lifecycle() {
    let my_pid = std::process::id();

    let backend = NativeBackend::initialize().expect("NativeBackend::initialize");
    let settings = AppSettings::default();
    let coordinator = EngineCoordinator::new(backend, settings).expect("EngineCoordinator::new");

    // 1. Tick 0 publishes a live AppSnapshot with real OS processes.
    assert!(
        wait_until(Duration::from_secs(3), || {
            coordinator.snapshot().revisions.processes >= 1
        }),
        "Tick 0 should publish an initial process snapshot"
    );

    let snap0 = coordinator.snapshot();
    assert_eq!(snap0.capabilities, NativeBackend::CAPABILITIES);
    assert!(
        snap0.processes.total_process_count > 10,
        "expected > 10 live OS processes on Tick 0, got {}",
        snap0.processes.total_process_count
    );
    assert!(
        !snap0.processes.rows.is_empty(),
        "Tick 0 processes.rows must not be empty"
    );
    assert_eq!(snap0.processes.view_mode, ProcessViewMode::Grouped);
    assert_eq!(
        snap0.revisions.details, 0,
        "Details slice must not be projected before acquiring a MonitorLease"
    );
    assert!(snap0.details.rows.is_empty());

    // 2. Acquiring MonitorLease(Details) populates details.rows with the current process.
    let details_lease = coordinator.acquire_lease(MonitorDomain::Details);
    assert!(
        wait_until(Duration::from_secs(3), || {
            let snap = coordinator.snapshot();
            snap.revisions.details >= 1 && !snap.details.rows.is_empty()
        }),
        "acquiring Details lease should populate details.rows"
    );

    let details_snap = coordinator.snapshot();
    let my_details = details_snap
        .details
        .rows
        .iter()
        .find(|row| row.key.pid == my_pid)
        .expect("current process must appear in details.rows");
    assert!(
        my_details.key.create_time_100ns > 0,
        "current process should have non-zero create_time_100ns"
    );
    assert!(
        !my_details.name.is_empty(),
        "current process should have non-empty image name"
    );
    assert!(
        my_details.working_set_bytes > 0,
        "current process should report positive working_set_bytes"
    );

    // Collect verified ancestors of the current process so we can expand them if the
    // test runner spawned us under a parent in the same Grouped section (e.g., cargo.exe).
    let parent_by_key: HashMap<ProcessKey, Option<ProcessKey>> = details_snap
        .details
        .rows
        .iter()
        .map(|row| (row.key, row.parent_key))
        .collect();
    let mut ancestors = Vec::new();
    let mut visited = HashSet::new();
    let mut cursor = my_details.parent_key;
    while let Some(parent_key) = cursor {
        if !visited.insert(parent_key) {
            break;
        }
        ancestors.push(parent_key);
        cursor = parent_by_key.get(&parent_key).copied().flatten();
    }

    // Expand ancestors (top-down) so the current process is directly visible in processes.rows
    // in both Grouped and Tree modes even without a search filter.
    for &ancestor in ancestors.iter().rev() {
        send_and_ack(&coordinator, EngineCommand::ToggleProcessExpanded(ancestor));
    }

    let grouped_with_self = coordinator.snapshot();
    assert!(
        grouped_with_self
            .processes
            .rows
            .iter()
            .any(|row| row.kind == ProcessRowKind::Process && row.key.pid == my_pid),
        "current process (PID {my_pid}) must be located in processes.rows"
    );

    // Dropping the Details lease stops further details revision bumps.
    drop(details_lease);
    // Synchronizing barrier tick guarantees any pre-drop in-flight publication has finished.
    send_and_ack(&coordinator, EngineCommand::ForceRefresh);
    let frozen_snap = coordinator.snapshot();
    let frozen_details_rev = frozen_snap.revisions.details;

    for _ in 0..2 {
        send_and_ack(&coordinator, EngineCommand::ForceRefresh);
    }
    let after_drop_snap = coordinator.snapshot();
    assert_eq!(
        after_drop_snap.revisions.details, frozen_details_rev,
        "details revision must stop bumping after MonitorLease(Details) is dropped"
    );
    assert!(
        Arc::ptr_eq(&frozen_snap.details, &after_drop_snap.details),
        "details Arc slice should remain untouched while unleased"
    );

    // 3. Switching ProcessViewMode::Tree and back to ProcessViewMode::Grouped succeeds
    //    without panic or cycles.
    send_and_ack(
        &coordinator,
        EngineCommand::SetProcessViewMode(ProcessViewMode::Tree),
    );
    let tree_snap = coordinator.snapshot();
    assert_eq!(tree_snap.processes.view_mode, ProcessViewMode::Tree);
    assert!(tree_snap.processes.total_process_count > 10);
    assert!(!tree_snap.processes.rows.is_empty());

    let mut seen_tree_keys = HashSet::with_capacity(tree_snap.processes.rows.len());
    for row in &tree_snap.processes.rows {
        assert_eq!(
            row.kind,
            ProcessRowKind::Process,
            "Tree mode must not emit SectionHeader rows"
        );
        assert!(
            seen_tree_keys.insert(row.key),
            "Tree mode must never emit duplicate ProcessKey {:?} (cycle guard)",
            row.key
        );
        assert!(
            usize::from(row.depth) < tree_snap.processes.total_process_count,
            "Tree row depth {} out of bounds",
            row.depth
        );
    }
    assert!(
        tree_snap
            .processes
            .rows
            .iter()
            .any(|row| row.key.pid == my_pid),
        "current process (PID {my_pid}) must appear in Tree mode"
    );

    send_and_ack(
        &coordinator,
        EngineCommand::SetProcessViewMode(ProcessViewMode::Grouped),
    );
    let back_to_grouped = coordinator.snapshot();
    assert_eq!(
        back_to_grouped.processes.view_mode,
        ProcessViewMode::Grouped
    );
    assert!(
        back_to_grouped
            .processes
            .rows
            .iter()
            .any(|row| matches!(row.kind, ProcessRowKind::SectionHeader { .. })),
        "Grouped mode must emit section headers"
    );
    let mut seen_grouped_keys = HashSet::with_capacity(back_to_grouped.processes.rows.len());
    for row in back_to_grouped
        .processes
        .rows
        .iter()
        .filter(|row| row.kind == ProcessRowKind::Process)
    {
        assert!(
            seen_grouped_keys.insert(row.key),
            "Grouped mode must not contain duplicate ProcessKey {:?}",
            row.key
        );
    }

    // 4. Search query filtering narrows processes.rows cleanly and clearing restores full list.
    let unfiltered_process_rows = back_to_grouped
        .processes
        .rows
        .iter()
        .filter(|row| row.kind == ProcessRowKind::Process)
        .count();
    assert!(
        unfiltered_process_rows > 1,
        "expected multiple process rows before filtering, got {unfiltered_process_rows}"
    );

    send_and_ack(
        &coordinator,
        EngineCommand::SetSearchQuery(my_pid.to_string()),
    );
    let filtered_snap = coordinator.snapshot();
    let filtered_process_rows = filtered_snap
        .processes
        .rows
        .iter()
        .filter(|row| row.kind == ProcessRowKind::Process)
        .count();
    assert!(
        filtered_process_rows >= 1,
        "PID query '{my_pid}' must match at least the current process"
    );
    assert!(
        filtered_process_rows < unfiltered_process_rows,
        "PID query '{my_pid}' should narrow processes.rows ({filtered_process_rows} vs {unfiltered_process_rows})"
    );
    assert!(
        filtered_snap
            .processes
            .rows
            .iter()
            .any(|row| row.kind == ProcessRowKind::Process && row.key.pid == my_pid),
        "filtered processes.rows must include current process (PID {my_pid})"
    );

    // Non-matching sentinel query yields zero rows.
    send_and_ack(
        &coordinator,
        EngineCommand::SetSearchQuery("__loom_e2e_no_such_process_sentinel__".to_owned()),
    );
    assert!(
        coordinator.snapshot().processes.rows.is_empty(),
        "non-matching query should yield an empty row list"
    );

    // Clearing the query restores the full process list.
    send_and_ack(&coordinator, EngineCommand::SetSearchQuery(String::new()));
    let restored_snap = coordinator.snapshot();
    let restored_process_rows = restored_snap
        .processes
        .rows
        .iter()
        .filter(|row| row.kind == ProcessRowKind::Process)
        .count();
    assert!(
        restored_process_rows > filtered_process_rows,
        "clearing search query must restore full list ({restored_process_rows} vs {filtered_process_rows})"
    );
    assert!(
        restored_snap.processes.total_process_count > 10,
        "restored snapshot should still track > 10 live processes"
    );

    // 5. Clean shutdown completes swiftly without hanging.
    let shutdown_start = Instant::now();
    coordinator.shutdown();
    let shutdown_elapsed = shutdown_start.elapsed();
    assert!(
        shutdown_elapsed < Duration::from_secs(2),
        "EngineCoordinator::shutdown took too long: {shutdown_elapsed:?}"
    );
}
