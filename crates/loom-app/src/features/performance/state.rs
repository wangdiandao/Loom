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

/// GPU engine chart layout mode in the Performance detail pane.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum GpuViewMode {
    /// Four-pane `2×2` multi-engine grid with per-pane engine selectors.
    #[default]
    Grid2x2,
    /// Single large engine chart with one engine selector.
    SingleEngine,
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

/// Selects up to four preferred default engine indices by well-known engine name
/// (`3D`, `Copy`, `Video Decode`, `Video Encode`), filling remaining panes with
/// distinct engines before cycling when fewer than 4 engines exist.
pub(crate) fn default_gpu_engine_indices(engines: &[GpuEngineSlice]) -> [usize; 4] {
    if engines.is_empty() {
        return [0; 4];
    }

    const PREFERRED: [&str; 4] = ["3d", "copy", "video decode", "video encode"];
    let mut chosen: Vec<usize> = Vec::with_capacity(4);

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
        if chosen.len() >= 4 {
            break;
        }
        if !chosen.contains(&idx) {
            chosen.push(idx);
        }
    }

    let mut result = [0usize; 4];
    for (slot, out) in result.iter_mut().enumerate() {
        *out = chosen
            .get(slot)
            .copied()
            .unwrap_or_else(|| slot % engines.len());
    }
    result
}

/// Computes smart `2×2` GPU engine indices prioritizing active engines that
/// reached `>= 1%` peak in the active window via [`select_active_gpu_engines`]
/// (always keeping at least 1 engine), then filling any remaining panes with
/// distinct preferred defaults.
pub(crate) fn active_gpu_grid_engine_indices(engines: &[GpuEngineSlice]) -> [usize; 4] {
    if engines.is_empty() {
        return [0; 4];
    }

    let peaks: Vec<(usize, u16)> = engines
        .iter()
        .enumerate()
        .map(|(idx, eng)| (idx, gpu_engine_peak_permille(eng)))
        .collect();
    let active = select_active_gpu_engines(&peaks, 4);
    let preferred = default_gpu_engine_indices(engines);

    let mut chosen: Vec<usize> = Vec::with_capacity(4);
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
        if chosen.len() >= 4 {
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

    let mut result = [0usize; 4];
    for (slot, out) in result.iter_mut().enumerate() {
        *out = chosen
            .get(slot)
            .copied()
            .unwrap_or_else(|| slot % engines.len());
    }
    result
}

/// Selects the default engine index for a GPU's single-engine view (preferring `3D`).
pub(crate) fn default_single_gpu_engine_index(engines: &[GpuEngineSlice]) -> usize {
    if engines.is_empty() {
        return 0;
    }
    engines
        .iter()
        .position(|eng| eng.name.trim().eq_ignore_ascii_case("3d"))
        .or_else(|| {
            engines
                .iter()
                .position(|eng| eng.name.to_ascii_lowercase().contains("3d"))
        })
        .unwrap_or(0)
}

/// Feature-local UI state owned by [`super::PerformanceView`].
#[derive(Debug, Default)]
pub(crate) struct PerformanceViewState {
    pub(crate) selected_device: SelectedDevice,
    pub(crate) cpu_graph_mode: CpuGraphMode,
    pub(crate) show_kernel_times: bool,
    pub(crate) gpu_view_mode: GpuViewMode,
    /// Last computed logical-core grid column count persisted across frames so
    /// [`compute_core_grid_layout`] applies its `8%` resize hysteresis dead-band.
    pub(crate) prev_core_grid_columns: Option<usize>,
    disk_pinned_order: Vec<u32>,
    network_pinned_order: Vec<u64>,
    gpu_grid_engines: HashMap<u64, [usize; 4]>,
    gpu_grid_user_overrides: HashMap<u64, [Option<usize>; 4]>,
    gpu_single_engine: HashMap<u64, usize>,
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

    /// Returns (and updates using active engine selection while preserving any
    /// explicit user `Select` overrides) the 4 engine indices for GPU `luid`.
    pub(crate) fn ensure_gpu_grid_engines(
        &mut self,
        luid: u64,
        engines: &[GpuEngineSlice],
    ) -> [usize; 4] {
        let len = engines.len();
        if len == 0 {
            self.gpu_grid_engines.insert(luid, [0; 4]);
            return [0; 4];
        }

        let auto = active_gpu_grid_engine_indices(engines);
        let overrides = self
            .gpu_grid_user_overrides
            .get(&luid)
            .copied()
            .unwrap_or([None; 4]);

        let mut resolved = [0usize; 4];
        for slot in 0..4 {
            resolved[slot] = match overrides[slot] {
                Some(idx) if idx < len => idx,
                _ => auto[slot],
            };
        }

        self.gpu_grid_engines.insert(luid, resolved);
        resolved
    }

    /// Records an explicit user override for a single pane's selected engine
    /// index in `2×2` GPU mode.
    pub(crate) fn set_gpu_grid_engine(
        &mut self,
        luid: u64,
        pane_slot: usize,
        engine_index: usize,
        engines: &[GpuEngineSlice],
    ) {
        if pane_slot >= 4 || engine_index >= engines.len() {
            return;
        }
        let overrides = self
            .gpu_grid_user_overrides
            .entry(luid)
            .or_insert([None; 4]);
        overrides[pane_slot] = Some(engine_index);

        let mut current = self.ensure_gpu_grid_engines(luid, engines);
        current[pane_slot] = engine_index;
        self.gpu_grid_engines.insert(luid, current);
    }

    /// Returns (and initializes/clamps if needed) the single-engine index for GPU `luid`.
    pub(crate) fn ensure_gpu_single_engine(
        &mut self,
        luid: u64,
        engines: &[GpuEngineSlice],
    ) -> usize {
        let len = engines.len();
        let entry = self
            .gpu_single_engine
            .entry(luid)
            .or_insert_with(|| default_single_gpu_engine_index(engines));
        if len == 0 {
            *entry = 0;
        } else if *entry >= len {
            *entry = default_single_gpu_engine_index(engines);
        }
        *entry
    }

    /// Updates the selected engine index in single-engine GPU mode.
    pub(crate) fn set_gpu_single_engine(
        &mut self,
        luid: u64,
        engine_index: usize,
        engines: &[GpuEngineSlice],
    ) {
        if engine_index < engines.len() {
            self.gpu_single_engine.insert(luid, engine_index);
        }
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
        // Preferred order: 3D (2), Copy (3), Video Decode (4), Video Encode (1)
        assert_eq!(default_gpu_engine_indices(&engines), [2, 3, 4, 1]);
        assert_eq!(default_single_gpu_engine_index(&engines), 2);

        // Partial engine list fills distinct remaining engines and wraps cleanly when < 4.
        let two_engines = vec![engine("Copy"), engine("3D")];
        assert_eq!(default_gpu_engine_indices(&two_engines), [1, 0, 0, 1]);
        assert_eq!(default_single_gpu_engine_index(&two_engines), 1);

        // Empty engine list safely yields zeros.
        assert_eq!(default_gpu_engine_indices(&[]), [0, 0, 0, 0]);
        assert_eq!(default_single_gpu_engine_index(&[]), 0);
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

        // Active >= 1% engines sorted by peak desc: Video Decode (4), 3D (2), Compute_1 (5), then preferred Copy (3).
        let mut state = PerformanceViewState::default();
        assert_eq!(state.ensure_gpu_grid_engines(77, &engines), [4, 2, 5, 3]);

        // User explicitly overrides slot 0 to Copy (3) -> preserved on subsequent ticks!
        state.set_gpu_grid_engine(77, 0, 3, &engines);
        assert_eq!(state.ensure_gpu_grid_engines(77, &engines), [3, 2, 5, 3]);
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
