use windows_sys::Win32::System::WindowsProgramming::SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION;

use super::cpu_cycles::CpuCycleAccountant;

fn perf_entry(idle: i64, kernel_only: i64, user: i64) -> SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION {
    SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION {
        IdleTime: idle,
        KernelTime: idle + kernel_only,
        UserTime: user,
        ..SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION::default()
    }
}

#[test]
fn cpu_cycle_accountant_computes_idle_cycle_busy_and_kernel_split() {
    let mut acc = CpuCycleAccountant::with_capacity(2, 1);
    let qpc_freq = 10_000_000u64; // 10 MHz QPC
    let rate_hz = 2_000_000_000u64; // 2.0 GHz TSC/cycle rate
    let mut per_core = Vec::new();

    // Cold sample establishes baseline and returns (0, 0).
    let p0 = [perf_entry(0, 0, 0), perf_entry(0, 0, 0)];
    let c0 = [1_000_000_000u64, 1_000_000_000u64];
    assert_eq!(
        acc.compute_delta(&p0, Some(&c0), &[2], Some(0), qpc_freq, Some(&mut per_core)),
        (0, 0)
    );
    assert_eq!(per_core, vec![0, 0]);

    // Tick 1 (1.0s): core 0 is 100% idle (calibrates 2.0 GHz exact rate),
    // core 1 is 50% busy (50% idle cycles = 1.0e9), with half of busy time in kernel.
    let p1 = [
        perf_entry(10_000_000, 0, 0),
        perf_entry(5_000_000, 2_500_000, 2_500_000),
    ];
    let c1 = [c0[0] + rate_hz, c0[1] + rate_hz / 2];
    let (total, kernel) = acc.compute_delta(
        &p1,
        Some(&c1),
        &[2],
        Some(qpc_freq as i64),
        qpc_freq,
        Some(&mut per_core),
    );
    assert_eq!(per_core, vec![0, 500]);
    assert_eq!(total, 250);
    assert_eq!(kernel, 125);
}

#[test]
fn cpu_cycle_accountant_falls_back_cleanly_on_anomaly_or_missing_cycles() {
    let mut acc = CpuCycleAccountant::with_capacity(2, 1);
    let qpc_freq = 10_000_000u64;
    let mut per_core = Vec::new();

    let p0 = [perf_entry(0, 0, 0), perf_entry(0, 0, 0)];
    let c0 = [5_000_000_000u64, 5_000_000_000u64];
    assert_eq!(
        acc.compute_delta(&p0, Some(&c0), &[2], Some(0), qpc_freq, Some(&mut per_core)),
        (0, 0)
    );

    // Counter underflow (idle cycles drop below previous): must fall back to
    // SystemProcessorPerformanceInformation (core 0 = 20% busy, core 1 = 60% busy => 40% total, 10% kernel).
    let p1 = [
        perf_entry(8_000_000, 1_000_000, 1_000_000),
        perf_entry(4_000_000, 1_000_000, 5_000_000),
    ];
    let c1_underflow = [100u64, 100u64];
    let (total, kernel) = acc.compute_delta(
        &p1,
        Some(&c1_underflow),
        &[2],
        Some(qpc_freq as i64),
        qpc_freq,
        Some(&mut per_core),
    );
    assert_eq!(per_core, vec![200, 600]);
    assert_eq!(total, 400);
    assert_eq!(kernel, 100);

    // Missing idle cycles (`None`) also falls back cleanly (cumulative counters:
    // core 0: +10M idle, 0 busy -> 0%; core 1: +5M idle, +2M kernel, +3M user -> 50% busy, 20% kernel).
    let p2 = [
        perf_entry(18_000_000, 1_000_000, 1_000_000),
        perf_entry(9_000_000, 3_000_000, 8_000_000),
    ];
    let (total2, kernel2) = acc.compute_delta(
        &p2,
        None,
        &[2],
        Some((qpc_freq * 2) as i64),
        qpc_freq,
        Some(&mut per_core),
    );
    assert_eq!(per_core, vec![0, 500]);
    assert_eq!(total2, 250);
    assert_eq!(kernel2, 100);
}

#[test]
fn cpu_cycle_accountant_supports_multiple_processor_groups() {
    let mut acc = CpuCycleAccountant::with_capacity(2, 2);
    let qpc_freq = 10_000_000u64;
    let mut per_core = Vec::new();

    let p0 = [perf_entry(0, 0, 0), perf_entry(0, 0, 0)];
    let c0 = [0u64, 0u64];
    acc.compute_delta(
        &p0,
        Some(&c0),
        &[1, 1],
        Some(0),
        qpc_freq,
        Some(&mut per_core),
    );

    // Group 0 runs at 3.0 GHz (idle), Group 1 runs at 2.0 GHz (idle) -> calibrate both groups.
    let p1 = [perf_entry(10_000_000, 0, 0), perf_entry(10_000_000, 0, 0)];
    let c1 = [3_000_000_000u64, 2_000_000_000u64];
    let (t1, _) = acc.compute_delta(
        &p1,
        Some(&c1),
        &[1, 1],
        Some(qpc_freq as i64),
        qpc_freq,
        Some(&mut per_core),
    );
    assert_eq!(t1, 0);
    assert_eq!(per_core, vec![0, 0]);

    // Tick 2: Group 0 is 40% busy (1.8e9 idle), Group 1 is 20% busy (1.6e9 idle).
    let p2 = [
        perf_entry(16_000_000, 1_000_000, 3_000_000),
        perf_entry(18_000_000, 500_000, 1_500_000),
    ];
    let c2 = [c1[0] + 1_800_000_000, c1[1] + 1_600_000_000];
    let (t2, k2) = acc.compute_delta(
        &p2,
        Some(&c2),
        &[1, 1],
        Some((qpc_freq * 2) as i64),
        qpc_freq,
        Some(&mut per_core),
    );
    assert_eq!(per_core, vec![400, 200]);
    assert_eq!(t2, 320); // (1.2e9 + 0.4e9) / (3.0e9 + 2.0e9) = 1.6 / 5.0 = 320 permille
    assert!(k2 <= t2);
}
