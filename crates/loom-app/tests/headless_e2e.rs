//! Real-OS headless end-to-end integration test for `loom-pal` + `loom-engine`.
//!
//! Exercises the live Windows native backend (`NativeBackend` -> `WindowsBackend`)
//! wired into `EngineCoordinator` without any UI framework or mock backend.

#![cfg(windows)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use loom_config::{AppSettings, ConfigStore};
use loom_engine::{EngineCommand, EngineCoordinator};
use loom_pal::{
    FastSystemSampler, NativeBackend, PlatformBackend, ProcessSampleScratch, SystemOperator,
};
use loom_types::{MonitorDomain, ProcessKey, ProcessRowKind, ProcessViewMode, RefreshSpeed};
use parking_lot::Mutex;

/// Serializes real-OS E2E tests so concurrent coordinators in the same test binary
/// do not cross-contaminate process-wide thread counts or CPU% budget measurements.
static E2E_SERIAL: Mutex<()> = Mutex::new(());

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

fn wait_until_fast<F>(timeout: Duration, mut cond: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    cond()
}

fn sample_self_metrics(
    sampler: &mut <NativeBackend as PlatformBackend>::FastSampler,
    scratch: &mut ProcessSampleScratch<<NativeBackend as PlatformBackend>::ProcessExt>,
    my_pid: u32,
) -> (u32, u64, u64) {
    scratch.clear();
    sampler
        .sample_processes(scratch)
        .expect("sample_processes for self metrics");
    let me = scratch
        .samples
        .iter()
        .find(|s| s.key.pid == my_pid)
        .expect("current test process must exist in SystemProcessInformation");
    (
        me.thread_count,
        me.kernel_time_100ns.saturating_add(me.user_time_100ns),
        me.private_working_set_bytes,
    )
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
    let _serial = E2E_SERIAL.lock();
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
    assert_eq!(snap0.platform, NativeBackend::PLATFORM);
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

#[test]
fn headless_windows_engineering_budget_verification() {
    let _serial = E2E_SERIAL.lock();
    let my_pid = std::process::id();

    // 1. Config load latency (cold first-open + warm < 5 ms debug / < 1 ms typical).
    let temp_dir = tempfile::tempdir().expect("create temp dir for config budget test");
    let config_path = temp_dir.path().join("settings.toml");
    let seed_store = ConfigStore::new(config_path.clone());
    seed_store
        .save(&AppSettings::default())
        .expect("seed settings.toml");

    let cfg_cold_start = Instant::now();
    let (loaded_settings, load_err) = ConfigStore::new(config_path.clone()).load_or_default();
    let cfg_cold_elapsed = cfg_cold_start.elapsed();
    assert!(
        load_err.is_none(),
        "seeded settings.toml should load cleanly: {load_err:?}"
    );
    assert_eq!(loaded_settings.refresh_speed, RefreshSpeed::Normal1S);

    let cfg_warm_start = Instant::now();
    let (warm_settings, warm_err) = ConfigStore::new(config_path).load_or_default();
    let cfg_warm_elapsed = cfg_warm_start.elapsed();
    assert!(warm_err.is_none());
    assert_eq!(warm_settings, loaded_settings);

    eprintln!(
        "[budget] config load: cold={:.3} ms ({} us), warm={:.3} ms ({} us)",
        cfg_cold_elapsed.as_secs_f64() * 1_000.0,
        cfg_cold_elapsed.as_micros(),
        cfg_warm_elapsed.as_secs_f64() * 1_000.0,
        cfg_warm_elapsed.as_micros()
    );
    let cfg_warm_ceiling = if cfg!(debug_assertions) {
        Duration::from_millis(10)
    } else {
        Duration::from_millis(5)
    };
    assert!(
        cfg_warm_elapsed < cfg_warm_ceiling,
        "warm config load exceeded budget ({cfg_warm_elapsed:?} >= {cfg_warm_ceiling:?})"
    );

    // Prepare a standalone probe sampler to measure warm fast-lane scan latency,
    // baseline OS thread count, and self CPU% without interfering with EngineCoordinator.
    let probe_backend = NativeBackend::initialize().expect("probe NativeBackend::initialize");
    let logical_cpus = probe_backend.operator().logical_cpu_count().max(1);
    let mut probe_sampler = probe_backend
        .create_fast_sampler()
        .expect("probe create_fast_sampler");
    let mut probe_scratch = ProcessSampleScratch::with_capacity(512, 16 * 1024);

    // Warm up standalone sampler once and measure steady-state warm sample_processes latency.
    let _ = sample_self_metrics(&mut probe_sampler, &mut probe_scratch, my_pid);
    let mut sample_total = Duration::ZERO;
    let mut sample_max = Duration::ZERO;
    const WARM_ITERS: u32 = 20;
    for _ in 0..WARM_ITERS {
        probe_scratch.clear();
        let t0 = Instant::now();
        probe_sampler
            .sample_processes(&mut probe_scratch)
            .expect("warm sample_processes");
        let dt = t0.elapsed();
        sample_total += dt;
        if dt > sample_max {
            sample_max = dt;
        }
    }
    let avg_sample_ms = (sample_total.as_secs_f64() * 1_000.0) / f64::from(WARM_ITERS);
    eprintln!(
        "[budget] warm sample_processes ({} procs, {} iters): avg={:.3} ms, max={:.3} ms",
        probe_scratch.samples.len(),
        WARM_ITERS,
        avg_sample_ms,
        sample_max.as_secs_f64() * 1_000.0
    );

    // Record baseline self OS thread count immediately prior to EngineCoordinator::new.
    let (threads_before, _, _) =
        sample_self_metrics(&mut probe_sampler, &mut probe_scratch, my_pid);

    // 2. Measure EngineCoordinator::<NativeBackend>::new time-to-first-non-empty-snapshot.
    let cold_start = Instant::now();
    let backend = NativeBackend::initialize().expect("NativeBackend::initialize");
    let coordinator =
        EngineCoordinator::new(backend, loaded_settings).expect("EngineCoordinator::new");
    assert!(
        wait_until_fast(Duration::from_secs(3), || {
            let snap = coordinator.snapshot();
            snap.revisions.processes >= 1 && !snap.processes.rows.is_empty()
        }),
        "Tick 0 must publish a non-empty snapshot"
    );
    let first_snap_elapsed = cold_start.elapsed();
    let snap0 = coordinator.snapshot();
    eprintln!(
        "[budget] EngineCoordinator first non-empty snapshot ({} live procs): {:.3} ms",
        snap0.processes.total_process_count,
        first_snap_elapsed.as_secs_f64() * 1_000.0
    );
    let cold_ceiling = if cfg!(debug_assertions) {
        Duration::from_millis(250)
    } else {
        Duration::from_millis(50)
    };
    assert!(
        first_snap_elapsed < cold_ceiling,
        "time-to-first-snapshot exceeded budget ({first_snap_elapsed:?} >= {cold_ceiling:?})"
    );

    // 3. Verify self process thread count delta caused by EngineCoordinator is strictly <= 3
    //    (1 fast-lane thread + <= 2 slow-lane bulkhead workers).
    let (threads_after_init, _, pws_bytes) =
        sample_self_metrics(&mut probe_sampler, &mut probe_scratch, my_pid);
    let thread_delta = threads_after_init.saturating_sub(threads_before);
    eprintln!(
        "[budget] OS threads: before={threads_before}, after_init={threads_after_init}, delta={thread_delta}, private_ws={:.2} MB",
        pws_bytes as f64 / (1024.0 * 1024.0)
    );
    assert!(
        thread_delta <= 3,
        "EngineCoordinator must spawn at most 3 background threads (before={threads_before}, after={threads_after_init}, delta={thread_delta})"
    );

    // 4. Measure steady-state headless 1s-cadence CPU% after initial cold-cache warmup.
    //    Allow Tick 1 (+250ms calibration) and initial slow-lane cold backfill to settle.
    std::thread::sleep(Duration::from_millis(1_500));
    let rev_before_steady = coordinator.snapshot().revisions.processes;
    let wall_t0 = Instant::now();
    let (_, cpu_t0, _) = sample_self_metrics(&mut probe_sampler, &mut probe_scratch, my_pid);
    std::thread::sleep(Duration::from_millis(2_200));
    let (_, cpu_t1, pws_steady) =
        sample_self_metrics(&mut probe_sampler, &mut probe_scratch, my_pid);
    let wall_elapsed = wall_t0.elapsed();
    let rev_after_steady = coordinator.snapshot().revisions.processes;

    let cpu_delta_100ns = cpu_t1.saturating_sub(cpu_t0);
    let cpu_pct = (cpu_delta_100ns as f64
        / (wall_elapsed.as_secs_f64() * 1e7 * f64::from(logical_cpus)))
        * 100.0;
    eprintln!(
        "[budget] steady-state 1s headless ({} ticks over {:.2}s, {} logical CPUs): cpu={:.4}%, private_ws={:.2} MB",
        rev_after_steady.saturating_sub(rev_before_steady),
        wall_elapsed.as_secs_f64(),
        logical_cpus,
        cpu_pct,
        pws_steady as f64 / (1024.0 * 1024.0)
    );
    assert!(
        rev_after_steady > rev_before_steady,
        "steady-state 1s cadence must continue publishing process snapshots"
    );
    let cpu_ceiling_pct = if cfg!(debug_assertions) { 5.0 } else { 1.0 };
    assert!(
        cpu_pct < cpu_ceiling_pct,
        "steady-state headless CPU% ({cpu_pct:.4}%) exceeded test ceiling ({cpu_ceiling_pct:.2}%)"
    );

    // 5. Real WindowsBackend on-demand lease acquire/release cycle across Performance,
    //    PerCoreCpu, Services, Startup, Users, and AppHistory without leaks or lingering polls.
    let perf_lease = coordinator.acquire_lease(MonitorDomain::Performance);
    let per_core_lease = coordinator.acquire_lease(MonitorDomain::PerCoreCpu);
    let services_lease = coordinator.acquire_lease(MonitorDomain::Services);
    let startup_lease = coordinator.acquire_lease(MonitorDomain::Startup);
    let users_lease = coordinator.acquire_lease(MonitorDomain::Users);
    let history_lease = coordinator.acquire_lease(MonitorDomain::AppHistory);

    assert!(
        wait_until(Duration::from_secs(6), || {
            let snap = coordinator.snapshot();
            snap.revisions.performance >= 1
                && !snap.performance.per_core_permille.is_empty()
                && snap.revisions.services >= 1
                && snap.revisions.startup >= 1
                && snap.revisions.users >= 1
                && snap.revisions.app_history >= 1
        }),
        "all leased on-demand domains (Performance/PerCoreCpu/Services/Startup/Users/AppHistory) must populate"
    );

    let leased_snap = coordinator.snapshot();
    eprintln!(
        "[budget] leased domains populated: per_core={}, services={}, startup={}, users={}, app_history={}",
        leased_snap.performance.per_core_permille.len(),
        leased_snap.services.items.len(),
        leased_snap.startup.items.len(),
        leased_snap.users.sessions.len(),
        leased_snap.app_history.items.len(),
    );
    assert!(
        !leased_snap.services.items.is_empty(),
        "real Windows SCM should enumerate active services"
    );

    // Drop all on-demand leases and run a synchronizing ForceRefresh barrier so any
    // in-flight slow-lane polls finish before capturing frozen revisions.
    drop(perf_lease);
    drop(per_core_lease);
    drop(services_lease);
    drop(startup_lease);
    drop(users_lease);
    drop(history_lease);

    send_and_ack(&coordinator, EngineCommand::ForceRefresh);
    std::thread::sleep(Duration::from_millis(100));
    send_and_ack(&coordinator, EngineCommand::ForceRefresh);

    let frozen = coordinator.snapshot();
    for _ in 0..2 {
        send_and_ack(&coordinator, EngineCommand::ForceRefresh);
    }
    let after_release = coordinator.snapshot();

    assert_eq!(
        after_release.revisions.services, frozen.revisions.services,
        "Services revision must not bump after lease drop"
    );
    assert!(Arc::ptr_eq(&frozen.services, &after_release.services));

    assert_eq!(
        after_release.revisions.startup, frozen.revisions.startup,
        "Startup revision must not bump after lease drop"
    );
    assert!(Arc::ptr_eq(&frozen.startup, &after_release.startup));

    assert_eq!(
        after_release.revisions.users, frozen.revisions.users,
        "Users revision must not bump after lease drop"
    );
    assert!(Arc::ptr_eq(&frozen.users, &after_release.users));

    assert_eq!(
        after_release.revisions.app_history, frozen.revisions.app_history,
        "AppHistory revision must not bump after lease drop"
    );
    assert!(Arc::ptr_eq(&frozen.app_history, &after_release.app_history));

    assert!(
        after_release.performance.per_core_permille.is_empty(),
        "PerCoreCpu slice must clear per-core vector once unleased"
    );

    // Clean shutdown completes swiftly.
    let shutdown_start = Instant::now();
    coordinator.shutdown();
    let shutdown_elapsed = shutdown_start.elapsed();
    assert!(
        shutdown_elapsed < Duration::from_secs(2),
        "EngineCoordinator::shutdown took too long: {shutdown_elapsed:?}"
    );
}
