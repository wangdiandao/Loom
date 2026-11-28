//! Revision-keyed chart and sparkline series cache for the Performance view.
//!
//! Avoids rebuilding 60-sample `Vec<SinglePoint>`, `Vec<DualPoint>`, and
//! `Vec<SparklinePoint>` buffers on unrelated UI redraws (window resize,
//! hover tooltips, context menus, sidebar collapse, or dropdown interactions)
//! when `revisions.performance` has not advanced.

use std::collections::HashMap;

use loom_types::{
    DiskDeviceSlice, GpuAdapterSlice, HistoryRing, NetAdapterSlice, PerformanceSlice,
};
use loom_ui_kit::{SparklinePoint, stepped_network_ceiling, stepped_throughput_ceiling};

use super::charts::{
    DualPoint, HISTORY_WINDOW, SinglePoint, dual_history_rings_to_points,
    dual_throughput_rings_to_points, history_ring_to_points, history_ring_to_sparkline_points,
    network_combined_peak_bytes, throughput_pair_to_sparkline_points,
};

/// Cached sidebar sparkline series and stepped ceiling for a network adapter.
pub(crate) struct CachedNetworkSparkline {
    pub(crate) points: Vec<SparklinePoint>,
    pub(crate) y_max_unit: f32,
}

/// Cached dual throughput series plus stepped Y-axis ceiling parameters.
pub(crate) struct CachedDualThroughput {
    pub(crate) points: Vec<DualPoint>,
    pub(crate) y_max_unit: f64,
    pub(crate) unit_label: &'static str,
    pub(crate) divisor: f64,
}

/// Revision-keyed cache of derived Performance chart & sparkline series.
///
/// Invalidated automatically whenever `revisions.performance` advances.
#[derive(Default)]
pub(crate) struct PerformanceSeriesCache {
    revision: Option<u64>,
    // Sidebar sparklines (populated once per performance revision)
    cpu_sparkline: Vec<SparklinePoint>,
    mem_sparkline: Vec<SparklinePoint>,
    disk_sparklines: HashMap<u32, Vec<SparklinePoint>>,
    network_sparklines: HashMap<u64, CachedNetworkSparkline>,
    gpu_sparklines: HashMap<u64, Vec<SparklinePoint>>,
    // Detail pane series (populated lazily on demand for the active device)
    cpu_overall: Option<Vec<SinglePoint>>,
    cpu_dual: Option<Vec<DualPoint>>,
    cpu_per_core: Option<Vec<Vec<SinglePoint>>>,
    memory_usage: Option<Vec<SinglePoint>>,
    disk_active: HashMap<u32, Vec<SinglePoint>>,
    disk_transfer: HashMap<u32, CachedDualThroughput>,
    network_throughput: HashMap<u64, CachedDualThroughput>,
    gpu_overall: HashMap<u64, Vec<SinglePoint>>,
    gpu_dedicated_mem: HashMap<u64, Vec<SinglePoint>>,
    gpu_shared_mem: HashMap<u64, Vec<SinglePoint>>,
    gpu_engines: HashMap<(u64, usize), Vec<SinglePoint>>,
    #[cfg(test)]
    sidebar_rebuilds: usize,
    #[cfg(test)]
    detail_builds: usize,
}

impl PerformanceSeriesCache {
    /// Synchronizes the cache with `(revision, perf)`.
    ///
    /// If `revision` matches the cached revision, returns immediately in O(1)
    /// without allocating or recomputing any series.
    pub(crate) fn sync(&mut self, revision: u64, perf: &PerformanceSlice) {
        if self.revision == Some(revision) {
            return;
        }
        self.revision = Some(revision);
        #[cfg(test)]
        {
            self.sidebar_rebuilds += 1;
        }

        // Rebuild sidebar sparklines once per performance revision.
        self.cpu_sparkline = history_ring_to_sparkline_points(&perf.cpu_history);
        self.mem_sparkline = history_ring_to_sparkline_points(&perf.mem_history);

        self.disk_sparklines.clear();
        for disk in &perf.disks {
            self.disk_sparklines.insert(
                disk.index,
                history_ring_to_sparkline_points(&disk.active_history),
            );
        }

        self.network_sparklines.clear();
        for net in &perf.networks {
            let max_bytes = network_combined_peak_bytes(net);
            let (y_max_unit, _unit, divisor) = stepped_network_ceiling(max_bytes);
            let points =
                throughput_pair_to_sparkline_points(&net.recv_history, &net.sent_history, divisor);
            self.network_sparklines.insert(
                net.luid,
                CachedNetworkSparkline {
                    points,
                    y_max_unit: y_max_unit as f32,
                },
            );
        }

        self.gpu_sparklines.clear();
        for gpu in &perf.gpus {
            self.gpu_sparklines.insert(
                gpu.luid,
                history_ring_to_sparkline_points(&gpu.util_history),
            );
        }

        // Clear lazy detail caches so only the currently displayed device is recomputed.
        self.cpu_overall = None;
        self.cpu_dual = None;
        self.cpu_per_core = None;
        self.memory_usage = None;
        self.disk_active.clear();
        self.disk_transfer.clear();
        self.network_throughput.clear();
        self.gpu_overall.clear();
        self.gpu_dedicated_mem.clear();
        self.gpu_shared_mem.clear();
        self.gpu_engines.clear();
    }

    #[inline]
    pub(crate) fn cpu_sparkline(&self) -> &[SparklinePoint] {
        &self.cpu_sparkline
    }

    #[inline]
    pub(crate) fn mem_sparkline(&self) -> &[SparklinePoint] {
        &self.mem_sparkline
    }

    #[inline]
    pub(crate) fn disk_sparkline(&self, index: u32) -> &[SparklinePoint] {
        self.disk_sparklines
            .get(&index)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    #[inline]
    pub(crate) fn network_sparkline(&self, luid: u64) -> Option<&CachedNetworkSparkline> {
        self.network_sparklines.get(&luid)
    }

    #[inline]
    pub(crate) fn gpu_sparkline(&self, luid: u64) -> &[SparklinePoint] {
        self.gpu_sparklines
            .get(&luid)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub(crate) fn cpu_overall(&mut self, perf: &PerformanceSlice) -> &[SinglePoint] {
        #[cfg(test)]
        let mut built = false;
        let pts = self.cpu_overall.get_or_insert_with(|| {
            #[cfg(test)]
            {
                built = true;
            }
            history_ring_to_points(&perf.cpu_history)
        });
        #[cfg(test)]
        if built {
            self.detail_builds += 1;
        }
        pts.as_slice()
    }

    pub(crate) fn cpu_dual(&mut self, perf: &PerformanceSlice) -> &[DualPoint] {
        #[cfg(test)]
        let mut built = false;
        let pts = self.cpu_dual.get_or_insert_with(|| {
            #[cfg(test)]
            {
                built = true;
            }
            dual_history_rings_to_points(&perf.cpu_history, &perf.cpu_kernel_history)
        });
        #[cfg(test)]
        if built {
            self.detail_builds += 1;
        }
        pts.as_slice()
    }

    pub(crate) fn cpu_per_core(&mut self, perf: &PerformanceSlice) -> &[Vec<SinglePoint>] {
        #[cfg(test)]
        let mut built = false;
        let series = self.cpu_per_core.get_or_insert_with(|| {
            #[cfg(test)]
            {
                built = true;
            }
            perf.per_core_history
                .iter()
                .map(history_ring_to_points)
                .collect()
        });
        #[cfg(test)]
        if built {
            self.detail_builds += 1;
        }
        series.as_slice()
    }

    pub(crate) fn memory_usage(&mut self, perf: &PerformanceSlice) -> &[SinglePoint] {
        #[cfg(test)]
        let mut built = false;
        let pts = self.memory_usage.get_or_insert_with(|| {
            #[cfg(test)]
            {
                built = true;
            }
            history_ring_to_points(&perf.mem_history)
        });
        #[cfg(test)]
        if built {
            self.detail_builds += 1;
        }
        pts.as_slice()
    }

    pub(crate) fn disk_active(&mut self, disk: &DiskDeviceSlice) -> &[SinglePoint] {
        #[cfg(test)]
        let mut built = false;
        let pts = self.disk_active.entry(disk.index).or_insert_with(|| {
            #[cfg(test)]
            {
                built = true;
            }
            history_ring_to_points(&disk.active_history)
        });
        #[cfg(test)]
        if built {
            self.detail_builds += 1;
        }
        pts.as_slice()
    }

    pub(crate) fn disk_transfer(&mut self, disk: &DiskDeviceSlice) -> &CachedDualThroughput {
        #[cfg(test)]
        let mut built = false;
        let entry = self.disk_transfer.entry(disk.index).or_insert_with(|| {
            #[cfg(test)]
            {
                built = true;
            }
            let max_rw_bps = disk
                .read_history
                .max_value()
                .max(disk.write_history.max_value())
                .max(disk.read_bytes_per_sec)
                .max(disk.write_bytes_per_sec);
            let (y_max_unit, unit_label, divisor) = stepped_throughput_ceiling(max_rw_bps);
            let points =
                dual_throughput_rings_to_points(&disk.read_history, &disk.write_history, divisor);
            CachedDualThroughput {
                points,
                y_max_unit,
                unit_label,
                divisor,
            }
        });
        #[cfg(test)]
        if built {
            self.detail_builds += 1;
        }
        entry
    }

    pub(crate) fn network_throughput(
        &mut self,
        adapter: &NetAdapterSlice,
    ) -> &CachedDualThroughput {
        #[cfg(test)]
        let mut built = false;
        let entry = self
            .network_throughput
            .entry(adapter.luid)
            .or_insert_with(|| {
                #[cfg(test)]
                {
                    built = true;
                }
                let max_bytes_per_sec = adapter
                    .recv_history
                    .max_value()
                    .max(adapter.sent_history.max_value())
                    .max(adapter.recv_bytes_per_sec)
                    .max(adapter.sent_bytes_per_sec);
                let (y_max_unit, unit_label, divisor) = stepped_network_ceiling(max_bytes_per_sec);
                let points = dual_throughput_rings_to_points(
                    &adapter.recv_history,
                    &adapter.sent_history,
                    divisor,
                );
                CachedDualThroughput {
                    points,
                    y_max_unit,
                    unit_label,
                    divisor,
                }
            });
        #[cfg(test)]
        if built {
            self.detail_builds += 1;
        }
        entry
    }

    pub(crate) fn gpu_overall(&mut self, gpu: &GpuAdapterSlice) -> &[SinglePoint] {
        #[cfg(test)]
        let mut built = false;
        let pts = self.gpu_overall.entry(gpu.luid).or_insert_with(|| {
            #[cfg(test)]
            {
                built = true;
            }
            history_ring_to_points(&gpu.util_history)
        });
        #[cfg(test)]
        if built {
            self.detail_builds += 1;
        }
        pts.as_slice()
    }

    pub(crate) fn gpu_dedicated_mem(&mut self, gpu: &GpuAdapterSlice) -> &[SinglePoint] {
        #[cfg(test)]
        let mut built = false;
        let pts = self.gpu_dedicated_mem.entry(gpu.luid).or_insert_with(|| {
            #[cfg(test)]
            {
                built = true;
            }
            history_ring_to_points(&gpu.dedicated_mem_history)
        });
        #[cfg(test)]
        if built {
            self.detail_builds += 1;
        }
        pts.as_slice()
    }

    pub(crate) fn gpu_shared_mem(&mut self, gpu: &GpuAdapterSlice) -> &[SinglePoint] {
        #[cfg(test)]
        let mut built = false;
        let pts = self.gpu_shared_mem.entry(gpu.luid).or_insert_with(|| {
            #[cfg(test)]
            {
                built = true;
            }
            history_ring_to_points(&gpu.shared_mem_history)
        });
        #[cfg(test)]
        if built {
            self.detail_builds += 1;
        }
        pts.as_slice()
    }

    pub(crate) fn gpu_engine(
        &mut self,
        luid: u64,
        engine_ix: usize,
        ring: &HistoryRing<HISTORY_WINDOW>,
    ) -> &[SinglePoint] {
        #[cfg(test)]
        let mut built = false;
        let pts = self
            .gpu_engines
            .entry((luid, engine_ix))
            .or_insert_with(|| {
                #[cfg(test)]
                {
                    built = true;
                }
                history_ring_to_points(ring)
            });
        #[cfg(test)]
        if built {
            self.detail_builds += 1;
        }
        pts.as_slice()
    }

    #[cfg(test)]
    pub(crate) const fn sidebar_rebuilds(&self) -> usize {
        self.sidebar_rebuilds
    }

    #[cfg(test)]
    pub(crate) const fn detail_builds(&self) -> usize {
        self.detail_builds
    }
}
