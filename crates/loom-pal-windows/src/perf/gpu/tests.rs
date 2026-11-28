use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use loom_types::{ProcessCategory, ProcessKey};

use super::power::{
    GpuDevicePowerState, GpuPowerGate, HYBRID_IDLE_PERFDATA_INTERVAL, PerfDataGateDecision,
};
use super::process::{
    ACTIVE_FULL_RECHECK_STRIDE, BG_NOGPU_RECHECK_STRIDE, ProcessGpuNodeKey, ProcessGpuProbeState,
    compute_process_node_util, next_probe_state_after_scan, prune_stale_process_gpu_nodes,
    should_probe_process,
};
use super::{GpuApiState, GpuPerfSampler};

#[test]
fn gpu_sampler_defers_dxgi_loading_off_cold_start_construction() {
    let sampler = GpuPerfSampler::new();
    assert!(matches!(sampler.api, GpuApiState::Uninitialized));
}

#[test]
fn gpu_power_gate_skips_sleeping_hybrid_dgpu_and_clears_cached_temp() {
    let mut gate = GpuPowerGate::new(true);
    let t0 = Instant::now();
    let t1 = t0 + Duration::from_secs(1);

    // First active D0 tick allows query and caches temperature.
    assert_eq!(
        gate.evaluate(GpuDevicePowerState::D0, true, t0),
        PerfDataGateDecision::Allow
    );
    gate.record_temperature(Some(52));
    assert_eq!(gate.cached_temperature(), Some(52));

    // Sleeping D1-D3 must ALWAYS skip and clear cached temperature to None.
    assert_eq!(
        gate.evaluate(GpuDevicePowerState::LowPowerD1ToD3, false, t1),
        PerfDataGateDecision::SkipSleeping
    );
    assert_eq!(gate.cached_temperature(), None);

    // Even if node activity flag were somehow true while in D3, D3 still skips.
    assert_eq!(
        gate.evaluate(
            GpuDevicePowerState::LowPowerD1ToD3,
            true,
            t1 + Duration::from_secs(30)
        ),
        PerfDataGateDecision::SkipSleeping
    );
}

#[test]
fn gpu_power_gate_throttles_idle_d0_to_20s_and_allows_active_d0_immediately() {
    let mut gate = GpuPowerGate::new(true);
    let t0 = Instant::now();

    // Initial idle D0 tick is allowed (establishes baseline).
    assert_eq!(
        gate.evaluate(GpuDevicePowerState::D0, false, t0),
        PerfDataGateDecision::Allow
    );
    gate.record_temperature(Some(41));

    // Subsequent idle D0 ticks before 20s must be throttled so dGPU can re-enter D3.
    assert_eq!(
        gate.evaluate(GpuDevicePowerState::D0, false, t0 + Duration::from_secs(5)),
        PerfDataGateDecision::SkipIdleThrottled
    );
    assert_eq!(gate.cached_temperature(), Some(41));
    assert_eq!(
        gate.evaluate(GpuDevicePowerState::D0, false, t0 + Duration::from_secs(19)),
        PerfDataGateDecision::SkipIdleThrottled
    );

    // At 20s elapsed, idle D0 is allowed again.
    let t20 = t0 + HYBRID_IDLE_PERFDATA_INTERVAL;
    assert_eq!(
        gate.evaluate(GpuDevicePowerState::D0, false, t20),
        PerfDataGateDecision::Allow
    );

    // Active D0 is allowed immediately even 1s after the previous query.
    assert_eq!(
        gate.evaluate(GpuDevicePowerState::D0, true, t20 + Duration::from_secs(1)),
        PerfDataGateDecision::Allow
    );
}

#[test]
fn gpu_power_gate_always_allows_non_hybrid_adapters() {
    let mut gate = GpuPowerGate::new(false);
    let t0 = Instant::now();

    assert_eq!(
        gate.evaluate(GpuDevicePowerState::D0, false, t0),
        PerfDataGateDecision::Allow
    );
    assert_eq!(
        gate.evaluate(GpuDevicePowerState::D0, false, t0 + Duration::from_secs(1)),
        PerfDataGateDecision::Allow
    );
    assert_eq!(
        gate.evaluate(
            GpuDevicePowerState::LowPowerD1ToD3,
            false,
            t0 + Duration::from_secs(2)
        ),
        PerfDataGateDecision::Allow
    );
}

#[test]
fn per_process_gpu_delta_math_counter_reset_clamping_and_stale_pruning() {
    let mut prev = HashMap::new();
    let t0 = Instant::now();
    let t1 = t0 + Duration::from_secs(1);
    let t2 = t1 + Duration::from_secs(1);
    let t3 = t2 + Duration::from_secs(1);

    let p1 = ProcessKey::new(1001, 111_111);
    let p2 = ProcessKey::new(2002, 222_222);
    let k1 = ProcessGpuNodeKey {
        process: p1,
        sequence_number: Some(10),
        adapter_luid: 0xAA,
        node_id: 0,
    };
    let k2 = ProcessGpuNodeKey {
        process: p2,
        sequence_number: Some(20),
        adapter_luid: 0xAA,
        node_id: 0,
    };

    // Cold sample returns 0.
    assert_eq!(compute_process_node_util(&mut prev, k1, 5_000_000, t0), 0);
    assert_eq!(compute_process_node_util(&mut prev, k2, 1_000_000, t0), 0);

    // 2_500_000 * 100ns = 0.25s over 1.0s => 250 permille (25.0%).
    assert_eq!(compute_process_node_util(&mut prev, k1, 7_500_000, t1), 250);

    // Overshoot (>100%) clamps cleanly to 1000 permille.
    assert_eq!(
        compute_process_node_util(&mut prev, k2, 20_000_000, t1),
        1_000
    );

    // Counter reset (running_time drops below previous): returns 0 instead of underflowing.
    assert_eq!(compute_process_node_util(&mut prev, k1, 100_000, t2), 0);

    // Recovery after reset computes accurate delta from new baseline.
    assert_eq!(compute_process_node_util(&mut prev, k1, 5_100_000, t3), 500);

    // Stale-key pruning removes exited processes while keeping live ones.
    let mut live = HashSet::new();
    live.insert(p1);
    prune_stale_process_gpu_nodes(&mut prev, &live);
    assert!(prev.contains_key(&k1));
    assert!(!prev.contains_key(&k2));
}

#[test]
fn process_gpu_probe_state_skips_denied_and_cools_down_non_gpu_with_app_promotion() {
    // Brand-new process always probes all adapters.
    assert_eq!(
        should_probe_process(None, ProcessCategory::Background, 1),
        Some(None)
    );

    // AccessDenied never re-probes for the lifetime of that ProcessKey.
    assert_eq!(
        should_probe_process(
            Some(ProcessGpuProbeState::AccessDenied),
            ProcessCategory::App,
            999,
        ),
        None
    );

    // Non-GPU background process cools down for at least BG_NOGPU_RECHECK_STRIDE ticks.
    let bg_state = next_probe_state_after_scan(0, false, 100, ProcessCategory::Background, 1);
    assert_eq!(
        should_probe_process(Some(bg_state), ProcessCategory::Background, 2),
        None
    );
    // Promoting the same process to ProcessCategory::App triggers an immediate re-probe.
    assert_eq!(
        should_probe_process(Some(bg_state), ProcessCategory::App, 2),
        Some(None)
    );
    // Once cooldown expires, background process re-probes all adapters.
    assert_eq!(
        should_probe_process(
            Some(bg_state),
            ProcessCategory::Background,
            1 + BG_NOGPU_RECHECK_STRIDE * 2,
        ),
        Some(None)
    );

    // Active GPU process probes only its attached adapter bitmask until full_recheck_tick.
    let active_state = next_probe_state_after_scan(0b01, true, 42, ProcessCategory::App, 1);
    assert_eq!(
        should_probe_process(Some(active_state), ProcessCategory::App, 2),
        Some(Some(0b01))
    );
    assert_eq!(
        should_probe_process(
            Some(active_state),
            ProcessCategory::App,
            1 + ACTIVE_FULL_RECHECK_STRIDE * 2,
        ),
        Some(None)
    );
}
