//! Pure state model and deterministic layout/fallback logic for the Performance slice.

use std::collections::HashMap;

use loom_types::{GpuEngineSlice, PerformanceSlice};

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

/// Returns the responsive column count (`1`, `2`, `4`, or `8`) for the logical
/// CPU chart grid based on logical processor count.
#[inline]
pub(crate) const fn logical_cpu_grid_columns(logical_count: usize) -> usize {
    match logical_count {
        0..=1 => 1,
        2..=4 => 2,
        5..=16 => 4,
        _ => 8,
    }
}

/// Selects up to four default engine indices for a GPU's `2×2` multi-engine grid.
///
/// Prefers distinct well-known engines (`3D`, `Copy`, `Video Decode`,
/// `Video Encode`), then fills remaining panes with any other distinct engines
/// before cycling/clamping when fewer than 4 engines are reported.
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
    gpu_grid_engines: HashMap<u64, [usize; 4]>,
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

    /// Returns (and initializes/clamps if needed) the 4 engine indices for GPU `luid`.
    pub(crate) fn ensure_gpu_grid_engines(
        &mut self,
        luid: u64,
        engines: &[GpuEngineSlice],
    ) -> [usize; 4] {
        let len = engines.len();
        let entry = self
            .gpu_grid_engines
            .entry(luid)
            .or_insert_with(|| default_gpu_engine_indices(engines));

        if len == 0 {
            *entry = [0; 4];
        } else if entry.iter().any(|&idx| idx >= len) {
            *entry = default_gpu_engine_indices(engines);
        }
        *entry
    }

    /// Updates a single pane's selected engine index in `2×2` GPU mode.
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
        DiskDeviceSlice, GpuAdapterSlice, GpuEngineSlice, NetAdapterSlice, PerformanceSlice,
    };

    use super::*;

    fn engine(name: &str) -> GpuEngineSlice {
        GpuEngineSlice {
            name: Arc::from(name),
            ..Default::default()
        }
    }

    #[test]
    fn logical_cpu_grid_columns_scales_cleanly() {
        assert_eq!(logical_cpu_grid_columns(0), 1);
        assert_eq!(logical_cpu_grid_columns(1), 1);
        assert_eq!(logical_cpu_grid_columns(2), 2);
        assert_eq!(logical_cpu_grid_columns(4), 2);
        assert_eq!(logical_cpu_grid_columns(6), 4);
        assert_eq!(logical_cpu_grid_columns(8), 4);
        assert_eq!(logical_cpu_grid_columns(16), 4);
        assert_eq!(logical_cpu_grid_columns(24), 8);
        assert_eq!(logical_cpu_grid_columns(32), 8);
        assert_eq!(logical_cpu_grid_columns(64), 8);
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
