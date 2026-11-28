//! Zero-extra-syscall in-memory resource leak heuristic analyzer.
//!
//! Evaluates live process snapshots on a coarse cadence (default `30 s`) using
//! exclusively metrics already collected by the fast lane (`handle_count`,
//! `cpu_time_100ns`, `ProcessTree`, and cached `exe_path`). Performs zero OS calls.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use loom_types::{LeakKind, LeakWarning, ProcessCategory, ProcessKey, ProcessStatusFlags};

use crate::slow_lane::normalize_exe_path_key;
use crate::tree::FastBuildHasher;

/// Default sampling interval between leak-analyzer evaluation passes (`30 s`).
pub const DEFAULT_LEAK_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// Default sustained observation window before monotonic/orphan leak heuristics trigger (`10 min`).
pub const DEFAULT_LEAK_SUSTAIN_WINDOW: Duration = Duration::from_secs(600);

/// Immediate critical handle-count threshold (excluding PID 0 / PID 4 / kernel processes).
pub const CRITICAL_HANDLE_THRESHOLD: u32 = 100_000;

/// Minimum handle count required before monotonic percentage growth triggers a handle leak.
pub const MONOTONIC_HANDLE_MIN_COUNT: u32 = 1_000;

/// Minimum live direct child count required for a child-process leak warning.
pub const CHILD_LEAK_MIN_COUNT: u32 = 15;

/// Minimum child count growth above the process's baseline floor required for a child leak.
pub const CHILD_LEAK_MIN_DELTA: u32 = 10;

/// Minimum stale orphan instances sharing the same executable required for an orphan leak.
pub const ORPHAN_LEAK_MIN_INSTANCES: usize = 5;

/// Maximum CPU time (`1.0 s` in 100 ns ticks) consumed over the sustain window for an orphan
/// instance to qualify as stale/leaked.
pub const ORPHAN_MAX_CPU_DELTA_100NS: u64 = 10_000_000;

/// Tunable timing thresholds for [`LeakDetector`] (overridable for deterministic unit tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeakDetectorConfig {
    /// Minimum wall time between successive evaluation passes (defaults to `30 s`).
    pub check_interval: Duration,
    /// Rolling sustained observation window (defaults to `10 min`).
    pub sustain_window: Duration,
}

impl Default for LeakDetectorConfig {
    fn default() -> Self {
        Self {
            check_interval: DEFAULT_LEAK_CHECK_INTERVAL,
            sustain_window: DEFAULT_LEAK_SUSTAIN_WINDOW,
        }
    }
}

/// Borrowed per-process input passed to [`LeakDetector::update_at`].
#[derive(Debug, Clone, Copy)]
pub struct LeakSampleInput<'a> {
    /// Composite process lifecycle key.
    pub key: ProcessKey,
    /// Raw executable image name.
    pub name: &'a str,
    /// Optional cached full executable path.
    pub exe_path: Option<&'a str>,
    /// Process grouping category.
    pub category: ProcessCategory,
    /// Effective dynamic process status flags.
    pub status: ProcessStatusFlags,
    /// Whether this process owns a visible top-level window.
    pub has_window: bool,
    /// Whether this process has a live verified parent in the process tree.
    pub has_live_parent: bool,
    /// Current open kernel handle count.
    pub handle_count: u32,
    /// Current number of direct live child processes in the process tree.
    pub child_count: u32,
    /// Cumulative CPU time (kernel + user) in 100 ns ticks.
    pub cpu_time_100ns: u64,
}

#[derive(Debug, Clone)]
struct ProcessLeakHistory {
    /// Rolling `(observed_at, handle_count)` samples covering `sustain_window`.
    handle_samples: VecDeque<(Instant, u32)>,
    /// Lowest observed direct child count before any sustained elevation.
    child_baseline_floor: u32,
    /// Timestamp when `child_count` first rose above `child_baseline_floor` (cleared on return).
    child_elevated_since: Option<Instant>,
    /// `(first_orphan_at, initial_cpu_time_100ns)` while process is a windowless non-system orphan.
    orphan_since: Option<(Instant, u64)>,
}

/// Pure in-memory resource leak heuristic engine updated periodically from fast-lane samples.
#[derive(Debug)]
pub struct LeakDetector {
    config: LeakDetectorConfig,
    last_eval_at: Option<Instant>,
    histories: HashMap<ProcessKey, ProcessLeakHistory, FastBuildHasher>,
    warnings: HashMap<ProcessKey, LeakWarning, FastBuildHasher>,
}

impl Default for LeakDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl LeakDetector {
    /// Creates a [`LeakDetector`] with default production thresholds (`30 s` check, `10 min` window).
    pub fn new() -> Self {
        Self::with_config(LeakDetectorConfig::default())
    }

    /// Creates a [`LeakDetector`] with explicit timing configuration.
    pub fn with_config(config: LeakDetectorConfig) -> Self {
        Self {
            config,
            last_eval_at: None,
            histories: HashMap::with_hasher(FastBuildHasher),
            warnings: HashMap::with_hasher(FastBuildHasher),
        }
    }

    /// Returns the active [`LeakDetectorConfig`].
    #[inline]
    pub const fn config(&self) -> LeakDetectorConfig {
        self.config
    }

    /// Returns the active [`LeakWarning`] for `key`, respecting `show_warnings` and
    /// `ignored_paths` (case-insensitive match against `exe_path` or `image_name`).
    pub fn warning_for(
        &self,
        key: ProcessKey,
        image_name: &str,
        exe_path: Option<&str>,
        show_warnings: bool,
        ignored_paths: &[String],
    ) -> Option<LeakWarning> {
        if !show_warnings {
            return None;
        }
        let warning = self.warnings.get(&key)?;
        if is_leak_path_ignored(image_name, exe_path, ignored_paths) {
            return None;
        }
        Some(warning.clone())
    }

    /// Runs an evaluation pass at `now` if `check_interval` has elapsed since the previous pass.
    ///
    /// Returns `true` if an evaluation pass ran.
    pub fn update_at(&mut self, now: Instant, samples: &[LeakSampleInput<'_>]) -> bool {
        if let Some(last) = self.last_eval_at
            && now.saturating_duration_since(last) < self.config.check_interval
        {
            return false;
        }
        self.last_eval_at = Some(now);

        let mut live_keys: HashSet<ProcessKey, FastBuildHasher> =
            HashSet::with_capacity_and_hasher(samples.len(), FastBuildHasher);
        for s in samples {
            live_keys.insert(s.key);
        }
        self.histories.retain(|k, _| live_keys.contains(k));
        self.warnings.clear();

        let window = self.config.sustain_window;
        let mut stale_orphans_by_exe: HashMap<Arc<str>, Vec<ProcessKey>> = HashMap::new();

        for sample in samples {
            let is_kernel_or_system_core = sample.key.pid == 0
                || sample.key.pid == 4
                || sample
                    .status
                    .intersects(ProcessStatusFlags::KERNEL | ProcessStatusFlags::SYSTEM_IDLE);

            let hist = self
                .histories
                .entry(sample.key)
                .or_insert_with(|| ProcessLeakHistory {
                    handle_samples: VecDeque::with_capacity(24),
                    child_baseline_floor: sample.child_count,
                    child_elevated_since: None,
                    orphan_since: None,
                });

            // 1. Record handle count in rolling window (retain oldest entry spanning >= window).
            hist.handle_samples.push_back((now, sample.handle_count));
            while hist.handle_samples.len() > 2 {
                let next_oldest = hist.handle_samples[1].0;
                if now.saturating_duration_since(next_oldest) >= window {
                    hist.handle_samples.pop_front();
                } else {
                    break;
                }
            }

            // 2. Update child baseline floor and sustained elevation timestamp.
            if sample.child_count <= hist.child_baseline_floor {
                hist.child_baseline_floor = sample.child_count;
                hist.child_elevated_since = None;
            } else if hist.child_elevated_since.is_none() {
                hist.child_elevated_since = Some(now);
            }

            // 3. Update stale orphan tracking for windowless non-system processes with no live parent.
            let is_orphan_candidate = !is_kernel_or_system_core
                && sample.category != ProcessCategory::System
                && sample.category != ProcessCategory::App
                && !sample.has_window
                && !sample.has_live_parent;

            if is_orphan_candidate {
                let (since, initial_cpu) = *hist
                    .orphan_since
                    .get_or_insert((now, sample.cpu_time_100ns));
                let elapsed = now.saturating_duration_since(since);
                let cpu_delta = sample.cpu_time_100ns.saturating_sub(initial_cpu);
                if elapsed >= window && cpu_delta <= ORPHAN_MAX_CPU_DELTA_100NS {
                    let group_key = sample.exe_path.map_or_else(
                        || Arc::from(sample.name.to_ascii_lowercase()),
                        normalize_exe_path_key,
                    );
                    stale_orphans_by_exe
                        .entry(group_key)
                        .or_default()
                        .push(sample.key);
                }
            } else {
                hist.orphan_since = None;
            }

            if is_kernel_or_system_core {
                continue;
            }

            // Evaluate Handle Leak heuristic first (highest severity).
            if let Some(warning) =
                evaluate_handle_leak(sample.handle_count, &hist.handle_samples, now, window)
            {
                self.warnings.insert(sample.key, warning);
                continue;
            }

            // Evaluate Children Leak heuristic next.
            if let Some(warning) = evaluate_children_leak(
                sample.child_count,
                hist.child_baseline_floor,
                hist.child_elevated_since,
                now,
                window,
            ) {
                self.warnings.insert(sample.key, warning);
            }
        }

        // Evaluate Orphans Leak heuristic across grouped stale orphans.
        for (_exe_key, keys) in stale_orphans_by_exe {
            if keys.len() >= ORPHAN_LEAK_MIN_INSTANCES {
                let count = keys.len() as u64;
                let summary: Arc<str> = Arc::from(format!(
                    "{count} idle orphaned instances without a live parent"
                ));
                for key in keys {
                    self.warnings.entry(key).or_insert_with(|| LeakWarning {
                        kind: LeakKind::Orphans,
                        summary: Arc::clone(&summary),
                        current_value: count,
                        baseline_value: ORPHAN_LEAK_MIN_INSTANCES as u64,
                    });
                }
            }
        }

        true
    }
}

fn evaluate_handle_leak(
    current: u32,
    samples: &VecDeque<(Instant, u32)>,
    now: Instant,
    window: Duration,
) -> Option<LeakWarning> {
    let floor = samples.iter().map(|&(_, h)| h).min().unwrap_or(current);

    if current >= CRITICAL_HANDLE_THRESHOLD {
        return Some(LeakWarning {
            kind: LeakKind::Handles,
            summary: Arc::from(format!("Critical handle count ({current} open handles)")),
            current_value: u64::from(current),
            baseline_value: u64::from(floor),
        });
    }

    if current < MONOTONIC_HANDLE_MIN_COUNT || samples.len() < 2 {
        return None;
    }

    let oldest_ts = samples.front()?.0;
    if now.saturating_duration_since(oldest_ts) < window {
        return None;
    }

    if floor == 0 {
        return None;
    }

    // Require monotonic non-decreasing growth across the rolling window and >= 150% of floor.
    let mut prev = 0u32;
    for &(_, count) in samples {
        if count < prev {
            return None;
        }
        prev = count;
    }

    if u64::from(current).saturating_mul(2) >= u64::from(floor).saturating_mul(3) {
        Some(LeakWarning {
            kind: LeakKind::Handles,
            summary: Arc::from(format!(
                "Handles grew monotonically from {floor} to {current}"
            )),
            current_value: u64::from(current),
            baseline_value: u64::from(floor),
        })
    } else {
        None
    }
}

fn evaluate_children_leak(
    current_children: u32,
    baseline_floor: u32,
    elevated_since: Option<Instant>,
    now: Instant,
    window: Duration,
) -> Option<LeakWarning> {
    if current_children < CHILD_LEAK_MIN_COUNT {
        return None;
    }
    if current_children < baseline_floor.saturating_add(CHILD_LEAK_MIN_DELTA) {
        return None;
    }
    let since = elevated_since?;
    if now.saturating_duration_since(since) < window {
        return None;
    }

    Some(LeakWarning {
        kind: LeakKind::Children,
        summary: Arc::from(format!(
            "Sustained child process growth ({current_children} live children, baseline {baseline_floor})"
        )),
        current_value: u64::from(current_children),
        baseline_value: u64::from(baseline_floor),
    })
}

/// Returns `true` if `image_name` or `exe_path` matches any entry in `ignored_paths`
/// (case-insensitive normalized path or image filename match).
pub fn is_leak_path_ignored(
    image_name: &str,
    exe_path: Option<&str>,
    ignored_paths: &[String],
) -> bool {
    if ignored_paths.is_empty() {
        return false;
    }
    let norm_exe = exe_path.map(normalize_exe_path_key);
    for raw in ignored_paths {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if image_name.eq_ignore_ascii_case(trimmed) {
            return true;
        }
        if let Some(ref exe) = norm_exe {
            let norm_ignored = normalize_exe_path_key(trimmed);
            if exe.as_ref() == norm_ignored.as_ref() {
                return true;
            }
            if let Some(basename) = exe.rsplit(['\\', '/']).next()
                && basename.eq_ignore_ascii_case(trimmed)
            {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_input<'a>(
        pid: u32,
        name: &'a str,
        exe_path: Option<&'a str>,
        handle_count: u32,
        child_count: u32,
        has_live_parent: bool,
        cpu_time_100ns: u64,
    ) -> LeakSampleInput<'a> {
        LeakSampleInput {
            key: ProcessKey::new(pid, 100),
            name,
            exe_path,
            category: ProcessCategory::Background,
            status: ProcessStatusFlags::empty(),
            has_window: false,
            has_live_parent,
            handle_count,
            child_count,
            cpu_time_100ns,
        }
    }

    #[test]
    fn critical_handle_threshold_triggers_immediately_except_kernel_pids() {
        let mut detector = LeakDetector::new();
        let t0 = Instant::now();
        let samples = [
            mk_input(0, "System Idle Process", None, 150_000, 0, false, 0),
            mk_input(4, "System", None, 150_000, 0, false, 0),
            mk_input(
                100,
                "leaker.exe",
                Some(r"C:\Apps\leaker.exe"),
                100_000,
                0,
                true,
                0,
            ),
        ];

        assert!(detector.update_at(t0, &samples));
        assert!(
            detector
                .warning_for(
                    ProcessKey::new(0, 100),
                    "System Idle Process",
                    None,
                    true,
                    &[]
                )
                .is_none()
        );
        assert!(
            detector
                .warning_for(ProcessKey::new(4, 100), "System", None, true, &[])
                .is_none()
        );

        let warn = detector
            .warning_for(
                ProcessKey::new(100, 100),
                "leaker.exe",
                Some(r"C:\Apps\leaker.exe"),
                true,
                &[],
            )
            .expect("100k handles must trigger LeakKind::Handles");
        assert_eq!(warn.kind, LeakKind::Handles);
        assert_eq!(warn.current_value, 100_000);

        // Suppressed when show_warnings == false or path is ignored.
        assert!(
            detector
                .warning_for(
                    ProcessKey::new(100, 100),
                    "leaker.exe",
                    Some(r"C:\Apps\leaker.exe"),
                    false,
                    &[]
                )
                .is_none()
        );
        assert!(
            detector
                .warning_for(
                    ProcessKey::new(100, 100),
                    "leaker.exe",
                    Some(r"C:\Apps\leaker.exe"),
                    true,
                    &["c:/apps/leaker.exe".to_owned()]
                )
                .is_none()
        );
    }

    #[test]
    fn monotonic_handle_growth_triggers_after_sustain_window_and_rejects_spikes() {
        let mut detector = LeakDetector::with_config(LeakDetectorConfig {
            check_interval: Duration::from_secs(30),
            sustain_window: Duration::from_secs(600),
        });
        let t0 = Instant::now();
        let leak_key = ProcessKey::new(200, 100);
        let spike_key = ProcessKey::new(201, 100);

        // Simulate 21 samples spaced 30s apart (0s..=600s).
        for step in 0..=20u32 {
            let now = t0 + Duration::from_secs(u64::from(step) * 30);
            let leak_handles = 1_000 + step * 30; // 1000 -> 1600 (160% monotonic)
            let spike_handles = if step == 10 { 2_000 } else { 1_000 + step * 30 };
            let samples = [
                mk_input(200, "grower.exe", None, leak_handles, 0, true, 0),
                mk_input(201, "spiker.exe", None, spike_handles, 0, true, 0),
            ];
            assert!(detector.update_at(now, &samples));
            if step < 20 {
                assert!(
                    detector
                        .warning_for(leak_key, "grower.exe", None, true, &[])
                        .is_none(),
                    "must not trigger before 10 min window at step {step}"
                );
            }
        }

        let warn = detector
            .warning_for(leak_key, "grower.exe", None, true, &[])
            .expect("monotonic 160% handle growth over 10m must trigger");
        assert_eq!(warn.kind, LeakKind::Handles);
        assert_eq!(warn.baseline_value, 1_000);
        assert_eq!(warn.current_value, 1_600);

        // Non-monotonic spike (dropped after step 10) must not trigger.
        assert!(
            detector
                .warning_for(spike_key, "spiker.exe", None, true, &[])
                .is_none()
        );
    }

    #[test]
    fn children_leak_triggers_after_sustained_growth_above_floor() {
        let mut detector = LeakDetector::with_config(LeakDetectorConfig {
            check_interval: Duration::from_secs(30),
            sustain_window: Duration::from_secs(600),
        });
        let t0 = Instant::now();
        let key = ProcessKey::new(300, 100);

        // Baseline: 2 children at t=0.
        detector.update_at(t0, &[mk_input(300, "spawner.exe", None, 200, 2, true, 0)]);
        // Starts growing at t=30s (5 children) and reaches 16 children by t=630s (>= 10m elevated).
        detector.update_at(
            t0 + Duration::from_secs(30),
            &[mk_input(300, "spawner.exe", None, 200, 5, true, 0)],
        );
        assert!(
            detector
                .warning_for(key, "spawner.exe", None, true, &[])
                .is_none()
        );

        detector.update_at(
            t0 + Duration::from_secs(630),
            &[mk_input(300, "spawner.exe", None, 200, 16, true, 0)],
        );
        let warn = detector
            .warning_for(key, "spawner.exe", None, true, &[])
            .expect("sustained >=15 children and >=floor+10 over 10m must trigger");
        assert_eq!(warn.kind, LeakKind::Children);
        assert_eq!(warn.baseline_value, 2);
        assert_eq!(warn.current_value, 16);
    }

    #[test]
    fn orphan_leak_triggers_for_five_idle_windowless_orphans_over_window() {
        let mut detector = LeakDetector::with_config(LeakDetectorConfig {
            check_interval: Duration::from_secs(30),
            sustain_window: Duration::from_secs(600),
        });
        let t0 = Instant::now();

        let make_orphans = |cpu_ticks: u64| -> Vec<LeakSampleInput<'static>> {
            (0..5u32)
                .map(|i| {
                    mk_input(
                        400 + i,
                        "worker.exe",
                        Some(r"C:\Tools\worker.exe"),
                        120,
                        0,
                        false,
                        cpu_ticks,
                    )
                })
                .collect()
        };

        detector.update_at(t0, &make_orphans(5_000_000));
        assert!(
            detector
                .warning_for(
                    ProcessKey::new(400, 100),
                    "worker.exe",
                    Some(r"C:\Tools\worker.exe"),
                    true,
                    &[]
                )
                .is_none()
        );

        // After 10 min with <= 1s (10_000_000 ticks) CPU used, all 5 orphans trigger LeakKind::Orphans.
        detector.update_at(t0 + Duration::from_secs(600), &make_orphans(10_000_000));
        for i in 0..5u32 {
            let warn = detector
                .warning_for(
                    ProcessKey::new(400 + i, 100),
                    "worker.exe",
                    Some(r"C:\Tools\worker.exe"),
                    true,
                    &[],
                )
                .expect("idle orphan group of 5 must trigger LeakKind::Orphans");
            assert_eq!(warn.kind, LeakKind::Orphans);
            assert_eq!(warn.current_value, 5);
        }
    }
}
