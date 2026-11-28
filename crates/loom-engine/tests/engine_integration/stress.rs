#![allow(unused_imports)]
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use loom_config::AppSettings;
use loom_engine::{EngineCommand, EngineConfig, EngineCoordinator};
use loom_types::{
    MonitorDomain, ProcessCategory, ProcessKey, ProcessRowKind, ProcessSortColumn,
    ProcessStatusFlags, ProcessViewMode, RefreshSpeed, SortDirection,
};

use crate::mock_backend::{MockBackend, MockProcSpec, MockState, wait_until};

#[test]
fn high_load_500_processes_hung_windows_and_slow_disk_stress_test() {
    use std::collections::HashSet;

    // Build 512 synthetic processes spanning:
    // - Index 0: slow-drive trigger binary on Z:\slow_app_0.exe
    // - 16 multi-level (depth 0 -> 1 -> 2) App trees (48 procs), with hung NOT_RESPONDING windows
    // - 443 mixed Background / System processes sharing local C:\ binaries
    // - Trailing 20 distinct executables on Z:\slow_app_1.exe .. Z:\slow_app_20.exe
    let mut procs = Vec::with_capacity(512);
    let mut hung_pids = HashSet::new();

    // Index 0: first slow-drive process (trips CircuitBreaker for Z: after >= 300ms).
    let mut slow_trigger = MockProcSpec::new(100, 1_000, 0, "slow_app_0.exe");
    slow_trigger.category = ProcessCategory::App;
    slow_trigger.window_title = "Slow External Drive App".to_owned();
    slow_trigger.exe_path = Some(r"Z:\slow_app_0.exe".to_owned());
    slow_trigger.private_mem_bytes = 256 * 1024 * 1024;
    slow_trigger.cpu_delta_per_tick = 500_000;
    procs.push(slow_trigger);

    // Multi-level 3-tier App trees (16 roots * 3 = 48 processes).
    let mut tracked_root_key = ProcessKey::default();
    let mut tracked_child_key = ProcessKey::default();
    let mut tracked_grandchild_key = ProcessKey::default();
    for i in 0..16u32 {
        let root_pid = 200 + i * 10;
        let child_pid = root_pid + 1;
        let grand_pid = root_pid + 2;
        let is_hung = i % 2 == 0;

        let mut root = MockProcSpec::new(
            root_pid,
            10_000 + u64::from(root_pid),
            0,
            &format!("studio_{i}.exe"),
        );
        root.category = ProcessCategory::App;
        root.window_title = if is_hung {
            format!("Hung Studio Window {i}")
        } else {
            format!("Studio Window {i}")
        };
        if is_hung {
            root.status = ProcessStatusFlags::NOT_RESPONDING;
            hung_pids.insert(root_pid);
        }
        root.exe_path = Some(format!(r"C:\Apps\studio_{}.exe", i % 4));
        root.private_mem_bytes = u64::from(128 + i * 16) * 1024 * 1024;
        root.cpu_delta_per_tick = u64::from(100_000 + i * 50_000);

        let mut child = MockProcSpec::new(
            child_pid,
            10_000 + u64::from(child_pid),
            root_pid,
            &format!("studio_{i}_worker.exe"),
        );
        child.category = ProcessCategory::Background;
        child.exe_path = Some(r"C:\Apps\worker.exe".to_owned());
        child.private_mem_bytes = u64::from(48 + i * 4) * 1024 * 1024;
        child.cpu_delta_per_tick = u64::from(40_000 + i * 10_000);

        let mut grand = MockProcSpec::new(
            grand_pid,
            10_000 + u64::from(grand_pid),
            child_pid,
            &format!("studio_{i}_gpu.exe"),
        );
        grand.category = ProcessCategory::Background;
        grand.exe_path = Some(r"C:\Apps\gpu_helper.exe".to_owned());
        grand.private_mem_bytes = u64::from(32 + i * 2) * 1024 * 1024;
        grand.cpu_delta_per_tick = u64::from(20_000 + i * 5_000);

        if i == 0 {
            tracked_root_key = root.key;
            tracked_child_key = child.key;
            tracked_grandchild_key = grand.key;
        }

        procs.push(root);
        procs.push(child);
        procs.push(grand);
    }

    // 443 mixed Background / System processes on fast local C:\ drive.
    for idx in 0..443u32 {
        let pid = 1_000 + idx * 4;
        let is_system = idx % 5 == 0;
        let parent_pid = if idx > 16 { 1_000 + (idx % 16) * 4 } else { 0 };
        let mut p = MockProcSpec::new(
            pid,
            50_000 + u64::from(idx),
            parent_pid,
            if is_system {
                "svchost.exe"
            } else {
                "bg_service.exe"
            },
        );
        p.category = if is_system {
            ProcessCategory::System
        } else {
            ProcessCategory::Background
        };
        p.exe_path = Some(if is_system {
            r"C:\Windows\System32\svchost.exe".to_owned()
        } else {
            format!(r"C:\ProgramData\Svc\bg_{}.exe", idx % 8)
        });
        p.private_mem_bytes = u64::from(8 + (idx % 64)) * 1024 * 1024;
        p.cpu_delta_per_tick = u64::from((idx % 20) * 10_000);
        procs.push(p);
    }

    // Trailing 20 distinct executables on Z:\ (indices 492..512, beyond initial 256 slow-queue cap).
    for idx in 1..=20u32 {
        let pid = 9_000 + idx * 4;
        let mut p = MockProcSpec::new(
            pid,
            90_000 + u64::from(idx),
            0,
            &format!("slow_app_{idx}.exe"),
        );
        p.category = ProcessCategory::Background;
        p.exe_path = Some(format!(r"Z:\slow_app_{idx}.exe"));
        p.private_mem_bytes = 16 * 1024 * 1024;
        procs.push(p);
    }
    assert_eq!(procs.len(), 512);

    let mut state = Arc::try_unwrap(MockState::new(procs)).ok().unwrap();
    state.logical_cpus = 16;
    state.mem_total_bytes = 32 * 1024 * 1024 * 1024;
    state.disk_probe_sleep = Duration::from_millis(500);
    state.exe_asset_slow_prefix = Some(r"Z:\".to_owned());
    state.exe_asset_sleep = Duration::from_millis(410);
    let state = Arc::new(state);

    let backend = MockBackend::from_state(Arc::clone(&state));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::Paused,
        ..AppSettings::default()
    };
    let config = EngineConfig {
        slow_workers: 2,
        slow_queue_capacity: 256,
        slow_probe_timeout: Duration::from_millis(300),
        slow_circuit_cooldown: Duration::from_secs(60),
        slow_shutdown_timeout: Duration::from_millis(100),
        ..EngineConfig::default()
    };
    let coord = EngineCoordinator::with_config(backend, settings, config).unwrap();

    // Wait for Tick 0 and lease both Performance (slow disk polling) and Details.
    assert!(wait_until(Duration::from_secs(1), || {
        coord.snapshot().revisions.processes >= 1
    }));
    let _perf_lease = coord.acquire_lease(MonitorDomain::Performance);
    let _details_lease = coord.acquire_lease(MonitorDomain::Details);

    // Wait until BOTH slow-lane workers are actively executing slow-disk & slow-asset stalls.
    assert!(
        wait_until(Duration::from_secs(2), || {
            state.probe_disk_calls.load(Ordering::Acquire) >= 1
                && state.probe_asset_slow_calls.load(Ordering::Acquire) >= 1
        }),
        "both slow disk and slow Z: asset probes should start on slow-lane workers"
    );

    // While both slow-lane workers are stalled in 410ms/500ms sleeps, fire 5 consecutive
    // fast-lane ForceRefresh ticks across all 512 processes and verify fast-lane responsiveness.
    let burst_start = Instant::now();
    let rev_before_burst = coord.snapshot().revisions.processes;
    for _ in 0..5 {
        let ev = coord
            .send_command_with_reply(EngineCommand::ForceRefresh)
            .unwrap()
            .recv_timeout(Duration::from_millis(200))
            .expect("fast lane must not block behind 410ms/500ms slow-lane stalls");
        assert!(ev.is_ok());
    }
    let burst_elapsed = burst_start.elapsed();
    assert!(
        burst_elapsed < Duration::from_millis(300),
        "5 fast-lane 512-process ticks should complete well under 410ms slow stall, took {burst_elapsed:?}"
    );
    assert!(coord.snapshot().revisions.processes >= rev_before_burst + 5);

    // Wait for slow-lane workers to finish the 410ms Z: probe (tripping CircuitBreaker for Z:)
    // and drain remaining ProcessMeta probes on subsequent ticks.
    assert!(
        wait_until(Duration::from_secs(3), || {
            if state.probe_proc_calls.load(Ordering::Acquire) < 512 {
                let _ = coord.send_command(EngineCommand::ForceRefresh);
                false
            } else {
                true
            }
        }),
        "all 512 process cold metadata probes should complete; got {}",
        state.probe_proc_calls.load(Ordering::Acquire)
    );
    // Give fast lane one more ForceRefresh barrier to apply any final drained slow results.
    coord
        .send_command_with_reply(EngineCommand::ForceRefresh)
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();

    // CircuitBreaker must have tripped on Z: after Z:\slow_app_0.exe (>= 300ms) and bypassed
    // all 20 trailing Z:\slow_app_1..20.exe asset probes!
    let slow_z_asset_calls = state.probe_asset_slow_calls.load(Ordering::Acquire);
    assert_eq!(
        slow_z_asset_calls, 1,
        "CircuitBreaker for Z: must bypass trailing 20 slow-drive asset probes, got {slow_z_asset_calls}"
    );

    // Verify snapshot integrity across all 512 processes & hung windows.
    let snap = coord.snapshot();
    assert_eq!(snap.processes.total_process_count, 512);
    assert_eq!(snap.details.rows.len(), 512);

    let projected_hung_details: HashSet<u32> = snap
        .details
        .rows
        .iter()
        .filter(|r| r.status.contains(ProcessStatusFlags::NOT_RESPONDING))
        .map(|r| r.key.pid)
        .collect();
    assert_eq!(
        projected_hung_details, hung_pids,
        "all NOT_RESPONDING hung windows must be accurately projected in Details"
    );
    assert!(
        snap.processes
            .rows
            .iter()
            .filter(|r| r.kind == ProcessRowKind::Process
                && r.status.contains(ProcessStatusFlags::NOT_RESPONDING))
            .count()
            >= hung_pids.len(),
        "hung NOT_RESPONDING apps must be surfaced in Processes slice"
    );

    // Verify Process Tree mode (zero duplicates, multi-level depth preserved).
    coord
        .send_command_with_reply(EngineCommand::SetProcessViewMode(ProcessViewMode::Tree))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    coord
        .send_command_with_reply(EngineCommand::ToggleProcessExpanded(tracked_root_key))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    coord
        .send_command_with_reply(EngineCommand::ToggleProcessExpanded(tracked_child_key))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();

    let tree_snap = coord.snapshot();
    let mut seen_keys = HashSet::with_capacity(tree_snap.processes.rows.len());
    for row in &tree_snap.processes.rows {
        assert_eq!(row.kind, ProcessRowKind::Process);
        assert!(seen_keys.insert(row.key), "duplicate key in Tree mode");
    }
    let grand_row = tree_snap
        .processes
        .rows
        .iter()
        .find(|r| r.key == tracked_grandchild_key)
        .expect("expanded depth-2 grandchild must be visible in Tree mode");
    assert_eq!(grand_row.depth, 2);

    // Verify search & sort accuracy on 512 processes.
    coord
        .send_command_with_reply(EngineCommand::SetProcessViewMode(ProcessViewMode::Grouped))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    coord
        .send_command_with_reply(EngineCommand::SetProcessSort {
            column: ProcessSortColumn::Memory,
            direction: SortDirection::Descending,
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let sorted_snap = coord.snapshot();
    let app_mems: Vec<u64> = sorted_snap
        .processes
        .rows
        .iter()
        .skip_while(|r| {
            !matches!(
                r.kind,
                ProcessRowKind::SectionHeader {
                    category: ProcessCategory::App,
                    ..
                }
            )
        })
        .skip(1)
        .take_while(|r| r.kind == ProcessRowKind::Process)
        .filter(|r| r.depth == 0)
        .map(|r| r.memory_bytes)
        .collect();
    assert!(app_mems.len() >= 16);
    assert!(
        app_mems.windows(2).all(|w| w[0] >= w[1]),
        "Apps section must be sorted by Memory descending"
    );

    // Search for a specific hung window title and verify accurate narrowing.
    coord
        .send_command_with_reply(EngineCommand::SetSearchQuery("Hung Studio Window 0".into()))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let search_snap = coord.snapshot();
    assert!(
        search_snap
            .processes
            .rows
            .iter()
            .any(|r| r.key == tracked_root_key
                && r.status.contains(ProcessStatusFlags::NOT_RESPONDING)),
        "search must find hung studio_0 root process"
    );
    assert!(search_snap.processes.rows.len() < 10);

    // Verify bottom-up kill ordering on the 3-level subtree (grandchild -> child -> root).
    let kill_ev = coord
        .send_command_with_reply(EngineCommand::TerminateProcessTree(tracked_root_key))
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert!(kill_ev.is_ok());
    let recorded = state.recorded_tree_kills.lock();
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0].as_slice(),
        &[tracked_grandchild_key, tracked_child_key, tracked_root_key],
        "TerminateProcessTree must kill grandchild -> child -> root strictly bottom-up"
    );
    drop(recorded);

    coord.shutdown();
}
