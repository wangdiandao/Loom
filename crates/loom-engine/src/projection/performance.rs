//! Performance counters, 60-point history ring updates, gap sentinels, and [`PerformanceSlice`] projection.

use std::sync::Arc;

use loom_types::{
    DiskDeviceSample, DiskDeviceSlice, GpuAdapterSample, GpuAdapterSlice, GpuEngineSlice,
    HardwareStaticInfo, HistoryRing, NetAdapterSample, NetAdapterSlice, PerformanceSlice,
    SystemPerfSample, ThroughputRing, Timestamp100Ns,
};

use super::ProjectionEngine;
use super::state::ratio_permille;

impl ProjectionEngine {
    /// Pushes a gap sentinel (`GAP_SENTINEL`) into every non-empty performance history ring
    /// (CPU, Kernel, Memory, Per-Core, Disk, Network, and GPU/Engine rings) to record
    /// a sampling discontinuity (pause/resume, sampling error, or system sleep/clock jump).
    pub fn push_performance_gap(&mut self) {
        push_history_gap_if_needed(&mut self.cpu_history);
        push_history_gap_if_needed(&mut self.cpu_kernel_history);
        push_history_gap_if_needed(&mut self.mem_history);
        for ring in &mut self.per_core_history {
            push_history_gap_if_needed(ring);
        }
        for disk in &mut self.disks {
            push_history_gap_if_needed(&mut disk.active_history);
            push_throughput_gap_if_needed(&mut disk.read_history);
            push_throughput_gap_if_needed(&mut disk.write_history);
        }
        for net in &mut self.networks {
            push_throughput_gap_if_needed(&mut net.recv_history);
            push_throughput_gap_if_needed(&mut net.sent_history);
        }
        for gpu in &mut self.gpus {
            push_history_gap_if_needed(&mut gpu.util_history);
            push_history_gap_if_needed(&mut gpu.dedicated_mem_history);
            push_history_gap_if_needed(&mut gpu.shared_mem_history);
            for eng in &mut gpu.engines {
                push_history_gap_if_needed(&mut eng.history);
            }
        }
    }

    /// Updates performance counters and 60-point history rings from either a full
    /// `sample_performance` sweep or the lightweight summary embedded in the process sample.
    pub fn record_performance(
        &mut self,
        full_sample: Option<SystemPerfSample>,
        per_core: Option<&[u16]>,
    ) {
        if let Some(mut sample) = full_sample {
            if sample.timestamp == Timestamp100Ns::default() {
                sample.timestamp = self.latest_timestamp;
            }
            if sample.cpu_total_permille == 0 {
                sample.cpu_total_permille = self.total_cpu_permille;
            } else {
                self.total_cpu_permille = sample.cpu_total_permille.min(1_000);
            }
            if sample.mem_total_bytes > 0 {
                self.total_memory_permille = sample.mem_util_permille();
            }
            if sample.disk_read_bytes_per_sec == 0 && sample.disk_write_bytes_per_sec == 0 {
                sample.disk_read_bytes_per_sec = self.latest_disk_read_bps;
                sample.disk_write_bytes_per_sec = self.latest_disk_write_bps;
            }
            if sample.physical_core_count == 0 {
                sample.physical_core_count = self.hardware_info.cpu.physical_cores;
            }
            if sample.cpu_base_freq_mhz == 0 {
                sample.cpu_base_freq_mhz = self.hardware_info.cpu.base_freq_mhz;
            }
            self.latest_perf = sample;
        }

        self.cpu_history.push(self.latest_perf.cpu_total_permille);
        self.cpu_kernel_history
            .push(self.latest_perf.cpu_kernel_permille);
        self.mem_history.push(self.latest_perf.mem_util_permille());

        if let Some(cores) = per_core {
            self.per_core_permille.clear();
            self.per_core_permille.extend_from_slice(cores);
            if self.per_core_history.len() != cores.len() {
                self.per_core_history
                    .resize_with(cores.len(), HistoryRing::new);
            }
            for (ring, &val) in self.per_core_history.iter_mut().zip(cores.iter()) {
                ring.push(val);
            }
        }
    }

    /// Stores static CPU and SMBIOS memory hardware metadata probed on the slow lane.
    pub fn set_hardware_static_info(&mut self, info: HardwareStaticInfo) {
        if self.latest_perf.physical_core_count == 0 {
            self.latest_perf.physical_core_count = info.cpu.physical_cores;
        }
        if self.latest_perf.cpu_base_freq_mhz == 0 {
            self.latest_perf.cpu_base_freq_mhz = info.cpu.base_freq_mhz;
        }
        self.hardware_info = info;
    }

    /// Updates per-network-adapter and per-GPU-adapter 60-point histories, pruning stale devices
    /// without per-tick `HashMap` allocations.
    pub fn record_network_and_gpu(
        &mut self,
        net_samples: &[NetAdapterSample],
        gpu_samples: &[GpuAdapterSample],
    ) {
        // Reconcile network adapters by LUID using a swapped Vec (typically 1..=8 NICs).
        let mut prev_nets = std::mem::take(&mut self.networks);
        self.networks.reserve(net_samples.len());

        for sample in net_samples {
            let (mut recv_history, mut sent_history) =
                if let Some(pos) = prev_nets.iter().position(|n| n.luid == sample.luid) {
                    let old = prev_nets.swap_remove(pos);
                    (old.recv_history, old.sent_history)
                } else {
                    (ThroughputRing::new(), ThroughputRing::new())
                };
            recv_history.push(sample.recv_bytes_per_sec);
            sent_history.push(sample.sent_bytes_per_sec);
            self.networks.push(NetAdapterSlice {
                luid: sample.luid,
                name: Arc::clone(&sample.name),
                description: Arc::clone(&sample.description),
                connection_type: sample.connection_type,
                link_speed_bps: sample.link_speed_bps,
                ipv4: sample.ipv4.clone(),
                ipv6: sample.ipv6.clone(),
                dns_suffix: sample.dns_suffix.clone(),
                recv_bytes_per_sec: sample.recv_bytes_per_sec,
                sent_bytes_per_sec: sample.sent_bytes_per_sec,
                recv_history,
                sent_history,
            });
        }

        // Reconcile GPU adapters by LUID and per-engine histories by engine name (typically 1..=4 GPUs).
        let mut prev_gpus = std::mem::take(&mut self.gpus);
        self.gpus.reserve(gpu_samples.len());

        for sample in gpu_samples {
            let (
                mut util_history,
                mut dedicated_mem_history,
                mut shared_mem_history,
                mut prev_engs,
            ) = if let Some(pos) = prev_gpus.iter().position(|g| g.luid == sample.luid) {
                let old = prev_gpus.swap_remove(pos);
                (
                    old.util_history,
                    old.dedicated_mem_history,
                    old.shared_mem_history,
                    old.engines,
                )
            } else {
                (
                    HistoryRing::new(),
                    HistoryRing::new(),
                    HistoryRing::new(),
                    Vec::new(),
                )
            };

            util_history.push(sample.util_permille);
            dedicated_mem_history.push(ratio_permille(
                sample.dedicated_used_bytes,
                sample.dedicated_total_bytes,
            ));
            shared_mem_history.push(ratio_permille(
                sample.shared_used_bytes,
                sample.shared_total_bytes,
            ));

            let mut engines = Vec::with_capacity(sample.engines.len());
            for eng in &sample.engines {
                let mut history = if let Some(pos) = prev_engs
                    .iter()
                    .position(|prev| prev.name.as_ref() == eng.name.as_ref())
                {
                    prev_engs.swap_remove(pos).history
                } else {
                    HistoryRing::new()
                };
                history.push(eng.util_permille);
                engines.push(GpuEngineSlice {
                    name: Arc::clone(&eng.name),
                    util_permille: eng.util_permille,
                    history,
                });
            }

            self.gpus.push(GpuAdapterSlice {
                index: sample.index,
                luid: sample.luid,
                name: Arc::clone(&sample.name),
                driver_version: sample.driver_version.clone(),
                directx_version: sample.directx_version.clone(),
                pci_location: sample.pci_location.clone(),
                temperature_celsius: sample.temperature_celsius,
                power_draw_mw: sample.power_draw_mw,
                fan_speed_percent: sample.fan_speed_percent,
                core_clock_mhz: sample.core_clock_mhz,
                memory_clock_mhz: sample.memory_clock_mhz,
                util_permille: sample.util_permille,
                dedicated_used_bytes: sample.dedicated_used_bytes,
                dedicated_total_bytes: sample.dedicated_total_bytes,
                shared_used_bytes: sample.shared_used_bytes,
                shared_total_bytes: sample.shared_total_bytes,
                d3_asleep: sample.d3_asleep,
                util_history,
                dedicated_mem_history,
                shared_mem_history,
                engines,
            });
        }
    }

    /// Updates per-physical-disk 60-point histories (keyed by physical disk `index`), prunes
    /// removed disks, and syncs aggregate disk read/write rates onto `latest_perf` without
    /// per-tick `HashMap` allocations.
    pub fn record_disk_performance(&mut self, disk_samples: &[DiskDeviceSample]) {
        let mut prev_disks = std::mem::take(&mut self.disks);
        self.disks.reserve(disk_samples.len());

        let mut total_read: u64 = 0;
        let mut total_write: u64 = 0;

        for sample in disk_samples {
            let (mut active_history, mut read_history, mut write_history) =
                if let Some(pos) = prev_disks.iter().position(|d| d.index == sample.index) {
                    let old = prev_disks.swap_remove(pos);
                    (old.active_history, old.read_history, old.write_history)
                } else {
                    (
                        HistoryRing::new(),
                        ThroughputRing::new(),
                        ThroughputRing::new(),
                    )
                };
            active_history.push(sample.active_time_permille);
            read_history.push(sample.read_bytes_per_sec);
            write_history.push(sample.write_bytes_per_sec);

            total_read = total_read.saturating_add(sample.read_bytes_per_sec);
            total_write = total_write.saturating_add(sample.write_bytes_per_sec);

            self.disks.push(DiskDeviceSlice {
                index: sample.index,
                model: Arc::clone(&sample.model),
                media_type: sample.media_type,
                drive_letters: Arc::clone(&sample.drive_letters),
                capacity_bytes: sample.capacity_bytes,
                active_time_permille: sample.active_time_permille,
                read_bytes_per_sec: sample.read_bytes_per_sec,
                write_bytes_per_sec: sample.write_bytes_per_sec,
                avg_response_us: sample.avg_response_us,
                circuit_open: sample.circuit_open,
                active_history,
                read_history,
                write_history,
            });
        }

        self.disks.sort_by_key(|d| d.index);
        self.latest_disk_read_bps = total_read;
        self.latest_disk_write_bps = total_write;
        self.latest_perf.disk_read_bytes_per_sec = total_read;
        self.latest_perf.disk_write_bytes_per_sec = total_write;
        let combined = total_read.saturating_add(total_write);
        if combined > 0 {
            self.total_disk_bytes_per_sec = combined;
        }
    }

    /// Builds a [`PerformanceSlice`] snapshot from the current counters and 60-point rings.
    pub fn project_performance(&self, include_per_core: bool, revision: u64) -> PerformanceSlice {
        PerformanceSlice {
            revision,
            latest: self.latest_perf,
            cpu_history: self.cpu_history,
            cpu_kernel_history: self.cpu_kernel_history,
            mem_history: self.mem_history,
            per_core_permille: if include_per_core {
                self.per_core_permille.clone()
            } else {
                Vec::new()
            },
            per_core_history: if include_per_core {
                self.per_core_history.clone()
            } else {
                Vec::new()
            },
            cpu_info: self.hardware_info.cpu.clone(),
            memory_info: self.hardware_info.memory.clone(),
            disks: self.disks.clone(),
            networks: self.networks.clone(),
            gpus: self.gpus.clone(),
        }
    }
}

#[inline]
fn push_history_gap_if_needed<const N: usize>(ring: &mut HistoryRing<N>) {
    if !ring.is_empty() && !ring.latest().is_some_and(HistoryRing::<N>::is_gap) {
        ring.push_gap();
    }
}

#[inline]
fn push_throughput_gap_if_needed<const N: usize>(ring: &mut ThroughputRing<N>) {
    if !ring.is_empty() && !ring.latest().is_some_and(ThroughputRing::<N>::is_gap) {
        ring.push_gap();
    }
}
