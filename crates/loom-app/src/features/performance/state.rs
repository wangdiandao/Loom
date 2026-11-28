//! Pure state model and deterministic layout/fallback logic for the Performance slice.

use std::collections::HashMap;

use loom_types::{DiskDeviceSlice, GpuEngineSlice, HistoryRing, NetAdapterSlice, PerformanceSlice};
use loom_ui_kit::{
    CoreGridLayout, compute_core_grid_layout, merge_stable_order, select_active_gpu_engines,
};

/// Currently selected hardware device in the Performance sidebar.
///
/// Anchored to stable hardware identifiers (`DiskDeviceSlice::index`,
/// `NetAdapterSlice::luid`, `GpuAdapterSlice::luid`) so hot-plugging or device
/// reordering never shifts selection onto an unrelated device.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SelectedDevice {
    /// System processor view.
    #[default]
    Cpu,
    /// Physical and virtual memory view.
    Memory,
    /// Physical disk identified by OS disk index (`Disk 0..N`).
    Disk(u32),
    /// Network adapter identified by OS interface LUID.
    Network(u64),
    /// Graphics adapter identified by OS adapter LUID.
    Gpu(u64),
}

impl SelectedDevice {
    /// Validates `self` against `perf`, falling back to [`SelectedDevice::Cpu`]
    /// if the previously selected dynamic device (Disk, Network, or GPU) is no
    /// longer present.
    pub(crate) fn resolve(self, perf: &PerformanceSlice) -> Self {
        match self {
            Self::Cpu | Self::Memory => self,
            Self::Disk(index) => {
                if perf.disks.iter().any(|d| d.index == index) {
                    self
                } else {
                    Self::Cpu
                }
            }
            Self::Network(luid) => {
                if perf.networks.iter().any(|n| n.luid == luid) {
                    self
                } else {
                    Self::Cpu
                }
            }
            Self::Gpu(luid) => {
                if perf.gpus.iter().any(|g| g.luid == luid) {
                    self
                } else {
                    Self::Cpu
                }
            }
        }
    }
}

/// CPU graph display mode in the Performance detail pane.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CpuGraphMode {
    /// Single 60-second system-wide utilization chart.
    #[default]
    Overall,
    /// Responsive grid of per-logical-processor 60-second charts
    /// (dynamically acquires `MonitorDomain::PerCoreCpu`).
    LogicalProcessors,
}

/// Maximum number of GPU engine charts selectable in the GPU detail pane.
pub(crate) const MAX_GPU_ENGINE_PANES: usize = 8;

/// Default number of GPU engine charts shown when an adapter exposes at least 4 engines.
pub(crate) const DEFAULT_GPU_ENGINE_COUNT: usize = 4;

/// Returns the maximum selectable GPU engine pane count (`0` when no engines exist,
/// otherwise clamped to `1..=MAX_GPU_ENGINE_PANES`).
#[inline]
pub(crate) fn max_gpu_engine_panes(engine_count: usize) -> usize {
    engine_count.min(MAX_GPU_ENGINE_PANES)
}

/// Clamps a desired GPU engine pane count against an adapter's available engine count.
#[inline]
pub(crate) fn effective_gpu_engine_count(desired: usize, engine_count: usize) -> usize {
    let max_panes = max_gpu_engine_panes(engine_count);
    if max_panes == 0 {
        0
    } else {
        desired.clamp(1, max_panes)
    }
}

/// Computes the peak non-gap permille utilization for a GPU engine across its
/// active 60-sample history window and current sample.
pub(crate) fn gpu_engine_peak_permille(engine: &GpuEngineSlice) -> u16 {
    let history_peak = engine
        .history
        .iter_ordered()
        .filter(|&v| !HistoryRing::<60>::is_gap(v))
        .max()
        .unwrap_or(0);
    let current = if HistoryRing::<60>::is_gap(engine.util_permille) {
        0
    } else {
        engine.util_permille
    };
    history_peak.max(current)
}

/// Selects up to [`MAX_GPU_ENGINE_PANES`] preferred default engine indices by well-known
/// engine name (`3D`, `Copy`, `Video Decode`, `Video Encode`, `Compute`, etc.), filling
/// remaining slots with distinct engines before cycling when fewer engines exist.
pub(crate) fn default_gpu_engine_indices(
    engines: &[GpuEngineSlice],
) -> [usize; MAX_GPU_ENGINE_PANES] {
    if engines.is_empty() {
        return [0; MAX_GPU_ENGINE_PANES];
    }

    const PREFERRED: [&str; MAX_GPU_ENGINE_PANES] = [
        "3d",
        "copy",
        "video decode",
        "video encode",
        "compute",
        "video processing",
        "high priority 3d",
        "vr",
    ];
    let mut chosen: Vec<usize> = Vec::with_capacity(MAX_GPU_ENGINE_PANES);

    for target in PREFERRED {
        let exact = engines
            .iter()
            .enumerate()
            .find(|(idx, eng)| {
                !chosen.contains(idx) && eng.name.trim().eq_ignore_ascii_case(target)
            })
            .map(|(idx, _)| idx);

        let matched = exact.or_else(|| {
            engines
                .iter()
                .enumerate()
                .find(|(idx, eng)| {
                    !chosen.contains(idx) && eng.name.to_ascii_lowercase().contains(target)
                })
                .map(|(idx, _)| idx)
        });

        if let Some(idx) = matched {
            chosen.push(idx);
        }
    }

    for idx in 0..engines.len() {
        if chosen.len() >= MAX_GPU_ENGINE_PANES {
            break;
        }
        if !chosen.contains(&idx) {
            chosen.push(idx);
        }
    }

    let mut result = [0usize; MAX_GPU_ENGINE_PANES];
    for (slot, out) in result.iter_mut().enumerate() {
        *out = chosen
            .get(slot)
            .copied()
            .unwrap_or_else(|| slot % engines.len());
    }
    result
}

/// Computes smart GPU engine indices prioritizing active engines that reached `>= 1%`
/// peak in the active window via [`select_active_gpu_engines`] (always keeping at
/// least 1 engine), then filling any remaining slots with distinct preferred defaults.
pub(crate) fn active_gpu_engine_indices(
    engines: &[GpuEngineSlice],
) -> [usize; MAX_GPU_ENGINE_PANES] {
    if engines.is_empty() {
        return [0; MAX_GPU_ENGINE_PANES];
    }

    let peaks: Vec<(usize, u16)> = engines
        .iter()
        .enumerate()
        .map(|(idx, eng)| (idx, gpu_engine_peak_permille(eng)))
        .collect();
    let active = select_active_gpu_engines(&peaks, MAX_GPU_ENGINE_PANES);
    let preferred = default_gpu_engine_indices(engines);

    let mut chosen: Vec<usize> = Vec::with_capacity(MAX_GPU_ENGINE_PANES);
    let has_active_above_threshold = peaks
        .iter()
        .any(|&(_, peak)| peak >= loom_ui_kit::GPU_ENGINE_ACTIVE_THRESHOLD_PERMILLE);

    if has_active_above_threshold {
        for &idx in &active {
            if !chosen.contains(&idx) {
                chosen.push(idx);
            }
        }
    } else if let Some(&fallback_idx) = active.first()
        && peaks.get(fallback_idx).is_some_and(|&(_, peak)| peak > 0)
    {
        chosen.push(fallback_idx);
    }

    for idx in preferred {
        if chosen.len() >= MAX_GPU_ENGINE_PANES {
            break;
        }
        if !chosen.contains(&idx) {
            chosen.push(idx);
        }
    }

    // Always preserve at least the active fallback if nothing was chosen yet.
    if chosen.is_empty()
        && let Some(&fallback_idx) = active.first()
    {
        chosen.push(fallback_idx);
    }

    let mut result = [0usize; MAX_GPU_ENGINE_PANES];
    for (slot, out) in result.iter_mut().enumerate() {
        *out = chosen
            .get(slot)
            .copied()
            .unwrap_or_else(|| slot % engines.len());
    }
    result
}

/// Feature-local UI state owned by [`super::PerformanceView`].
#[derive(Debug)]
pub(crate) struct PerformanceViewState {
    pub(crate) selected_device: SelectedDevice,
    pub(crate) cpu_graph_mode: CpuGraphMode,
    pub(crate) show_kernel_times: bool,
    pub(crate) gpu_engine_count: usize,
    /// Last computed logical-core grid column count persisted across frames so
    /// [`compute_core_grid_layout`] applies its `8%` resize hysteresis dead-band.
    pub(crate) prev_core_grid_columns: Option<usize>,
    disk_pinned_order: Vec<u32>,
    network_pinned_order: Vec<u64>,
    gpu_engine_indices: HashMap<u64, [usize; MAX_GPU_ENGINE_PANES]>,
    gpu_engine_user_overrides: HashMap<u64, [Option<usize>; MAX_GPU_ENGINE_PANES]>,
}

impl Default for PerformanceViewState {
    fn default() -> Self {
        Self {
            selected_device: SelectedDevice::default(),
            cpu_graph_mode: CpuGraphMode::default(),
            show_kernel_times: false,
            gpu_engine_count: DEFAULT_GPU_ENGINE_COUNT,
            prev_core_grid_columns: None,
            disk_pinned_order: Vec::new(),
            network_pinned_order: Vec::new(),
            gpu_engine_indices: HashMap::new(),
            gpu_engine_user_overrides: HashMap::new(),
        }
    }
}

impl PerformanceViewState {
    /// Ensures `selected_device` still exists in `perf`, falling back to CPU if removed.
    ///
    /// Returns `true` if the active selection changed.
    pub(crate) fn reconcile_selection(&mut self, perf: &PerformanceSlice) -> bool {
        let resolved = self.selected_device.resolve(perf);
        if resolved != self.selected_device {
            self.selected_device = resolved;
            true
        } else {
            false
        }
    }

    /// Computes the adaptive logical-core grid layout and persists `prev_core_grid_columns`
    /// for smooth `8%` resize hysteresis.
    pub(crate) fn compute_core_grid(
        &mut self,
        core_count: usize,
        avail_width_px: f32,
        avail_height_px: f32,
    ) -> CoreGridLayout {
        let layout = compute_core_grid_layout(
            core_count,
            avail_width_px,
            avail_height_px,
            self.prev_core_grid_columns,
        );
        self.prev_core_grid_columns = (layout.columns > 0).then_some(layout.columns);
        layout
    }

    /// Returns index order into `disks` preserving the open-time adapter order
    /// via [`merge_stable_order`].
    pub(crate) fn ordered_disk_indices(&mut self, disks: &[DiskDeviceSlice]) -> Vec<usize> {
        let keys: Vec<u32> = disks.iter().map(|d| d.index).collect();
        merge_stable_order(&mut self.disk_pinned_order, &keys)
    }

    /// Returns index order into `networks` preserving the open-time adapter order
    /// via [`merge_stable_order`].
    pub(crate) fn ordered_network_indices(&mut self, networks: &[NetAdapterSlice]) -> Vec<usize> {
        let keys: Vec<u64> = networks.iter().map(|n| n.luid).collect();
        merge_stable_order(&mut self.network_pinned_order, &keys)
    }

    /// Updates the desired number of visible GPU engine charts (`1..=MAX_GPU_ENGINE_PANES`).
    ///
    /// Returns `true` if the desired count changed.
    pub(crate) fn set_gpu_engine_count(&mut self, count: usize) -> bool {
        let clamped = count.clamp(1, MAX_GPU_ENGINE_PANES);
        if self.gpu_engine_count == clamped {
            return false;
        }
        self.gpu_engine_count = clamped;
        true
    }

    /// Returns (and updates using active engine selection while preserving any
    /// explicit user `Select` overrides) the engine indices for GPU `luid`.
    pub(crate) fn ensure_gpu_engine_indices(
        &mut self,
        luid: u64,
        engines: &[GpuEngineSlice],
    ) -> [usize; MAX_GPU_ENGINE_PANES] {
        let len = engines.len();
        if len == 0 {
            self.gpu_engine_indices
                .insert(luid, [0; MAX_GPU_ENGINE_PANES]);
            return [0; MAX_GPU_ENGINE_PANES];
        }

        let auto = active_gpu_engine_indices(engines);
        let overrides = self
            .gpu_engine_user_overrides
            .get(&luid)
            .copied()
            .unwrap_or([None; MAX_GPU_ENGINE_PANES]);

        let mut resolved = [0usize; MAX_GPU_ENGINE_PANES];
        for slot in 0..MAX_GPU_ENGINE_PANES {
            resolved[slot] = match overrides[slot] {
                Some(idx) if idx < len => idx,
                _ => auto[slot],
            };
        }

        self.gpu_engine_indices.insert(luid, resolved);
        resolved
    }

    /// Records an explicit user override for a single pane's selected GPU engine index.
    pub(crate) fn set_gpu_engine_slot(
        &mut self,
        luid: u64,
        pane_slot: usize,
        engine_index: usize,
        engines: &[GpuEngineSlice],
    ) {
        if pane_slot >= MAX_GPU_ENGINE_PANES || engine_index >= engines.len() {
            return;
        }
        let overrides = self
            .gpu_engine_user_overrides
            .entry(luid)
            .or_insert([None; MAX_GPU_ENGINE_PANES]);
        overrides[pane_slot] = Some(engine_index);

        let mut current = self.ensure_gpu_engine_indices(luid, engines);
        current[pane_slot] = engine_index;
        self.gpu_engine_indices.insert(luid, current);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use loom_types::{
        DiskDeviceSlice, GpuAdapterSlice, GpuEngineSlice, HistoryRing, NetAdapterSlice,
        PerformanceSlice,
    };

    use super::*;

    fn engine(name: &str) -> GpuEngineSlice {
        GpuEngineSlice {
            name: Arc::from(name),
            ..Default::default()
        }
    }

    fn engine_with_peak(name: &str, peak_permille: u16) -> GpuEngineSlice {
        let mut history = HistoryRing::<60>::new();
        history.push(peak_permille);
        GpuEngineSlice {
            name: Arc::from(name),
            util_permille: peak_permille,
            history,
        }
    }

    #[test]
    fn compute_core_grid_persists_prev_columns_with_hysteresis() {
        let mut state = PerformanceViewState::default();
        assert_eq!(state.prev_core_grid_columns, None);

        let initial = state.compute_core_grid(16, 1000.0, 600.0);
        assert_eq!(initial.columns, 4);
        assert_eq!(initial.rows, 4);
        assert_eq!(state.prev_core_grid_columns, Some(4));

        // Zero cores resets cleanly without panicking.
        let empty = state.compute_core_grid(0, 1000.0, 600.0);
        assert_eq!(empty.columns, 0);
        assert_eq!(state.prev_core_grid_columns, None);
    }

    #[test]
    fn default_gpu_engine_selection_prefers_distinct_standard_engines() {
        let engines = vec![
            engine("Compute_0"),
            engine("Video Encode"),
            engine("3D"),
            engine("Copy"),
            engine("Video Decode"),
            engine("Copy 1"),
        ];
        // Preferred order: 3D (2), Copy (3), Video Decode (4), Video Encode (1), Compute_0 (0), Copy 1 (5), then wraps.
        assert_eq!(
            default_gpu_engine_indices(&engines),
            [2, 3, 4, 1, 0, 5, 0, 1]
        );

        // Partial engine list fills distinct remaining engines and wraps cleanly when < MAX_GPU_ENGINE_PANES.
        let two_engines = vec![engine("Copy"), engine("3D")];
        assert_eq!(
            default_gpu_engine_indices(&two_engines),
            [1, 0, 0, 1, 0, 1, 0, 1]
        );

        // Empty engine list safely yields zeros.
        assert_eq!(default_gpu_engine_indices(&[]), [0; MAX_GPU_ENGINE_PANES]);
    }

    #[test]
    fn gpu_engine_count_clamping_and_defaults() {
        assert_eq!(max_gpu_engine_panes(0), 0);
        assert_eq!(max_gpu_engine_panes(1), 1);
        assert_eq!(max_gpu_engine_panes(6), 6);
        assert_eq!(max_gpu_engine_panes(12), MAX_GPU_ENGINE_PANES);

        assert_eq!(effective_gpu_engine_count(4, 0), 0);
        assert_eq!(effective_gpu_engine_count(4, 2), 2);
        assert_eq!(effective_gpu_engine_count(4, 10), 4);
        assert_eq!(effective_gpu_engine_count(8, 10), 8);
        assert_eq!(effective_gpu_engine_count(0, 5), 1);

        let mut state = PerformanceViewState::default();
        assert_eq!(state.gpu_engine_count, DEFAULT_GPU_ENGINE_COUNT);
        assert!(!state.set_gpu_engine_count(4));
        assert!(state.set_gpu_engine_count(1));
        assert_eq!(state.gpu_engine_count, 1);
        assert!(state.set_gpu_engine_count(99));
        assert_eq!(state.gpu_engine_count, MAX_GPU_ENGINE_PANES);
    }

    #[test]
    fn active_gpu_engine_selection_prioritizes_active_and_respects_user_overrides() {
        let mut gap_only = GpuEngineSlice {
            name: Arc::from("Compute_0"),
            util_permille: HistoryRing::<60>::GAP_SENTINEL,
            ..Default::default()
        };
        gap_only.history.push_gap();

        let engines = vec![
            gap_only,
            engine_with_peak("Video Encode", 5),   // < 1% idle
            engine_with_peak("3D", 180),           // 18.0% active
            engine_with_peak("Copy", 0),           // idle
            engine_with_peak("Video Decode", 420), // 42.0% active
            engine_with_peak("Compute_1", 95),     // 9.5% active
        ];

        // Active >= 1% engines sorted by peak desc: Video Decode (4), 3D (2), Compute_1 (5),
        // then preferred distinct defaults: Copy (3), Video Encode (1), Compute_0 (0), then wrap.
        let mut state = PerformanceViewState::default();
        assert_eq!(
            &state.ensure_gpu_engine_indices(77, &engines)[..6],
            &[4, 2, 5, 3, 1, 0]
        );

        // User explicitly overrides slot 0 to Copy (3) and slot 5 to 3D (2) -> preserved on subsequent ticks!
        state.set_gpu_engine_slot(77, 0, 3, &engines);
        state.set_gpu_engine_slot(77, 5, 2, &engines);
        assert_eq!(
            &state.ensure_gpu_engine_indices(77, &engines)[..6],
            &[3, 2, 5, 3, 1, 2]
        );
    }

    #[test]
    fn stable_adapter_ordering_preserves_open_time_disk_and_network_order() {
        let mut state = PerformanceViewState::default();

        let disks_open = vec![
            DiskDeviceSlice {
                index: 0,
                ..Default::default()
            },
            DiskDeviceSlice {
                index: 1,
                ..Default::default()
            },
        ];
        assert_eq!(state.ordered_disk_indices(&disks_open), vec![0, 1]);

        // OS reorders disks and hot-plugs Disk 2 at the front -> open-time order [0, 1] stays pinned, 2 appended.
        let disks_reordered = vec![
            DiskDeviceSlice {
                index: 2,
                ..Default::default()
            },
            DiskDeviceSlice {
                index: 1,
                ..Default::default()
            },
            DiskDeviceSlice {
                index: 0,
                ..Default::default()
            },
        ];
        assert_eq!(state.ordered_disk_indices(&disks_reordered), vec![2, 1, 0]);

        let nets_open = vec![
            NetAdapterSlice {
                luid: 100,
                ..Default::default()
            },
            NetAdapterSlice {
                luid: 200,
                ..Default::default()
            },
        ];
        assert_eq!(state.ordered_network_indices(&nets_open), vec![0, 1]);

        let nets_reordered = vec![
            NetAdapterSlice {
                luid: 200,
                ..Default::default()
            },
            NetAdapterSlice {
                luid: 300,
                ..Default::default()
            },
            NetAdapterSlice {
                luid: 100,
                ..Default::default()
            },
        ];
        assert_eq!(
            state.ordered_network_indices(&nets_reordered),
            vec![2, 0, 1]
        );
    }

    #[test]
    fn selected_device_falls_back_to_cpu_when_device_disappears() {
        let mut perf = PerformanceSlice::default();
        perf.disks.push(DiskDeviceSlice {
            index: 0,
            ..Default::default()
        });
        perf.networks.push(NetAdapterSlice {
            luid: 1001,
            ..Default::default()
        });
        perf.gpus.push(GpuAdapterSlice {
            index: 0,
            luid: 2002,
            ..Default::default()
        });

        let mut state = PerformanceViewState {
            selected_device: SelectedDevice::Disk(0),
            ..Default::default()
        };
        assert!(!state.reconcile_selection(&perf));
        assert_eq!(state.selected_device, SelectedDevice::Disk(0));

        // Remove disk 0 -> falls back to CPU.
        perf.disks.clear();
        assert!(state.reconcile_selection(&perf));
        assert_eq!(state.selected_device, SelectedDevice::Cpu);

        // Network adapter exists, then disappears.
        state.selected_device = SelectedDevice::Network(1001);
        assert!(!state.reconcile_selection(&perf));
        perf.networks.clear();
        assert!(state.reconcile_selection(&perf));
        assert_eq!(state.selected_device, SelectedDevice::Cpu);

        // GPU exists, then disappears.
        state.selected_device = SelectedDevice::Gpu(2002);
        assert!(!state.reconcile_selection(&perf));
        perf.gpus.clear();
        assert!(state.reconcile_selection(&perf));
        assert_eq!(state.selected_device, SelectedDevice::Cpu);

        // Memory never falls back.
        state.selected_device = SelectedDevice::Memory;
        assert!(!state.reconcile_selection(&perf));
        assert_eq!(state.selected_device, SelectedDevice::Memory);
    }
}
