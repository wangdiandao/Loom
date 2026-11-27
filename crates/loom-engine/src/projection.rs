//! Fast-lane delta computation and pre-sorted, pre-filtered snapshot projections.
//!
//! All sorting, searching, grouping, and tree flattening happen off the UI thread
//! inside [`ProjectionEngine`] using dense process indices and reused scratch
//! buffers so virtualized UI tables perform `O(1)` indexing.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use loom_pal_api::ProcessSampleScratch;
use loom_types::{
    AppHistoryRecord, AppHistorySlice, DetailsRowProjection, DetailsSortColumn, DetailsTableSlice,
    DetailsViewMode, DiskDeviceSample, DiskDeviceSlice, GpuAdapterSample, GpuAdapterSlice,
    GpuEngineSlice, HardwareStaticInfo, HistoryRing, NetAdapterSample, NetAdapterSlice,
    PerformanceSlice, PlatformKind, PlatformProfile, ProcessArch, ProcessCategory, ProcessKey,
    ProcessPriority, ProcessRowKind, ProcessRowProjection, ProcessSortColumn, ProcessStatusFlags,
    ProcessTableSlice, ProcessViewMode, SortDirection, SystemPerfSample, ThroughputRing,
    Timestamp100Ns, UacVirtualization, UserSessionProcess, UserSessionRecord, UsersSlice,
};

use crate::slow_lane::ColdMetaCache;
use crate::tree::{FastBuildHasher, ProcessTree};

/// Maximum number of interned strings retained by the fast-lane interner.
const MAX_INTERNED_STRINGS: usize = 4_096;

/// Maximum number of distinct application keys retained in the live App History accumulator.
const MAX_LIVE_APP_HISTORY_ENTRIES: usize = 2_048;

/// View filtering, grouping, sorting, and tree expansion parameters.
#[derive(Debug, Default, Clone)]
pub struct ViewProjectionConfig {
    /// Active case-insensitive search query.
    pub search_query: String,
    /// Processes view mode (`Grouped` or `Tree`).
    pub process_view_mode: ProcessViewMode,
    /// Processes table sort column.
    pub process_sort_column: ProcessSortColumn,
    /// Processes table sort direction.
    pub process_sort_direction: SortDirection,
    /// Expanded rows in the Processes view.
    pub process_expanded: HashSet<ProcessKey>,
    /// Details view mode (`Flat` or `Tree`).
    pub details_view_mode: DetailsViewMode,
    /// Details table sort column.
    pub details_sort_column: DetailsSortColumn,
    /// Details table sort direction.
    pub details_sort_direction: SortDirection,
    /// Expanded rows in the Details tree view.
    pub details_expanded: HashSet<ProcessKey>,
}

#[derive(Debug, Clone, Copy)]
struct PrevProcCounters {
    cpu_time_100ns: u64,
    disk_bytes: u64,
    net_bytes: u64,
}

/// Double-buffered per-process delta calculator avoiding map reallocation across ticks.
#[derive(Debug, Default)]
struct DeltaTracker {
    prev_timestamp: Option<Timestamp100Ns>,
    prev_counters: HashMap<ProcessKey, PrevProcCounters, FastBuildHasher>,
    next_counters: HashMap<ProcessKey, PrevProcCounters, FastBuildHasher>,
}

impl DeltaTracker {
    fn with_capacity(cap: usize) -> Self {
        Self {
            prev_timestamp: None,
            prev_counters: HashMap::with_capacity_and_hasher(cap, FastBuildHasher),
            next_counters: HashMap::with_capacity_and_hasher(cap, FastBuildHasher),
        }
    }

    fn begin_tick(&mut self, timestamp: Timestamp100Ns) -> u64 {
        self.next_counters.clear();
        self.prev_timestamp
            .map(|prev| timestamp.saturating_sub(prev).as_raw())
            .unwrap_or(0)
    }

    #[inline]
    fn compute_for_process(
        &mut self,
        key: ProcessKey,
        cpu_time_100ns: u64,
        disk_bytes: u64,
        net_bytes: u64,
        elapsed_100ns: u64,
        logical_cpus: u64,
    ) -> (u16, u64, u64, u64, u64) {
        self.next_counters.insert(
            key,
            PrevProcCounters {
                cpu_time_100ns,
                disk_bytes,
                net_bytes,
            },
        );

        if elapsed_100ns == 0 {
            return (0, 0, 0, 0, 0);
        }

        let Some(&prev) = self.prev_counters.get(&key) else {
            return (0, 0, 0, 0, 0);
        };

        let delta_cpu = cpu_time_100ns.saturating_sub(prev.cpu_time_100ns);
        let denom = u128::from(elapsed_100ns).saturating_mul(u128::from(logical_cpus.max(1)));
        let cpu_permille = (u128::from(delta_cpu) * 1_000 + (denom / 2))
            .checked_div(denom)
            .map_or(0, |scaled| scaled.min(1_000) as u16);

        let delta_disk = disk_bytes.saturating_sub(prev.disk_bytes);
        let disk_bps = ((u128::from(delta_disk) * u128::from(Timestamp100Ns::TICKS_PER_SEC))
            / u128::from(elapsed_100ns))
        .min(u128::from(u64::MAX)) as u64;

        let delta_net = net_bytes.saturating_sub(prev.net_bytes);
        let net_bps = ((u128::from(delta_net) * u128::from(Timestamp100Ns::TICKS_PER_SEC))
            / u128::from(elapsed_100ns))
        .min(u128::from(u64::MAX)) as u64;

        (cpu_permille, disk_bps, net_bps, delta_cpu, delta_net)
    }

    fn finish_tick(&mut self, timestamp: Timestamp100Ns) {
        std::mem::swap(&mut self.prev_counters, &mut self.next_counters);
        self.prev_timestamp = Some(timestamp);
    }
}

/// Fast-lane local string interner preventing steady-state heap allocations for repeated image names.
#[derive(Debug, Default)]
struct StringInterner {
    pool: HashSet<Arc<str>, FastBuildHasher>,
}

impl StringInterner {
    #[inline]
    fn intern(&mut self, s: &str) -> Arc<str> {
        if let Some(existing) = self.pool.get(s) {
            return Arc::clone(existing);
        }
        let arc: Arc<str> = Arc::from(s);
        if self.pool.len() >= MAX_INTERNED_STRINGS {
            self.pool.clear();
        }
        self.pool.insert(Arc::clone(&arc));
        arc
    }
}

/// In-memory cumulative application usage counters accumulated on the fast lane.
#[derive(Debug, Clone)]
struct LiveAppHistoryEntry {
    name: Arc<str>,
    cpu_time_100ns: u64,
    network_bytes: u64,
    metered_network_bytes: u64,
}

/// Per-process hot + computed delta snapshot cached on the fast-lane thread.
#[derive(Debug, Clone)]
struct ComputedProcess {
    key: ProcessKey,
    session_id: u32,
    thread_count: u32,
    handle_count: u32,
    cpu_time_100ns: u64,
    delta_cpu_100ns: u64,
    cpu_permille: u16,
    private_working_set_bytes: u64,
    working_set_bytes: u64,
    commit_bytes: u64,
    memory_permille: u16,
    disk_bytes_per_sec: u64,
    net_bytes_per_sec: u64,
    delta_net_bytes: u64,
    gpu_dedicated_bytes: u64,
    gpu_shared_bytes: u64,
    gpu_util_permille: u16,
    priority: ProcessPriority,
    category: ProcessCategory,
    status: ProcessStatusFlags,
    name: Arc<str>,
    window_title: Option<Arc<str>>,
}

/// Fast-lane delta, history-ring, tree, and view-projection engine.
#[derive(Debug)]
pub struct ProjectionEngine {
    platform: PlatformProfile,
    delta_tracker: DeltaTracker,
    interner: StringInterner,
    latest_timestamp: Timestamp100Ns,
    latest_perf: SystemPerfSample,
    computed: Vec<ComputedProcess>,
    seen_keys_scratch: HashSet<ProcessKey, FastBuildHasher>,
    tree_links_scratch: Vec<(ProcessKey, u32)>,
    latest_tree: ProcessTree,
    total_cpu_permille: u16,
    total_memory_permille: u16,
    total_disk_bytes_per_sec: u64,
    total_net_bytes_per_sec: u64,
    total_gpu_permille: u16,
    cpu_history: HistoryRing<60>,
    cpu_kernel_history: HistoryRing<60>,
    mem_history: HistoryRing<60>,
    per_core_permille: Vec<u16>,
    per_core_history: Vec<HistoryRing<60>>,
    hardware_info: HardwareStaticInfo,
    disks: Vec<DiskDeviceSlice>,
    networks: Vec<NetAdapterSlice>,
    gpus: Vec<GpuAdapterSlice>,
    latest_disk_read_bps: u64,
    latest_disk_write_bps: u64,
    app_history_live: HashMap<Arc<str>, LiveAppHistoryEntry>,
}

impl Default for ProjectionEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ProjectionEngine {
    /// Creates a fresh [`ProjectionEngine`] with pre-sized reusable buffers
    /// and [`PlatformProfile::UNKNOWN`].
    pub fn new() -> Self {
        Self::with_platform(PlatformProfile::UNKNOWN)
    }

    /// Creates a fresh [`ProjectionEngine`] configured for `platform`.
    pub fn with_platform(platform: PlatformProfile) -> Self {
        Self {
            platform,
            delta_tracker: DeltaTracker::with_capacity(512),
            interner: StringInterner::default(),
            latest_timestamp: Timestamp100Ns::default(),
            latest_perf: SystemPerfSample::default(),
            computed: Vec::with_capacity(512),
            seen_keys_scratch: HashSet::with_capacity_and_hasher(512, FastBuildHasher),
            tree_links_scratch: Vec::with_capacity(512),
            latest_tree: ProcessTree::default(),
            total_cpu_permille: 0,
            total_memory_permille: 0,
            total_disk_bytes_per_sec: 0,
            total_net_bytes_per_sec: 0,
            total_gpu_permille: 0,
            cpu_history: HistoryRing::new(),
            cpu_kernel_history: HistoryRing::new(),
            mem_history: HistoryRing::new(),
            per_core_permille: Vec::new(),
            per_core_history: Vec::new(),
            hardware_info: HardwareStaticInfo::default(),
            disks: Vec::new(),
            networks: Vec::new(),
            gpus: Vec::new(),
            latest_disk_read_bps: 0,
            latest_disk_write_bps: 0,
            app_history_live: HashMap::new(),
        }
    }

    /// Returns the static [`PlatformProfile`] bound to this projection engine.
    #[inline]
    pub const fn platform(&self) -> PlatformProfile {
        self.platform
    }

    /// Ingests a new fast-lane process snapshot, computing per-process deltas and
    /// rebuilding the PID-reuse-safe [`ProcessTree`] in place.
    pub fn ingest_process_sample<Ext>(
        &mut self,
        timestamp: Timestamp100Ns,
        scratch: &ProcessSampleScratch<Ext>,
        fallback_logical_cpus: u32,
    ) {
        let elapsed_100ns = self.delta_tracker.begin_tick(timestamp);
        let logical_cpus = u64::from(
            scratch
                .perf
                .logical_cpu_count
                .max(fallback_logical_cpus)
                .max(1),
        );
        let mem_total_bytes = scratch.perf.mem_total_bytes;

        self.computed.clear();
        self.seen_keys_scratch.clear();
        self.tree_links_scratch.clear();

        let mut sum_cpu_permille: u32 = 0;
        let mut sum_disk_bps: u64 = 0;
        let mut sum_net_bps: u64 = 0;
        let mut sum_gpu_permille: u32 = 0;

        for sample in &scratch.samples {
            if !self.seen_keys_scratch.insert(sample.key) {
                continue;
            }

            let cpu_time_100ns = sample
                .kernel_time_100ns
                .saturating_add(sample.user_time_100ns);
            let disk_bytes = sample
                .disk_read_bytes
                .saturating_add(sample.disk_write_bytes);
            let net_bytes = sample.net_recv_bytes.saturating_add(sample.net_sent_bytes);

            let (
                cpu_permille,
                disk_bytes_per_sec,
                net_bytes_per_sec,
                delta_cpu_100ns,
                delta_net_bytes,
            ) = self.delta_tracker.compute_for_process(
                sample.key,
                cpu_time_100ns,
                disk_bytes,
                net_bytes,
                elapsed_100ns,
                logical_cpus,
            );

            let memory_permille = if mem_total_bytes > 0 {
                ((u128::from(sample.private_working_set_bytes) * 1_000)
                    / u128::from(mem_total_bytes))
                .min(1_000) as u16
            } else {
                0
            };

            if !sample.status.contains(ProcessStatusFlags::SYSTEM_IDLE) {
                sum_cpu_permille = sum_cpu_permille.saturating_add(u32::from(cpu_permille));
            }
            sum_disk_bps = sum_disk_bps.saturating_add(disk_bytes_per_sec);
            sum_net_bps = sum_net_bps.saturating_add(net_bytes_per_sec);
            sum_gpu_permille = sum_gpu_permille.saturating_add(u32::from(sample.gpu_util_permille));

            let raw_name = scratch.str_slice(sample.name_offset, sample.name_len);
            let name = self.interner.intern(raw_name);

            let raw_title = scratch.str_slice(sample.window_title_offset, sample.window_title_len);
            let window_title = if raw_title.is_empty() {
                None
            } else {
                Some(self.interner.intern(raw_title))
            };

            self.tree_links_scratch
                .push((sample.key, sample.parent_pid));
            self.computed.push(ComputedProcess {
                key: sample.key,
                session_id: sample.session_id,
                thread_count: sample.thread_count,
                handle_count: sample.handle_count,
                cpu_time_100ns,
                delta_cpu_100ns,
                cpu_permille,
                private_working_set_bytes: sample.private_working_set_bytes,
                working_set_bytes: sample.working_set_bytes,
                commit_bytes: sample.commit_bytes,
                memory_permille,
                disk_bytes_per_sec,
                net_bytes_per_sec,
                delta_net_bytes,
                gpu_dedicated_bytes: sample.gpu_dedicated_bytes,
                gpu_shared_bytes: sample.gpu_shared_bytes,
                gpu_util_permille: sample.gpu_util_permille,
                priority: sample.priority,
                category: sample.category,
                status: sample.status,
                name,
                window_title,
            });
        }

        self.delta_tracker.finish_tick(timestamp);
        self.latest_tree.rebuild(&self.tree_links_scratch);
        self.latest_timestamp = timestamp;
        self.latest_perf = scratch.perf;
        if self.latest_perf.timestamp == Timestamp100Ns::default() {
            self.latest_perf.timestamp = timestamp;
        }
        if self.latest_perf.logical_cpu_count == 0 {
            self.latest_perf.logical_cpu_count = logical_cpus as u32;
        }
        if self.latest_perf.process_count == 0 {
            self.latest_perf.process_count = self.computed.len() as u32;
        }

        self.total_cpu_permille = if scratch.perf.cpu_total_permille > 0 {
            scratch.perf.cpu_total_permille.min(1_000)
        } else {
            sum_cpu_permille.min(1_000) as u16
        };
        if self.latest_perf.cpu_total_permille == 0 {
            self.latest_perf.cpu_total_permille = self.total_cpu_permille;
        }

        self.total_memory_permille = self.latest_perf.mem_util_permille();

        if self.latest_perf.disk_read_bytes_per_sec == 0
            && self.latest_perf.disk_write_bytes_per_sec == 0
        {
            self.latest_perf.disk_read_bytes_per_sec = self.latest_disk_read_bps;
            self.latest_perf.disk_write_bytes_per_sec = self.latest_disk_write_bps;
        }
        if self.latest_perf.physical_core_count == 0 {
            self.latest_perf.physical_core_count = self.hardware_info.cpu.physical_cores;
        }
        if self.latest_perf.cpu_base_freq_mhz == 0 {
            self.latest_perf.cpu_base_freq_mhz = self.hardware_info.cpu.base_freq_mhz;
        }

        let perf_disk_bps = self
            .latest_perf
            .disk_read_bytes_per_sec
            .saturating_add(self.latest_perf.disk_write_bytes_per_sec);
        self.total_disk_bytes_per_sec = if perf_disk_bps > 0 {
            perf_disk_bps
        } else {
            sum_disk_bps
        };

        let perf_net_bps = scratch
            .perf
            .net_recv_bytes_per_sec
            .saturating_add(scratch.perf.net_sent_bytes_per_sec);
        self.total_net_bytes_per_sec = if perf_net_bps > 0 {
            perf_net_bps
        } else {
            sum_net_bps
        };

        self.total_gpu_permille = if scratch.perf.gpu_util_permille > 0 {
            scratch.perf.gpu_util_permille.min(1_000)
        } else {
            sum_gpu_permille.min(1_000) as u16
        };
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

    /// Updates per-network-adapter and per-GPU-adapter 60-point histories, pruning stale devices.
    pub fn record_network_and_gpu(
        &mut self,
        net_samples: &[NetAdapterSample],
        gpu_samples: &[GpuAdapterSample],
    ) {
        // Reconcile network adapters by LUID.
        let mut prev_nets: HashMap<u64, (ThroughputRing<60>, ThroughputRing<60>)> = self
            .networks
            .drain(..)
            .map(|n| (n.luid, (n.recv_history, n.sent_history)))
            .collect();

        for sample in net_samples {
            let (mut recv_history, mut sent_history) = prev_nets
                .remove(&sample.luid)
                .unwrap_or_else(|| (ThroughputRing::new(), ThroughputRing::new()));
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

        // Reconcile GPU adapters by LUID and per-engine histories by engine name.
        type PrevGpuRings = (
            HistoryRing<60>,
            HistoryRing<60>,
            HistoryRing<60>,
            HashMap<Arc<str>, HistoryRing<60>>,
        );
        let mut prev_gpus: HashMap<u64, PrevGpuRings> = self
            .gpus
            .drain(..)
            .map(|g| {
                let eng_map = g.engines.into_iter().map(|e| (e.name, e.history)).collect();
                (
                    g.luid,
                    (
                        g.util_history,
                        g.dedicated_mem_history,
                        g.shared_mem_history,
                        eng_map,
                    ),
                )
            })
            .collect();

        for sample in gpu_samples {
            let (
                mut util_history,
                mut dedicated_mem_history,
                mut shared_mem_history,
                mut prev_engs,
            ) = prev_gpus.remove(&sample.luid).unwrap_or_else(|| {
                (
                    HistoryRing::new(),
                    HistoryRing::new(),
                    HistoryRing::new(),
                    HashMap::new(),
                )
            });

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
                let mut history = prev_engs.remove(&eng.name).unwrap_or_else(HistoryRing::new);
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
    /// removed disks, and syncs aggregate disk read/write rates onto `latest_perf`.
    pub fn record_disk_performance(&mut self, disk_samples: &[DiskDeviceSample]) {
        type PrevDiskRings = (HistoryRing<60>, ThroughputRing<60>, ThroughputRing<60>);
        let mut prev_disks: HashMap<u32, PrevDiskRings> = self
            .disks
            .drain(..)
            .map(|d| (d.index, (d.active_history, d.read_history, d.write_history)))
            .collect();

        let mut total_read: u64 = 0;
        let mut total_write: u64 = 0;

        for sample in disk_samples {
            let (mut active_history, mut read_history, mut write_history) =
                prev_disks.remove(&sample.index).unwrap_or_else(|| {
                    (
                        HistoryRing::new(),
                        ThroughputRing::new(),
                        ThroughputRing::new(),
                    )
                });
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

    /// Returns the latest verified PID-reuse-safe [`ProcessTree`].
    #[inline]
    pub fn latest_tree(&self) -> &ProcessTree {
        &self.latest_tree
    }

    /// Returns the timestamp of the most recently ingested sample.
    #[inline]
    pub fn latest_timestamp(&self) -> Timestamp100Ns {
        self.latest_timestamp
    }

    /// Iterates over the [`ProcessKey`]s of all processes in the latest sample.
    #[inline]
    pub fn live_keys(&self) -> impl Iterator<Item = ProcessKey> + '_ {
        self.computed.iter().map(|p| p.key)
    }

    /// Builds a pre-sorted, pre-filtered [`ProcessTableSlice`].
    pub fn project_processes(
        &self,
        config: &ViewProjectionConfig,
        cold_cache: &ColdMetaCache,
        revision: u64,
    ) -> ProcessTableSlice {
        let query_lower = config.search_query.trim().to_lowercase();
        let has_query = !query_lower.is_empty();

        let rows = match config.process_view_mode {
            ProcessViewMode::Grouped => {
                self.project_processes_grouped(config, cold_cache, &query_lower, has_query)
            }
            ProcessViewMode::Tree => {
                self.project_processes_tree(config, cold_cache, &query_lower, has_query)
            }
        };

        ProcessTableSlice {
            revision,
            timestamp: self.latest_timestamp,
            view_mode: config.process_view_mode,
            sort_column: config.process_sort_column,
            sort_direction: config.process_sort_direction,
            total_process_count: self.computed.len(),
            total_cpu_permille: self.total_cpu_permille,
            total_memory_permille: self.total_memory_permille,
            total_disk_bytes_per_sec: self.total_disk_bytes_per_sec,
            total_net_bytes_per_sec: self.total_net_bytes_per_sec,
            total_gpu_permille: self.total_gpu_permille,
            rows,
        }
    }

    /// Builds a pre-sorted, pre-filtered [`DetailsTableSlice`].
    pub fn project_details(
        &self,
        config: &ViewProjectionConfig,
        cold_cache: &ColdMetaCache,
        revision: u64,
    ) -> DetailsTableSlice {
        let query_lower = config.search_query.trim().to_lowercase();
        let has_query = !query_lower.is_empty();

        let rows = match config.details_view_mode {
            DetailsViewMode::Flat => {
                self.project_details_flat(config, cold_cache, &query_lower, has_query)
            }
            DetailsViewMode::Tree => {
                self.project_details_tree(config, cold_cache, &query_lower, has_query)
            }
        };

        DetailsTableSlice {
            revision,
            timestamp: self.latest_timestamp,
            view_mode: config.details_view_mode,
            sort_column: config.details_sort_column,
            sort_direction: config.details_sort_direction,
            rows,
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

    /// Accumulates per-tick CPU and network deltas from non-system processes into the
    /// in-memory App History map (keyed by app group ID or lowercase image name).
    pub fn accumulate_app_history(&mut self, cold_cache: &ColdMetaCache) {
        for proc in &self.computed {
            if proc.delta_cpu_100ns == 0 && proc.delta_net_bytes == 0 {
                continue;
            }
            if proc.category == ProcessCategory::System
                || proc
                    .status
                    .intersects(ProcessStatusFlags::SYSTEM_IDLE | ProcessStatusFlags::KERNEL)
            {
                continue;
            }

            let cold = cold_cache.get_process(proc.key);
            let app_id: Arc<str> =
                if let Some(group_id) = cold.and_then(|meta| meta.app_group_id.as_ref()) {
                    Arc::clone(group_id)
                } else if proc.name.bytes().all(|b| !b.is_ascii_uppercase()) {
                    Arc::clone(&proc.name)
                } else {
                    Arc::from(proc.name.to_ascii_lowercase())
                };

            let friendly_name = resolve_display_name(proc, cold_cache);

            if let Some(entry) = self.app_history_live.get_mut(&app_id) {
                entry.cpu_time_100ns = entry.cpu_time_100ns.saturating_add(proc.delta_cpu_100ns);
                entry.network_bytes = entry.network_bytes.saturating_add(proc.delta_net_bytes);
                // Upgrade raw image name once PE description / window title resolves.
                if entry.name.as_ref() == proc.name.as_ref()
                    && friendly_name.as_ref() != proc.name.as_ref()
                {
                    entry.name = friendly_name;
                }
            } else if self.app_history_live.len() < MAX_LIVE_APP_HISTORY_ENTRIES {
                self.app_history_live.insert(
                    app_id,
                    LiveAppHistoryEntry {
                        name: friendly_name,
                        cpu_time_100ns: proc.delta_cpu_100ns,
                        network_bytes: proc.delta_net_bytes,
                        metered_network_bytes: 0,
                    },
                );
            }
        }
    }

    /// Clears all in-memory cumulative App History counters.
    pub fn clear_app_history(&mut self) {
        self.app_history_live.clear();
    }

    /// Builds an [`AppHistorySlice`] by merging persisted baseline entries with live
    /// in-memory cumulative usage counters, sorted by CPU time and network bytes descending.
    pub fn project_app_history(
        &self,
        since_epoch_secs: Option<u64>,
        persisted: &[AppHistoryRecord],
        revision: u64,
    ) -> AppHistorySlice {
        let mut merged: HashMap<Arc<str>, AppHistoryRecord> =
            HashMap::with_capacity(persisted.len().saturating_add(self.app_history_live.len()));

        for rec in persisted {
            merged.insert(Arc::clone(&rec.app_id), rec.clone());
        }

        for (app_id, live) in &self.app_history_live {
            if let Some(existing) = merged.get_mut(app_id) {
                existing.cpu_time_100ns =
                    existing.cpu_time_100ns.saturating_add(live.cpu_time_100ns);
                existing.network_bytes = existing.network_bytes.saturating_add(live.network_bytes);
                existing.metered_network_bytes = existing
                    .metered_network_bytes
                    .saturating_add(live.metered_network_bytes);
                if existing.name.is_empty() && !live.name.is_empty() {
                    existing.name = Arc::clone(&live.name);
                }
            } else {
                merged.insert(
                    Arc::clone(app_id),
                    AppHistoryRecord {
                        app_id: Arc::clone(app_id),
                        name: Arc::clone(&live.name),
                        cpu_time_100ns: live.cpu_time_100ns,
                        network_bytes: live.network_bytes,
                        metered_network_bytes: live.metered_network_bytes,
                    },
                );
            }
        }

        let mut items: Vec<AppHistoryRecord> = merged.into_values().collect();
        items.sort_by(|a, b| {
            b.cpu_time_100ns
                .cmp(&a.cpu_time_100ns)
                .then_with(|| b.network_bytes.cmp(&a.network_bytes))
                .then_with(|| {
                    a.name
                        .as_bytes()
                        .iter()
                        .map(u8::to_ascii_lowercase)
                        .cmp(b.name.as_bytes().iter().map(u8::to_ascii_lowercase))
                })
                .then_with(|| a.app_id.cmp(&b.app_id))
        });

        AppHistorySlice {
            revision,
            timestamp: self.latest_timestamp,
            since_epoch_secs,
            items,
        }
    }

    /// Enriches OS user sessions with live per-session process aggregates and sorted
    /// per-session process lists from the latest fast-lane sample.
    pub fn project_users(
        &self,
        base_sessions: &[UserSessionRecord],
        cold_cache: &ColdMetaCache,
        revision: u64,
    ) -> UsersSlice {
        let mut sessions = Vec::with_capacity(base_sessions.len());

        for base in base_sessions {
            let mut cpu_sum: u32 = 0;
            let mut memory_bytes: u64 = 0;
            let mut disk_bytes_per_sec: u64 = 0;
            let mut net_bytes_per_sec: u64 = 0;
            let mut processes = Vec::new();

            for proc in &self.computed {
                if proc.session_id != base.session_id {
                    continue;
                }
                cpu_sum = cpu_sum.saturating_add(u32::from(proc.cpu_permille));
                memory_bytes = memory_bytes.saturating_add(proc.private_working_set_bytes);
                disk_bytes_per_sec = disk_bytes_per_sec.saturating_add(proc.disk_bytes_per_sec);
                net_bytes_per_sec = net_bytes_per_sec.saturating_add(proc.net_bytes_per_sec);

                processes.push(UserSessionProcess {
                    key: proc.key,
                    name: Arc::clone(&proc.name),
                    display_name: resolve_display_name(proc, cold_cache),
                    status: effective_status(proc, cold_cache),
                    cpu_permille: proc.cpu_permille,
                    memory_bytes: proc.private_working_set_bytes,
                    disk_bytes_per_sec: proc.disk_bytes_per_sec,
                    net_bytes_per_sec: proc.net_bytes_per_sec,
                });
            }

            processes.sort_by(|a, b| {
                b.cpu_permille
                    .cmp(&a.cpu_permille)
                    .then_with(|| b.memory_bytes.cmp(&a.memory_bytes))
                    .then_with(|| {
                        a.name
                            .as_bytes()
                            .iter()
                            .map(u8::to_ascii_lowercase)
                            .cmp(b.name.as_bytes().iter().map(u8::to_ascii_lowercase))
                    })
                    .then_with(|| a.key.cmp(&b.key))
            });

            sessions.push(UserSessionRecord {
                session_id: base.session_id,
                user_name: Arc::clone(&base.user_name),
                state: base.state,
                process_count: processes.len() as u32,
                cpu_permille: cpu_sum.min(1_000) as u16,
                memory_bytes,
                disk_bytes_per_sec,
                net_bytes_per_sec,
                processes,
            });
        }

        UsersSlice {
            revision,
            timestamp: self.latest_timestamp,
            sessions,
        }
    }

    #[inline]
    fn build_process_row(
        &self,
        proc: &ComputedProcess,
        cold_cache: &ColdMetaCache,
        depth: u16,
        has_children: bool,
        is_expanded: bool,
        child_count: u32,
    ) -> ProcessRowProjection {
        let cold = cold_cache.get_process(proc.key);
        let asset = cold.and_then(|meta| meta.asset.as_ref());

        let publisher = asset.and_then(|a| a.publisher.clone());
        let exe_path = cold.and_then(|meta| meta.exe_path.clone());
        let command_line = cold.and_then(|meta| meta.command_line.clone());
        let icon = asset.and_then(|a| a.icon.clone());
        let display_name = resolve_display_name(proc, cold_cache);
        let status = effective_status(proc, cold_cache);
        let disk_permille =
            compute_throughput_permille(proc.disk_bytes_per_sec, self.total_disk_bytes_per_sec);
        let net_permille =
            compute_throughput_permille(proc.net_bytes_per_sec, self.total_net_bytes_per_sec);

        ProcessRowProjection {
            kind: ProcessRowKind::Process,
            key: proc.key,
            depth,
            has_children,
            is_expanded,
            child_count,
            name: Arc::clone(&proc.name),
            display_name,
            window_title: proc.window_title.clone(),
            publisher,
            exe_path,
            command_line,
            icon,
            category: proc.category,
            status,
            cpu_permille: proc.cpu_permille,
            memory_bytes: proc.private_working_set_bytes,
            memory_permille: proc.memory_permille,
            disk_bytes_per_sec: proc.disk_bytes_per_sec,
            disk_permille,
            net_bytes_per_sec: proc.net_bytes_per_sec,
            net_permille,
            gpu_util_permille: proc.gpu_util_permille,
            gpu_dedicated_bytes: proc.gpu_dedicated_bytes,
            parent_key: None,
            leak_warning: None,
            webview2_role: None,
        }
    }

    #[inline]
    fn build_details_row(
        &self,
        idx: usize,
        cold_cache: &ColdMetaCache,
        depth: u16,
        has_children: bool,
        is_expanded: bool,
    ) -> DetailsRowProjection {
        let proc = &self.computed[idx];
        let cold = cold_cache.get_process(proc.key);
        let asset = cold.and_then(|m| m.asset.as_ref());
        let parent_key = self
            .latest_tree
            .parent_index(idx)
            .map(|p| self.latest_tree.key_at(p));
        let status = effective_status(proc, cold_cache);

        DetailsRowProjection {
            key: proc.key,
            parent_key,
            depth,
            has_children,
            is_expanded,
            name: Arc::clone(&proc.name),
            description: asset.and_then(|a| a.file_description.clone()),
            publisher: asset.and_then(|a| a.publisher.clone()),
            icon: asset.and_then(|a| a.icon.clone()),
            status,
            user_name: cold.and_then(|m| m.user_name.clone()),
            session_id: proc.session_id,
            cpu_permille: proc.cpu_permille,
            cpu_time_100ns: proc.cpu_time_100ns,
            private_working_set_bytes: proc.private_working_set_bytes,
            working_set_bytes: proc.working_set_bytes,
            commit_bytes: proc.commit_bytes,
            thread_count: proc.thread_count,
            handle_count: proc.handle_count,
            priority: proc.priority,
            arch: cold.map_or(ProcessArch::Unknown, |m| m.arch),
            uac_virtualization: cold.map_or(UacVirtualization::Unknown, |m| m.uac_virtualization),
            gpu_dedicated_bytes: proc.gpu_dedicated_bytes,
            gpu_shared_bytes: proc.gpu_shared_bytes,
            exe_path: cold.and_then(|m| m.exe_path.clone()),
            command_line: cold.and_then(|m| m.command_line.clone()),
            leak_warning: None,
            webview2_role: None,
        }
    }

    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn build_aggregated_process_row(
        &self,
        proc: &ComputedProcess,
        agg: GroupAggregate,
        cold_cache: &ColdMetaCache,
        depth: u16,
        has_children: bool,
        is_expanded: bool,
        child_count: u32,
    ) -> ProcessRowProjection {
        let mut row = self.build_process_row(
            proc,
            cold_cache,
            depth,
            has_children,
            is_expanded,
            child_count,
        );
        row.status = agg.status;
        row.cpu_permille = agg.cpu_permille;
        row.memory_bytes = agg.memory_bytes;
        row.memory_permille = agg.memory_permille;
        row.disk_bytes_per_sec = agg.disk_bytes_per_sec;
        row.disk_permille =
            compute_throughput_permille(agg.disk_bytes_per_sec, self.total_disk_bytes_per_sec);
        row.net_bytes_per_sec = agg.net_bytes_per_sec;
        row.net_permille =
            compute_throughput_permille(agg.net_bytes_per_sec, self.total_net_bytes_per_sec);
        row.gpu_util_permille = agg.gpu_util_permille;
        row.gpu_dedicated_bytes = agg.gpu_dedicated_bytes;
        row
    }

    fn compute_group_aggregate(
        &self,
        root_idx: usize,
        children: &[usize],
        cold_cache: &ColdMetaCache,
    ) -> GroupAggregate {
        let root = &self.computed[root_idx];
        let root_status = effective_status(root, cold_cache);
        let mut sum_cpu: u32 = u32::from(root.cpu_permille);
        let mut sum_mem: u64 = root.private_working_set_bytes;
        let mut sum_mem_permille: u32 = u32::from(root.memory_permille);
        let mut sum_disk: u64 = root.disk_bytes_per_sec;
        let mut sum_net: u64 = root.net_bytes_per_sec;
        let mut sum_gpu_util: u32 = u32::from(root.gpu_util_permille);
        let mut sum_gpu_vram: u64 = root.gpu_dedicated_bytes;
        let mut all_suspended = root_status.contains(ProcessStatusFlags::SUSPENDED);
        let mut rolled_status = root_status - ProcessStatusFlags::SUSPENDED;

        for &c_idx in children {
            let child = &self.computed[c_idx];
            let child_status = effective_status(child, cold_cache);
            sum_cpu = sum_cpu.saturating_add(u32::from(child.cpu_permille));
            sum_mem = sum_mem.saturating_add(child.private_working_set_bytes);
            sum_mem_permille = sum_mem_permille.saturating_add(u32::from(child.memory_permille));
            sum_disk = sum_disk.saturating_add(child.disk_bytes_per_sec);
            sum_net = sum_net.saturating_add(child.net_bytes_per_sec);
            sum_gpu_util = sum_gpu_util.saturating_add(u32::from(child.gpu_util_permille));
            sum_gpu_vram = sum_gpu_vram.saturating_add(child.gpu_dedicated_bytes);
            all_suspended &= child_status.contains(ProcessStatusFlags::SUSPENDED);
            rolled_status |= child_status
                & (ProcessStatusFlags::EFFICIENCY_MODE | ProcessStatusFlags::NOT_RESPONDING);
        }

        if all_suspended {
            rolled_status |= ProcessStatusFlags::SUSPENDED;
        }

        let memory_permille = if self.latest_perf.mem_total_bytes > 0 {
            ((u128::from(sum_mem) * 1_000) / u128::from(self.latest_perf.mem_total_bytes))
                .min(1_000) as u16
        } else {
            sum_mem_permille.min(1_000) as u16
        };

        GroupAggregate {
            cpu_permille: sum_cpu.min(1_000) as u16,
            memory_bytes: sum_mem,
            memory_permille,
            disk_bytes_per_sec: sum_disk,
            net_bytes_per_sec: sum_net,
            gpu_util_permille: sum_gpu_util.min(1_000) as u16,
            gpu_dedicated_bytes: sum_gpu_vram,
            status: rolled_status,
        }
    }

    fn project_processes_grouped(
        &self,
        config: &ViewProjectionConfig,
        cold_cache: &ColdMetaCache,
        query_lower: &str,
        has_query: bool,
    ) -> Vec<ProcessRowProjection> {
        if self.platform.kind == PlatformKind::Windows {
            return self.project_processes_grouped_windows(
                config,
                cold_cache,
                query_lower,
                has_query,
            );
        }

        let n = self.computed.len();
        let mut by_category: [Vec<usize>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        let mut in_any_section = vec![false; n];

        for (idx, proc) in self.computed.iter().enumerate() {
            if has_query && !matches_process_query(proc, cold_cache, query_lower) {
                continue;
            }
            in_any_section[idx] = true;
            by_category[category_index(proc.category)].push(idx);
        }

        let categories = [
            ProcessCategory::App,
            ProcessCategory::Background,
            ProcessCategory::System,
        ];
        let mut rows = Vec::with_capacity(n + 3);
        let mut visited = vec![false; n];
        let mut stack: Vec<(usize, u16)> = Vec::new();
        let mut section_roots: Vec<usize> = Vec::new();
        let mut visible_children: Vec<usize> = Vec::new();
        let has_expanded = !config.process_expanded.is_empty();

        for category in categories {
            let indices = &by_category[category_index(category)];
            if indices.is_empty() {
                continue;
            }

            rows.push(section_header_row(category, indices.len()));

            // Collect category-local roots: nodes whose verified parent is not in the same category.
            section_roots.clear();
            for &idx in indices {
                let has_same_cat_parent = self.latest_tree.parent_index(idx).is_some_and(|p_idx| {
                    in_any_section[p_idx] && self.computed[p_idx].category == category
                });
                if !has_same_cat_parent {
                    section_roots.push(idx);
                }
            }

            self.sort_process_indices(
                &mut section_roots,
                cold_cache,
                config.process_sort_column,
                config.process_sort_direction,
            );

            stack.clear();
            for &root_idx in section_roots.iter().rev() {
                stack.push((root_idx, 0));
            }

            while let Some((idx, depth)) = stack.pop() {
                if visited[idx] {
                    continue;
                }
                visited[idx] = true;

                let proc = &self.computed[idx];
                visible_children.clear();
                for &c_idx in self.latest_tree.children_indices(idx) {
                    if in_any_section[c_idx] && self.computed[c_idx].category == category {
                        visible_children.push(c_idx);
                    }
                }

                let has_children = !visible_children.is_empty();
                let is_expanded = has_children
                    && (has_query || (has_expanded && config.process_expanded.contains(&proc.key)));
                let child_count = visible_children.len() as u32;

                rows.push(self.build_process_row(
                    proc,
                    cold_cache,
                    depth,
                    has_children,
                    is_expanded,
                    child_count,
                ));

                if is_expanded {
                    self.sort_process_indices(
                        &mut visible_children,
                        cold_cache,
                        config.process_sort_column,
                        config.process_sort_direction,
                    );
                    for &c_idx in visible_children.iter().rev() {
                        stack.push((c_idx, depth.saturating_add(1)));
                    }
                }
            }
        }

        rows
    }

    /// Windows Task Manager-aligned `Grouped` projection (`PlatformKind::Windows` only).
    ///
    /// - Groups multi-process apps strictly by **app identity** (MSIX/UWP `app_group_id`
    ///   when present, otherwise normalized `exe_path` case-insensitive, falling back to
    ///   executable `name` case-insensitive before slow-lane paths arrive), scoped by
    ///   `session_id`.
    /// - Never pulls different-executable child processes (e.g. `crashpad_handler.exe`,
    ///   `WeChatAppEx.exe`) into an `App` group; those remain in `Background`.
    /// - Every unique `App` identity appears once at depth 0 in `Apps` (never nested
    ///   under `explorer.exe` or another app) with a deterministic leader, total group
    ///   count (`child_count = total_members` when `>= 2`), aggregated resource metrics,
    ///   and rolled-up status flags (`EFFICIENCY_MODE` / `NOT_RESPONDING` on any member,
    ///   `SUSPENDED` only when all members are suspended).
    /// - `explorer.exe` is treated as a desktop shell launcher across all sections and
    ///   never adopts different-image child processes.
    fn project_processes_grouped_windows(
        &self,
        config: &ViewProjectionConfig,
        cold_cache: &ColdMetaCache,
        query_lower: &str,
        has_query: bool,
    ) -> Vec<ProcessRowProjection> {
        let n = self.computed.len();
        let mut app_groups: Vec<(usize, Vec<usize>)> = Vec::new();
        let mut pkg_to_group: HashMap<(u32, AsciiCiStr<'_>), usize, FastBuildHasher> =
            HashMap::with_hasher(FastBuildHasher);
        let mut exe_to_group: HashMap<(u32, AsciiCiStr<'_>), usize, FastBuildHasher> =
            HashMap::with_hasher(FastBuildHasher);
        let mut name_to_groups: HashMap<(u32, AsciiCiStr<'_>), Vec<usize>, FastBuildHasher> =
            HashMap::with_hasher(FastBuildHasher);

        // Step 1: Build unique App-identity groups from all visible-window App processes.
        for (idx, proc) in self.computed.iter().enumerate() {
            if proc.category != ProcessCategory::App {
                continue;
            }
            let pkg = process_app_group_id(cold_cache, proc.key);
            let exe = process_normalized_exe_slice(cold_cache, proc.key);

            let matched_group = pkg
                .and_then(|p| pkg_to_group.get(&(proc.session_id, AsciiCiStr(p))).copied())
                .or_else(|| {
                    exe.and_then(|e| exe_to_group.get(&(proc.session_id, AsciiCiStr(e))).copied())
                })
                .or_else(|| {
                    if proc.name.is_empty() {
                        return None;
                    }
                    name_to_groups
                        .get(&(proc.session_id, AsciiCiStr(&proc.name)))
                        .and_then(|candidates| {
                            candidates.iter().copied().find(|&g_idx| {
                                let leader_key = self.computed[app_groups[g_idx].0].key;
                                exe_paths_compatible(cold_cache, leader_key, proc.key)
                                    && app_group_ids_compatible(cold_cache, leader_key, proc.key)
                            })
                        })
                });

            let g_idx = if let Some(g_idx) = matched_group {
                let (leader_idx, children) = &mut app_groups[g_idx];
                if is_better_app_leader(proc, &self.computed[*leader_idx]) {
                    let prev_leader = *leader_idx;
                    *leader_idx = idx;
                    children.push(prev_leader);
                } else {
                    children.push(idx);
                }
                g_idx
            } else {
                let g_idx = app_groups.len();
                app_groups.push((idx, Vec::new()));
                if !proc.name.is_empty() {
                    name_to_groups
                        .entry((proc.session_id, AsciiCiStr(&proc.name)))
                        .or_default()
                        .push(g_idx);
                }
                g_idx
            };

            if let Some(p) = pkg {
                pkg_to_group
                    .entry((proc.session_id, AsciiCiStr(p)))
                    .or_insert(g_idx);
            }
            if let Some(e) = exe {
                exe_to_group
                    .entry((proc.session_id, AsciiCiStr(e)))
                    .or_insert(g_idx);
            }
        }

        // Step 2: Attach windowless Background processes strictly by matching App identity.
        let mut bg_indices: Vec<usize> = Vec::new();
        let mut sys_indices: Vec<usize> = Vec::new();
        let mut in_remaining_section = vec![false; n];

        for (idx, proc) in self.computed.iter().enumerate() {
            match proc.category {
                ProcessCategory::App => {}
                ProcessCategory::Background => {
                    let pkg = process_app_group_id(cold_cache, proc.key);
                    let exe = process_normalized_exe_slice(cold_cache, proc.key);

                    let matched_group = pkg
                        .and_then(|p| pkg_to_group.get(&(proc.session_id, AsciiCiStr(p))).copied())
                        .or_else(|| {
                            exe.and_then(|e| {
                                exe_to_group.get(&(proc.session_id, AsciiCiStr(e))).copied()
                            })
                        })
                        .or_else(|| {
                            if proc.name.is_empty() || is_windows_shell_launcher(&proc.name) {
                                return None;
                            }
                            name_to_groups
                                .get(&(proc.session_id, AsciiCiStr(&proc.name)))
                                .and_then(|candidates| {
                                    candidates.iter().copied().find(|&g_idx| {
                                        let leader_key = self.computed[app_groups[g_idx].0].key;
                                        exe_paths_compatible(cold_cache, leader_key, proc.key)
                                            && app_group_ids_compatible(
                                                cold_cache, leader_key, proc.key,
                                            )
                                    })
                                })
                        });

                    if let Some(g_idx) = matched_group {
                        app_groups[g_idx].1.push(idx);
                    } else if !has_query || matches_process_query(proc, cold_cache, query_lower) {
                        in_remaining_section[idx] = true;
                        bg_indices.push(idx);
                    }
                }
                ProcessCategory::System => {
                    if !has_query || matches_process_query(proc, cold_cache, query_lower) {
                        in_remaining_section[idx] = true;
                        sys_indices.push(idx);
                    }
                }
            }
        }

        let mut rows = Vec::with_capacity(n + 3);
        let has_expanded = !config.process_expanded.is_empty();

        // 1. Emit Apps section with aggregated metrics and rolled-up status flags.
        let mut visible_apps: Vec<(usize, GroupAggregate, Vec<usize>)> =
            Vec::with_capacity(app_groups.len());
        for (leader_idx, mut children_all) in app_groups {
            let agg = self.compute_group_aggregate(leader_idx, &children_all, cold_cache);
            if has_query {
                let root_matches =
                    matches_process_query(&self.computed[leader_idx], cold_cache, query_lower);
                let matching_children: Vec<usize> = children_all
                    .into_iter()
                    .filter(|&c_idx| {
                        matches_process_query(&self.computed[c_idx], cold_cache, query_lower)
                    })
                    .collect();
                if root_matches || !matching_children.is_empty() {
                    visible_apps.push((leader_idx, agg, matching_children));
                }
            } else {
                visible_apps.push((leader_idx, agg, std::mem::take(&mut children_all)));
            }
        }

        if !visible_apps.is_empty() {
            rows.push(section_header_row(ProcessCategory::App, visible_apps.len()));

            let sort_col = config.process_sort_column;
            let sort_dir = config.process_sort_direction;
            visible_apps.sort_by(|(ia, agg_a, _), (ib, agg_b, _)| {
                let a = &self.computed[*ia];
                let b = &self.computed[*ib];
                let ord =
                    cmp_process_sort_column(a, b, Some(*agg_a), Some(*agg_b), cold_cache, sort_col);
                let directed = match sort_dir {
                    SortDirection::Ascending => ord,
                    SortDirection::Descending => ord.reverse(),
                };
                directed
                    .then_with(|| cmp_str_ascii_ci(&a.name, &b.name))
                    .then_with(|| a.key.cmp(&b.key))
            });

            for (leader_idx, agg, mut children) in visible_apps {
                let leader = &self.computed[leader_idx];
                let has_children = !children.is_empty();
                let is_expanded = has_children
                    && (has_query
                        || (has_expanded && config.process_expanded.contains(&leader.key)));
                // Windows Task Manager displays total process count in the group (leader + children).
                let child_count = if has_children {
                    (children.len() + 1) as u32
                } else {
                    0
                };

                rows.push(self.build_aggregated_process_row(
                    leader,
                    agg,
                    cold_cache,
                    0,
                    has_children,
                    is_expanded,
                    child_count,
                ));

                if is_expanded {
                    self.sort_process_indices(
                        &mut children,
                        cold_cache,
                        config.process_sort_column,
                        config.process_sort_direction,
                    );
                    for c_idx in children {
                        rows.push(self.build_process_row(
                            &self.computed[c_idx],
                            cold_cache,
                            1,
                            false,
                            false,
                            0,
                        ));
                    }
                }
            }
        }

        // 2. Emit remaining Background and System sections.
        let mut visited = vec![false; n];
        let mut stack: Vec<(usize, u16)> = Vec::new();
        let mut section_roots: Vec<usize> = Vec::new();
        let mut visible_children: Vec<usize> = Vec::new();
        let mut subtree_descendants: Vec<usize> = Vec::new();
        let mut desc_stack: Vec<usize> = Vec::new();
        let mut group_agg: Vec<Option<GroupAggregate>> = vec![None; n];
        let mut subtree_counts: Vec<u32> = vec![0; n];

        for (category, indices) in [
            (ProcessCategory::Background, &bg_indices),
            (ProcessCategory::System, &sys_indices),
        ] {
            if indices.is_empty() {
                continue;
            }

            section_roots.clear();
            for &idx in indices {
                let has_same_cat_parent = self.latest_tree.parent_index(idx).is_some_and(|p_idx| {
                    in_remaining_section[p_idx]
                        && self.computed[p_idx].category == category
                        && same_windows_app_identity(
                            &self.computed[p_idx],
                            &self.computed[idx],
                            cold_cache,
                        )
                });
                if !has_same_cat_parent {
                    section_roots.push(idx);
                }
            }

            // Precompute aggregated metrics and total member counts for collapsible section rows.
            for &idx in indices {
                let proc = &self.computed[idx];
                subtree_descendants.clear();
                desc_stack.clear();
                for &c_idx in self.latest_tree.children_indices(idx) {
                    if in_remaining_section[c_idx]
                        && self.computed[c_idx].category == category
                        && same_windows_app_identity(proc, &self.computed[c_idx], cold_cache)
                    {
                        desc_stack.push(c_idx);
                    }
                }
                if !desc_stack.is_empty() {
                    while let Some(d_idx) = desc_stack.pop() {
                        if d_idx == idx || subtree_descendants.len() >= n {
                            continue;
                        }
                        subtree_descendants.push(d_idx);
                        let d_proc = &self.computed[d_idx];
                        for &gc_idx in self.latest_tree.children_indices(d_idx) {
                            if in_remaining_section[gc_idx]
                                && self.computed[gc_idx].category == category
                                && same_windows_app_identity(
                                    d_proc,
                                    &self.computed[gc_idx],
                                    cold_cache,
                                )
                            {
                                desc_stack.push(gc_idx);
                            }
                        }
                    }
                    subtree_counts[idx] = (subtree_descendants.len() + 1) as u32;
                    group_agg[idx] =
                        Some(self.compute_group_aggregate(idx, &subtree_descendants, cold_cache));
                }
            }

            rows.push(section_header_row(category, section_roots.len()));

            self.sort_process_indices_with_aggregates(
                &mut section_roots,
                &group_agg,
                cold_cache,
                config.process_sort_column,
                config.process_sort_direction,
            );

            stack.clear();
            for &root_idx in section_roots.iter().rev() {
                stack.push((root_idx, 0));
            }

            while let Some((idx, depth)) = stack.pop() {
                if visited[idx] {
                    continue;
                }
                visited[idx] = true;

                let proc = &self.computed[idx];
                visible_children.clear();
                for &c_idx in self.latest_tree.children_indices(idx) {
                    if in_remaining_section[c_idx]
                        && self.computed[c_idx].category == category
                        && same_windows_app_identity(proc, &self.computed[c_idx], cold_cache)
                    {
                        visible_children.push(c_idx);
                    }
                }

                let has_children = !visible_children.is_empty();
                let is_expanded = has_children
                    && (has_query || (has_expanded && config.process_expanded.contains(&proc.key)));
                let child_count = if has_children {
                    subtree_counts[idx].max((visible_children.len() + 1) as u32)
                } else {
                    0
                };

                if let Some(agg) = group_agg[idx] {
                    rows.push(self.build_aggregated_process_row(
                        proc,
                        agg,
                        cold_cache,
                        depth,
                        has_children,
                        is_expanded,
                        child_count,
                    ));
                } else {
                    rows.push(self.build_process_row(
                        proc,
                        cold_cache,
                        depth,
                        has_children,
                        is_expanded,
                        child_count,
                    ));
                }

                if is_expanded {
                    self.sort_process_indices_with_aggregates(
                        &mut visible_children,
                        &group_agg,
                        cold_cache,
                        config.process_sort_column,
                        config.process_sort_direction,
                    );
                    for &c_idx in visible_children.iter().rev() {
                        stack.push((c_idx, depth.saturating_add(1)));
                    }
                }
            }
        }

        rows
    }

    fn project_processes_tree(
        &self,
        config: &ViewProjectionConfig,
        cold_cache: &ColdMetaCache,
        query_lower: &str,
        has_query: bool,
    ) -> Vec<ProcessRowProjection> {
        let n = self.computed.len();
        let included = if has_query {
            Some(self.collect_matching_with_ancestors(|proc| {
                matches_process_query(proc, cold_cache, query_lower)
            }))
        } else {
            None
        };

        let mut roots: Vec<usize> = self
            .latest_tree
            .root_indices()
            .iter()
            .copied()
            .filter(|&idx| included.as_ref().is_none_or(|mask| mask[idx]))
            .collect();

        self.sort_process_indices(
            &mut roots,
            cold_cache,
            config.process_sort_column,
            config.process_sort_direction,
        );

        let mut rows = Vec::with_capacity(n);
        let mut visited = vec![false; n];
        let mut stack: Vec<(usize, u16)> = Vec::with_capacity(roots.len());
        let mut visible_children: Vec<usize> = Vec::new();
        let has_expanded = !config.process_expanded.is_empty();

        for &root_idx in roots.iter().rev() {
            stack.push((root_idx, 0));
        }

        while let Some((idx, depth)) = stack.pop() {
            if visited[idx] {
                continue;
            }
            visited[idx] = true;

            let proc = &self.computed[idx];
            visible_children.clear();
            for &c_idx in self.latest_tree.children_indices(idx) {
                if included.as_ref().is_none_or(|mask| mask[c_idx]) {
                    visible_children.push(c_idx);
                }
            }

            let has_children = !visible_children.is_empty();
            let is_expanded = has_children
                && (has_query || (has_expanded && config.process_expanded.contains(&proc.key)));
            let child_count = visible_children.len() as u32;

            rows.push(self.build_process_row(
                proc,
                cold_cache,
                depth,
                has_children,
                is_expanded,
                child_count,
            ));

            if is_expanded {
                self.sort_process_indices(
                    &mut visible_children,
                    cold_cache,
                    config.process_sort_column,
                    config.process_sort_direction,
                );
                for &c_idx in visible_children.iter().rev() {
                    stack.push((c_idx, depth.saturating_add(1)));
                }
            }
        }

        rows
    }

    fn project_details_flat(
        &self,
        config: &ViewProjectionConfig,
        cold_cache: &ColdMetaCache,
        query_lower: &str,
        has_query: bool,
    ) -> Vec<DetailsRowProjection> {
        let mut indices: Vec<usize> = (0..self.computed.len())
            .filter(|&idx| {
                !has_query || matches_details_query(&self.computed[idx], cold_cache, query_lower)
            })
            .collect();

        self.sort_details_indices(
            &mut indices,
            cold_cache,
            config.details_sort_column,
            config.details_sort_direction,
        );

        indices
            .into_iter()
            .map(|idx| self.build_details_row(idx, cold_cache, 0, false, false))
            .collect()
    }

    fn project_details_tree(
        &self,
        config: &ViewProjectionConfig,
        cold_cache: &ColdMetaCache,
        query_lower: &str,
        has_query: bool,
    ) -> Vec<DetailsRowProjection> {
        let n = self.computed.len();
        let included = if has_query {
            Some(self.collect_matching_with_ancestors(|proc| {
                matches_details_query(proc, cold_cache, query_lower)
            }))
        } else {
            None
        };

        let mut roots: Vec<usize> = self
            .latest_tree
            .root_indices()
            .iter()
            .copied()
            .filter(|&idx| included.as_ref().is_none_or(|mask| mask[idx]))
            .collect();

        self.sort_details_indices(
            &mut roots,
            cold_cache,
            config.details_sort_column,
            config.details_sort_direction,
        );

        let mut rows = Vec::with_capacity(n);
        let mut visited = vec![false; n];
        let mut stack: Vec<(usize, u16)> = Vec::with_capacity(roots.len());
        let mut visible_children: Vec<usize> = Vec::new();
        let has_expanded = !config.details_expanded.is_empty();

        for &root_idx in roots.iter().rev() {
            stack.push((root_idx, 0));
        }

        while let Some((idx, depth)) = stack.pop() {
            if visited[idx] {
                continue;
            }
            visited[idx] = true;

            let key = self.computed[idx].key;
            visible_children.clear();
            for &c_idx in self.latest_tree.children_indices(idx) {
                if included.as_ref().is_none_or(|mask| mask[c_idx]) {
                    visible_children.push(c_idx);
                }
            }

            let has_children = !visible_children.is_empty();
            let is_expanded = has_children
                && (has_query || (has_expanded && config.details_expanded.contains(&key)));

            rows.push(self.build_details_row(idx, cold_cache, depth, has_children, is_expanded));

            if is_expanded {
                self.sort_details_indices(
                    &mut visible_children,
                    cold_cache,
                    config.details_sort_column,
                    config.details_sort_direction,
                );
                for &c_idx in visible_children.iter().rev() {
                    stack.push((c_idx, depth.saturating_add(1)));
                }
            }
        }

        rows
    }

    fn collect_matching_with_ancestors<F>(&self, predicate: F) -> Vec<bool>
    where
        F: Fn(&ComputedProcess) -> bool,
    {
        let mut included = vec![false; self.computed.len()];
        for (idx, proc) in self.computed.iter().enumerate() {
            if predicate(proc) {
                let mut curr = idx;
                while !included[curr] {
                    included[curr] = true;
                    if let Some(p_idx) = self.latest_tree.parent_index(curr) {
                        curr = p_idx;
                    } else {
                        break;
                    }
                }
            }
        }
        included
    }

    fn sort_process_indices_with_aggregates(
        &self,
        indices: &mut [usize],
        group_agg: &[Option<GroupAggregate>],
        cold_cache: &ColdMetaCache,
        column: ProcessSortColumn,
        direction: SortDirection,
    ) {
        indices.sort_by(|&ia, &ib| {
            let a = &self.computed[ia];
            let b = &self.computed[ib];
            let ord =
                cmp_process_sort_column(a, b, group_agg[ia], group_agg[ib], cold_cache, column);
            let directed = match direction {
                SortDirection::Ascending => ord,
                SortDirection::Descending => ord.reverse(),
            };
            directed
                .then_with(|| cmp_str_ascii_ci(&a.name, &b.name))
                .then_with(|| a.key.cmp(&b.key))
        });
    }

    fn sort_process_indices(
        &self,
        indices: &mut [usize],
        cold_cache: &ColdMetaCache,
        column: ProcessSortColumn,
        direction: SortDirection,
    ) {
        indices.sort_by(|&ia, &ib| {
            let a = &self.computed[ia];
            let b = &self.computed[ib];
            let ord = cmp_process_sort_column(a, b, None, None, cold_cache, column);
            let directed = match direction {
                SortDirection::Ascending => ord,
                SortDirection::Descending => ord.reverse(),
            };
            directed
                .then_with(|| cmp_str_ascii_ci(&a.name, &b.name))
                .then_with(|| a.key.cmp(&b.key))
        });
    }

    fn sort_details_indices(
        &self,
        indices: &mut [usize],
        cold_cache: &ColdMetaCache,
        column: DetailsSortColumn,
        direction: SortDirection,
    ) {
        indices.sort_by(|&ia, &ib| {
            let a = &self.computed[ia];
            let b = &self.computed[ib];
            let ca = cold_cache.get_process(a.key);
            let cb = cold_cache.get_process(b.key);
            let ord = match column {
                DetailsSortColumn::Name => cmp_str_ascii_ci(&a.name, &b.name),
                DetailsSortColumn::Pid => a.key.pid.cmp(&b.key.pid),
                DetailsSortColumn::Status => effective_status(a, cold_cache)
                    .contains(ProcessStatusFlags::SUSPENDED)
                    .cmp(&effective_status(b, cold_cache).contains(ProcessStatusFlags::SUSPENDED)),
                DetailsSortColumn::UserName => {
                    let ua = display_user_for_sort(ca.and_then(|m| m.user_name.as_deref()));
                    let ub = display_user_for_sort(cb.and_then(|m| m.user_name.as_deref()));
                    cmp_str_ascii_ci(ua, ub)
                }
                DetailsSortColumn::SessionId => a.session_id.cmp(&b.session_id),
                DetailsSortColumn::Cpu => a.cpu_permille.cmp(&b.cpu_permille),
                DetailsSortColumn::CpuTime => a.cpu_time_100ns.cmp(&b.cpu_time_100ns),
                DetailsSortColumn::Memory => a
                    .private_working_set_bytes
                    .cmp(&b.private_working_set_bytes),
                DetailsSortColumn::WorkingSet => a.working_set_bytes.cmp(&b.working_set_bytes),
                DetailsSortColumn::CommitSize => a.commit_bytes.cmp(&b.commit_bytes),
                DetailsSortColumn::Priority => a.priority.cmp(&b.priority),
                DetailsSortColumn::Handles => a.handle_count.cmp(&b.handle_count),
                DetailsSortColumn::Threads => a.thread_count.cmp(&b.thread_count),
                DetailsSortColumn::Arch => {
                    let aa = arch_rank(ca.map_or(ProcessArch::Unknown, |m| m.arch));
                    let ab = arch_rank(cb.map_or(ProcessArch::Unknown, |m| m.arch));
                    aa.cmp(&ab)
                }
                DetailsSortColumn::UacVirtualization => {
                    let va =
                        uac_rank(ca.map_or(UacVirtualization::Unknown, |m| m.uac_virtualization));
                    let vb =
                        uac_rank(cb.map_or(UacVirtualization::Unknown, |m| m.uac_virtualization));
                    va.cmp(&vb)
                }
                DetailsSortColumn::Elevated => effective_status(a, cold_cache)
                    .contains(ProcessStatusFlags::ELEVATED)
                    .cmp(&effective_status(b, cold_cache).contains(ProcessStatusFlags::ELEVATED)),
                DetailsSortColumn::GpuMemory => a.gpu_dedicated_bytes.cmp(&b.gpu_dedicated_bytes),
                DetailsSortColumn::Publisher => {
                    let pa = ca
                        .and_then(|m| m.asset.as_ref())
                        .and_then(|asset| asset.publisher.as_deref())
                        .unwrap_or("");
                    let pb = cb
                        .and_then(|m| m.asset.as_ref())
                        .and_then(|asset| asset.publisher.as_deref())
                        .unwrap_or("");
                    cmp_str_ascii_ci(pa, pb)
                }
                DetailsSortColumn::Description => {
                    let da = ca
                        .and_then(|m| m.asset.as_ref())
                        .and_then(|asset| asset.file_description.as_deref())
                        .unwrap_or("");
                    let db = cb
                        .and_then(|m| m.asset.as_ref())
                        .and_then(|asset| asset.file_description.as_deref())
                        .unwrap_or("");
                    cmp_str_ascii_ci(da, db)
                }
                DetailsSortColumn::ExePath => {
                    let ea = ca.and_then(|m| m.exe_path.as_deref()).unwrap_or("");
                    let eb = cb.and_then(|m| m.exe_path.as_deref()).unwrap_or("");
                    cmp_str_ascii_ci(ea, eb)
                }
                DetailsSortColumn::CommandLine => {
                    let cmd_a = ca.and_then(|m| m.command_line.as_deref()).unwrap_or("");
                    let cmd_b = cb.and_then(|m| m.command_line.as_deref()).unwrap_or("");
                    cmp_str_ascii_ci(cmd_a, cmd_b)
                }
            };
            let directed = match direction {
                SortDirection::Ascending => ord,
                SortDirection::Descending => ord.reverse(),
            };
            directed
                .then_with(|| cmp_str_ascii_ci(&a.name, &b.name))
                .then_with(|| a.key.cmp(&b.key))
        });
    }
}

#[inline]
fn cmp_process_sort_column(
    a: &ComputedProcess,
    b: &ComputedProcess,
    agg_a: Option<GroupAggregate>,
    agg_b: Option<GroupAggregate>,
    cold_cache: &ColdMetaCache,
    column: ProcessSortColumn,
) -> Ordering {
    match column {
        ProcessSortColumn::Name => {
            let da = process_display_name_ref(a, cold_cache);
            let db = process_display_name_ref(b, cold_cache);
            cmp_str_ascii_ci(da, db)
        }
        ProcessSortColumn::Category => category_index(a.category).cmp(&category_index(b.category)),
        ProcessSortColumn::Pid => a.key.pid.cmp(&b.key.pid),
        ProcessSortColumn::ProcessName => cmp_str_ascii_ci(&a.name, &b.name),
        ProcessSortColumn::Status => {
            let sa = agg_a.map_or_else(|| effective_status(a, cold_cache), |g| g.status);
            let sb = agg_b.map_or_else(|| effective_status(b, cold_cache), |g| g.status);
            sa.bits().cmp(&sb.bits())
        }
        ProcessSortColumn::Cpu => agg_a
            .map_or(a.cpu_permille, |g| g.cpu_permille)
            .cmp(&agg_b.map_or(b.cpu_permille, |g| g.cpu_permille)),
        ProcessSortColumn::Memory => agg_a
            .map_or(a.private_working_set_bytes, |g| g.memory_bytes)
            .cmp(&agg_b.map_or(b.private_working_set_bytes, |g| g.memory_bytes)),
        ProcessSortColumn::Disk => agg_a
            .map_or(a.disk_bytes_per_sec, |g| g.disk_bytes_per_sec)
            .cmp(&agg_b.map_or(b.disk_bytes_per_sec, |g| g.disk_bytes_per_sec)),
        ProcessSortColumn::Network => agg_a
            .map_or(a.net_bytes_per_sec, |g| g.net_bytes_per_sec)
            .cmp(&agg_b.map_or(b.net_bytes_per_sec, |g| g.net_bytes_per_sec)),
        ProcessSortColumn::Gpu => {
            let ga = agg_a.map_or((a.gpu_util_permille, a.gpu_dedicated_bytes), |g| {
                (g.gpu_util_permille, g.gpu_dedicated_bytes)
            });
            let gb = agg_b.map_or((b.gpu_util_permille, b.gpu_dedicated_bytes), |g| {
                (g.gpu_util_permille, g.gpu_dedicated_bytes)
            });
            ga.cmp(&gb)
        }
        ProcessSortColumn::Publisher => {
            let pa = cold_cache
                .get_process(a.key)
                .and_then(|m| m.asset.as_ref())
                .and_then(|asset| asset.publisher.as_deref())
                .unwrap_or("");
            let pb = cold_cache
                .get_process(b.key)
                .and_then(|m| m.asset.as_ref())
                .and_then(|asset| asset.publisher.as_deref())
                .unwrap_or("");
            cmp_str_ascii_ci(pa, pb)
        }
        ProcessSortColumn::CommandLine => {
            let ca = cold_cache
                .get_process(a.key)
                .and_then(|m| m.command_line.as_deref())
                .unwrap_or("");
            let cb = cold_cache
                .get_process(b.key)
                .and_then(|m| m.command_line.as_deref())
                .unwrap_or("");
            cmp_str_ascii_ci(ca, cb)
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct GroupAggregate {
    cpu_permille: u16,
    memory_bytes: u64,
    memory_permille: u16,
    disk_bytes_per_sec: u64,
    net_bytes_per_sec: u64,
    gpu_util_permille: u16,
    gpu_dedicated_bytes: u64,
    status: ProcessStatusFlags,
}

#[derive(Clone, Copy)]
struct AsciiCiStr<'a>(&'a str);

impl PartialEq for AsciiCiStr<'_> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.0.eq_ignore_ascii_case(other.0)
    }
}

impl Eq for AsciiCiStr<'_> {}

impl std::hash::Hash for AsciiCiStr<'_> {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        for b in self.0.bytes() {
            state.write_u8(b.to_ascii_lowercase());
        }
        state.write_u8(0xff);
    }
}

#[inline]
fn is_windows_shell_launcher(image_name: &str) -> bool {
    image_name.eq_ignore_ascii_case("explorer.exe")
}

#[inline]
fn process_app_group_id(cold_cache: &ColdMetaCache, key: ProcessKey) -> Option<&str> {
    cold_cache
        .get_process(key)
        .and_then(|m| m.app_group_id.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

#[inline]
fn process_normalized_exe_slice(cold_cache: &ColdMetaCache, key: ProcessKey) -> Option<&str> {
    cold_cache
        .get_process(key)
        .and_then(|m| m.exe_path.as_deref())
        .map(str::trim)
        .map(|p| p.strip_prefix(r"\\?\").unwrap_or(p))
        .filter(|s| !s.is_empty())
}

#[inline]
fn exe_paths_compatible(
    cold_cache: &ColdMetaCache,
    app_key: ProcessKey,
    helper_key: ProcessKey,
) -> bool {
    match (
        process_normalized_exe_slice(cold_cache, app_key),
        process_normalized_exe_slice(cold_cache, helper_key),
    ) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => true,
    }
}

#[inline]
fn app_group_ids_compatible(
    cold_cache: &ColdMetaCache,
    app_key: ProcessKey,
    helper_key: ProcessKey,
) -> bool {
    match (
        process_app_group_id(cold_cache, app_key),
        process_app_group_id(cold_cache, helper_key),
    ) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => true,
    }
}

#[inline]
fn same_windows_app_identity(
    a: &ComputedProcess,
    b: &ComputedProcess,
    cold_cache: &ColdMetaCache,
) -> bool {
    if a.session_id != b.session_id {
        return false;
    }
    if let (Some(pa), Some(pb)) = (
        process_app_group_id(cold_cache, a.key),
        process_app_group_id(cold_cache, b.key),
    ) {
        return pa.eq_ignore_ascii_case(pb);
    }
    if let (Some(ea), Some(eb)) = (
        process_normalized_exe_slice(cold_cache, a.key),
        process_normalized_exe_slice(cold_cache, b.key),
    ) {
        return ea.eq_ignore_ascii_case(eb);
    }
    !a.name.is_empty()
        && a.name.eq_ignore_ascii_case(&b.name)
        && exe_paths_compatible(cold_cache, a.key, b.key)
        && app_group_ids_compatible(cold_cache, a.key, b.key)
}

#[inline]
fn is_better_app_leader(cand: &ComputedProcess, curr: &ComputedProcess) -> bool {
    let cand_has_title = cand
        .window_title
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty());
    let curr_has_title = curr
        .window_title
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty());
    if cand_has_title != curr_has_title {
        return cand_has_title;
    }
    (cand.key.create_time_100ns, cand.key.pid) < (curr.key.create_time_100ns, curr.key.pid)
}

fn section_header_row(category: ProcessCategory, count: usize) -> ProcessRowProjection {
    let section_name: Arc<str> = Arc::from(category_label(category));
    ProcessRowProjection {
        kind: ProcessRowKind::SectionHeader { category, count },
        key: ProcessKey::default(),
        depth: 0,
        has_children: false,
        is_expanded: true,
        child_count: count as u32,
        name: Arc::clone(&section_name),
        display_name: section_name,
        window_title: None,
        publisher: None,
        exe_path: None,
        command_line: None,
        icon: None,
        category,
        status: ProcessStatusFlags::empty(),
        cpu_permille: 0,
        memory_bytes: 0,
        memory_permille: 0,
        disk_bytes_per_sec: 0,
        disk_permille: 0,
        net_bytes_per_sec: 0,
        net_permille: 0,
        gpu_util_permille: 0,
        gpu_dedicated_bytes: 0,
        parent_key: None,
        leak_warning: None,
        webview2_role: None,
    }
}

#[inline]
fn display_user_for_sort(raw: Option<&str>) -> &str {
    let Some(name) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return "";
    };
    match name.rsplit_once('\\') {
        Some((_, account)) if !account.is_empty() => account,
        _ => name,
    }
}

#[inline]
fn resolve_display_name(proc: &ComputedProcess, cold_cache: &ColdMetaCache) -> Arc<str> {
    cold_cache
        .get_process(proc.key)
        .and_then(|meta| meta.asset.as_ref())
        .and_then(|asset| asset.file_description.as_ref())
        .filter(|desc| !desc.trim().is_empty())
        .cloned()
        .or_else(|| proc.window_title.clone())
        .unwrap_or_else(|| Arc::clone(&proc.name))
}

#[inline]
fn effective_status(proc: &ComputedProcess, cold_cache: &ColdMetaCache) -> ProcessStatusFlags {
    let mut status = proc.status;
    if cold_cache.efficiency_mode(proc.key) {
        status |= ProcessStatusFlags::EFFICIENCY_MODE;
    }
    if cold_cache.is_elevated(proc.key) {
        status |= ProcessStatusFlags::ELEVATED;
    }
    status
}

#[inline]
fn compute_throughput_permille(bytes_per_sec: u64, total_bytes_per_sec: u64) -> u16 {
    if bytes_per_sec == 0 || total_bytes_per_sec == 0 {
        return 0;
    }
    let total = u128::from(total_bytes_per_sec);
    ((u128::from(bytes_per_sec) * 1_000 + (total / 2)) / total).min(1_000) as u16
}

#[inline]
fn ratio_permille(used: u64, total: u64) -> u16 {
    if used == 0 || total == 0 {
        return 0;
    }
    ((u128::from(used) * 1_000) / u128::from(total)).min(1_000) as u16
}

#[inline]
fn category_index(category: ProcessCategory) -> usize {
    match category {
        ProcessCategory::App => 0,
        ProcessCategory::Background => 1,
        ProcessCategory::System => 2,
    }
}

#[inline]
const fn category_label(category: ProcessCategory) -> &'static str {
    match category {
        ProcessCategory::App => "Apps",
        ProcessCategory::Background => "Background",
        ProcessCategory::System => "System",
    }
}

#[inline]
const fn arch_rank(arch: ProcessArch) -> u8 {
    match arch {
        ProcessArch::Unknown => 0,
        ProcessArch::X86 => 1,
        ProcessArch::X64 => 2,
        ProcessArch::Arm64 => 3,
    }
}

#[inline]
const fn uac_rank(uac: UacVirtualization) -> u8 {
    match uac {
        UacVirtualization::Unknown => 0,
        UacVirtualization::NotAllowed => 1,
        UacVirtualization::Disabled => 2,
        UacVirtualization::Enabled => 3,
    }
}

#[inline]
fn process_display_name_ref<'a>(
    proc: &'a ComputedProcess,
    cold_cache: &'a ColdMetaCache,
) -> &'a str {
    if let Some(desc) = cold_cache
        .get_process(proc.key)
        .and_then(|m| m.asset.as_ref())
        .and_then(|a| a.file_description.as_deref())
        .filter(|d| !d.trim().is_empty())
    {
        return desc;
    }
    if let Some(ref title) = proc.window_title {
        return title;
    }
    &proc.name
}

fn matches_process_query(
    proc: &ComputedProcess,
    cold_cache: &ColdMetaCache,
    query_lower: &str,
) -> bool {
    if pid_matches_query(proc.key.pid, query_lower) {
        return true;
    }
    if contains_case_insensitive(&proc.name, query_lower) {
        return true;
    }
    if let Some(ref title) = proc.window_title
        && contains_case_insensitive(title, query_lower)
    {
        return true;
    }
    if let Some(asset) = cold_cache
        .get_process(proc.key)
        .and_then(|m| m.asset.as_ref())
    {
        if let Some(ref desc) = asset.file_description
            && contains_case_insensitive(desc, query_lower)
        {
            return true;
        }
        if let Some(ref pub_name) = asset.publisher
            && contains_case_insensitive(pub_name, query_lower)
        {
            return true;
        }
    }
    false
}

fn matches_details_query(
    proc: &ComputedProcess,
    cold_cache: &ColdMetaCache,
    query_lower: &str,
) -> bool {
    if matches_process_query(proc, cold_cache, query_lower) {
        return true;
    }
    if let Some(cold) = cold_cache.get_process(proc.key) {
        if let Some(ref user) = cold.user_name
            && contains_case_insensitive(user, query_lower)
        {
            return true;
        }
        if let Some(ref exe) = cold.exe_path
            && contains_case_insensitive(exe, query_lower)
        {
            return true;
        }
        if let Some(ref cmd) = cold.command_line
            && contains_case_insensitive(cmd, query_lower)
        {
            return true;
        }
    }
    false
}

fn pid_matches_query(mut pid: u32, query_lower: &str) -> bool {
    if query_lower.is_empty() || !query_lower.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    // Stack-allocated decimal formatting (u32::MAX has 10 digits).
    let mut buf = [0u8; 10];
    let mut idx = buf.len();
    if pid == 0 {
        idx -= 1;
        buf[idx] = b'0';
    } else {
        while pid > 0 {
            idx -= 1;
            buf[idx] = b'0' + (pid % 10) as u8;
            pid /= 10;
        }
    }
    let pid_str = std::str::from_utf8(&buf[idx..]).unwrap_or("");
    pid_str.contains(query_lower)
}

fn contains_case_insensitive(haystack: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if haystack.is_ascii() && needle_lower.is_ascii() {
        let h = haystack.as_bytes();
        let n = needle_lower.as_bytes();
        if n.len() > h.len() {
            return false;
        }
        return h.windows(n.len()).any(|window| {
            window
                .iter()
                .zip(n.iter())
                .all(|(&hb, &nb)| hb.to_ascii_lowercase() == nb)
        });
    }
    haystack.to_lowercase().contains(needle_lower)
}

#[inline]
fn cmp_str_ascii_ci(a: &str, b: &str) -> Ordering {
    for (ba, bb) in a.bytes().zip(b.bytes()) {
        let ord = ba.to_ascii_lowercase().cmp(&bb.to_ascii_lowercase());
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

#[cfg(test)]
mod tests {
    use loom_types::ProcessHotSample;

    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn make_sample(
        scratch: &mut ProcessSampleScratch<()>,
        pid: u32,
        create_time: u64,
        parent_pid: u32,
        name: &str,
        category: ProcessCategory,
        cpu_100ns: u64,
        disk_bytes: u64,
    ) {
        let (name_offset, name_len) = scratch.push_str(name);
        scratch.samples.push(ProcessHotSample {
            key: ProcessKey::new(pid, create_time),
            parent_pid,
            kernel_time_100ns: cpu_100ns / 2,
            user_time_100ns: cpu_100ns - cpu_100ns / 2,
            private_working_set_bytes: 100 * 1024 * 1024,
            disk_read_bytes: disk_bytes,
            category,
            name_offset,
            name_len,
            ..ProcessHotSample::default()
        });
    }

    #[test]
    fn delta_calculator_computes_accurate_cpu_and_disk_rates() {
        let mut engine = ProjectionEngine::new();
        let mut scratch = ProcessSampleScratch::<()>::new();
        scratch.perf.logical_cpu_count = 4;
        scratch.perf.mem_total_bytes = 1_000 * 1024 * 1024;
        make_sample(
            &mut scratch,
            42,
            100,
            0,
            "worker.exe",
            ProcessCategory::App,
            10_000_000,
            1_000_000,
        );

        let t0 = Timestamp100Ns::new(100_000_000);
        engine.ingest_process_sample(t0, &scratch, 4);

        let cold = ColdMetaCache::new();
        let cfg = ViewProjectionConfig::default();
        let s0 = engine.project_processes(&cfg, &cold, 1);
        let row0 = s0
            .rows
            .iter()
            .find(|r| r.kind == ProcessRowKind::Process)
            .unwrap();
        assert_eq!(row0.cpu_permille, 0);
        assert_eq!(row0.disk_bytes_per_sec, 0);

        // +250 ms elapsed (2_500_000 ticks), 4 CPUs => 10_000_000 ticks = 100% total CPU.
        // Give worker.exe 2_500_000 CPU ticks => 25.0% (250 permille) and +500_000 disk bytes => 2_000_000 B/s.
        scratch.clear();
        scratch.perf.logical_cpu_count = 4;
        scratch.perf.mem_total_bytes = 1_000 * 1024 * 1024;
        make_sample(
            &mut scratch,
            42,
            100,
            0,
            "worker.exe",
            ProcessCategory::App,
            12_500_000,
            1_500_000,
        );
        let t1 = Timestamp100Ns::new(102_500_000);
        engine.ingest_process_sample(t1, &scratch, 4);

        let s1 = engine.project_processes(&cfg, &cold, 2);
        let row1 = s1
            .rows
            .iter()
            .find(|r| r.kind == ProcessRowKind::Process)
            .unwrap();
        assert_eq!(row1.cpu_permille, 250);
        assert_eq!(row1.disk_bytes_per_sec, 2_000_000);
        assert_eq!(row1.memory_permille, 100);
    }

    #[test]
    fn search_in_tree_mode_auto_expands_ancestors() {
        let mut engine = ProjectionEngine::new();
        let mut scratch = ProcessSampleScratch::<()>::new();
        scratch.perf.logical_cpu_count = 4;
        make_sample(
            &mut scratch,
            1,
            10,
            0,
            "root.exe",
            ProcessCategory::System,
            0,
            0,
        );
        make_sample(
            &mut scratch,
            2,
            20,
            1,
            "mid.exe",
            ProcessCategory::Background,
            0,
            0,
        );
        make_sample(
            &mut scratch,
            3,
            30,
            2,
            "target_leaf.exe",
            ProcessCategory::App,
            0,
            0,
        );
        make_sample(
            &mut scratch,
            4,
            40,
            0,
            "unrelated.exe",
            ProcessCategory::App,
            0,
            0,
        );

        engine.ingest_process_sample(Timestamp100Ns::new(1_000_000), &scratch, 4);

        let cold = ColdMetaCache::new();
        let mut cfg = ViewProjectionConfig {
            process_view_mode: ProcessViewMode::Tree,
            search_query: "target_leaf".to_owned(),
            ..ViewProjectionConfig::default()
        };

        let slice = engine.project_processes(&cfg, &cold, 1);
        let names: Vec<&str> = slice.rows.iter().map(|r| r.name.as_ref()).collect();
        assert_eq!(names, vec!["root.exe", "mid.exe", "target_leaf.exe"]);
        assert_eq!(slice.rows[0].depth, 0);
        assert_eq!(slice.rows[1].depth, 1);
        assert_eq!(slice.rows[2].depth, 2);

        // Search by PID also matches and auto-expands ancestors.
        cfg.search_query = "3".to_owned();
        let by_pid = engine.project_processes(&cfg, &cold, 2);
        assert!(by_pid.rows.iter().any(|r| r.key.pid == 3));
    }

    #[allow(clippy::too_many_arguments)]
    fn make_detailed_sample(
        scratch: &mut ProcessSampleScratch<()>,
        pid: u32,
        create_time: u64,
        parent_pid: u32,
        session_id: u32,
        name: &str,
        category: ProcessCategory,
        status: ProcessStatusFlags,
        cpu_100ns: u64,
        mem_bytes: u64,
        disk_bytes: u64,
    ) {
        let (name_offset, name_len) = scratch.push_str(name);
        scratch.samples.push(ProcessHotSample {
            key: ProcessKey::new(pid, create_time),
            parent_pid,
            session_id,
            kernel_time_100ns: cpu_100ns / 2,
            user_time_100ns: cpu_100ns - cpu_100ns / 2,
            private_working_set_bytes: mem_bytes,
            disk_read_bytes: disk_bytes,
            category,
            status,
            name_offset,
            name_len,
            ..ProcessHotSample::default()
        });
    }

    #[test]
    fn windows_grouped_un_nests_apps_and_background_daemons_from_explorer() {
        let mut engine = ProjectionEngine::with_platform(PlatformProfile::WINDOWS);
        let mut scratch = ProcessSampleScratch::<()>::new();
        scratch.perf.logical_cpu_count = 4;
        scratch.perf.mem_total_bytes = 16 * 1024 * 1024 * 1024;

        // explorer.exe spawns independent GUI apps and a background tray daemon.
        make_sample(
            &mut scratch,
            100,
            10,
            0,
            "explorer.exe",
            ProcessCategory::App,
            0,
            0,
        );
        for (pid, name) in [
            (200, "Antigravity.exe"),
            (201, "chrome.exe"),
            (202, "loom-app.exe"),
            (203, "QQ.exe"),
            (204, "Weixin.exe"),
        ] {
            make_sample(
                &mut scratch,
                pid,
                20 + u64::from(pid),
                100,
                name,
                ProcessCategory::App,
                0,
                0,
            );
        }
        make_sample(
            &mut scratch,
            300,
            500,
            100,
            "OneDrive.exe",
            ProcessCategory::Background,
            0,
            0,
        );

        engine.ingest_process_sample(Timestamp100Ns::new(1_000_000), &scratch, 4);

        let cold = ColdMetaCache::new();
        let cfg = ViewProjectionConfig {
            process_view_mode: ProcessViewMode::Grouped,
            process_sort_column: ProcessSortColumn::Name,
            process_sort_direction: SortDirection::Ascending,
            ..ViewProjectionConfig::default()
        };

        let slice = engine.project_processes(&cfg, &cold, 1);

        // Apps section header reports all 6 top-level apps, including explorer.exe.
        assert_eq!(
            slice.rows[0].kind,
            ProcessRowKind::SectionHeader {
                category: ProcessCategory::App,
                count: 6
            }
        );

        let app_rows: Vec<&ProcessRowProjection> = slice.rows[1..7].iter().collect();
        for row in &app_rows {
            assert_eq!(row.kind, ProcessRowKind::Process);
            assert_eq!(row.depth, 0, "{} must be top-level (depth 0)", row.name);
            assert!(!row.has_children);
            assert_eq!(row.child_count, 0);
        }
        let explorer = app_rows
            .iter()
            .find(|r| r.name.eq_ignore_ascii_case("explorer.exe"))
            .expect("explorer.exe must appear in Apps");
        assert!(!explorer.has_children);
        assert_eq!(explorer.child_count, 0);

        // Background daemon spawned by explorer.exe remains in Background, not under explorer.exe.
        assert_eq!(
            slice.rows[7].kind,
            ProcessRowKind::SectionHeader {
                category: ProcessCategory::Background,
                count: 1
            }
        );
        assert_eq!(slice.rows[8].name.as_ref(), "OneDrive.exe");
        assert_eq!(slice.rows[8].depth, 0);
    }

    #[test]
    fn windows_grouped_never_nests_under_windowless_explorer() {
        let mut engine = ProjectionEngine::with_platform(PlatformProfile::WINDOWS);
        let mut scratch = ProcessSampleScratch::<()>::new();
        scratch.perf.logical_cpu_count = 4;

        // When no File Explorer folder window is open, explorer.exe can be Background or System.
        // In either case it must NEVER adopt different-image child processes.
        make_sample(
            &mut scratch,
            100,
            10,
            0,
            "explorer.exe",
            ProcessCategory::Background,
            0,
            0,
        );
        make_sample(
            &mut scratch,
            200,
            20,
            100,
            "chrome.exe",
            ProcessCategory::Background,
            0,
            0,
        );
        make_sample(
            &mut scratch,
            201,
            30,
            200,
            "chrome.exe",
            ProcessCategory::Background,
            0,
            0,
        );
        make_sample(
            &mut scratch,
            300,
            40,
            100,
            "Weixin.exe",
            ProcessCategory::Background,
            0,
            0,
        );

        engine.ingest_process_sample(Timestamp100Ns::new(1_000_000), &scratch, 4);

        let cold = ColdMetaCache::new();
        let cfg = ViewProjectionConfig {
            process_view_mode: ProcessViewMode::Grouped,
            process_sort_column: ProcessSortColumn::Name,
            process_sort_direction: SortDirection::Ascending,
            ..ViewProjectionConfig::default()
        };

        let slice = engine.project_processes(&cfg, &cold, 1);
        assert_eq!(
            slice.rows[0].kind,
            ProcessRowKind::SectionHeader {
                category: ProcessCategory::Background,
                count: 3
            }
        );

        let names: Vec<(&str, u32, u64)> = slice.rows[1..]
            .iter()
            .map(|r| (r.name.as_ref(), r.child_count, r.memory_bytes))
            .collect();
        let mib = 1024 * 1024;
        assert_eq!(
            names,
            vec![
                ("chrome.exe", 2, 200 * mib),
                ("explorer.exe", 0, 100 * mib),
                ("Weixin.exe", 0, 100 * mib),
            ]
        );
    }

    #[test]
    fn windows_grouped_aggregates_by_app_identity_and_excludes_different_exes() {
        let mut engine = ProjectionEngine::with_platform(PlatformProfile::WINDOWS);
        let mib = 1024 * 1024;
        let populate = |s: &mut ProcessSampleScratch<()>, tick: u64| {
            s.clear();
            s.perf.logical_cpu_count = 4;
            s.perf.mem_total_bytes = 2_000 * mib;

            // Shell launcher.
            make_detailed_sample(
                s,
                100,
                10,
                0,
                1,
                "explorer.exe",
                ProcessCategory::App,
                ProcessStatusFlags::empty(),
                0,
                50 * mib,
                0,
            );

            // QQ: 1 visible-window leader + 4 windowless QQ.exe helpers = QQ (5).
            // Plus a child crashpad_handler.exe that MUST stay in Background (not pulled into QQ).
            make_detailed_sample(
                s,
                200,
                100,
                100,
                1,
                "QQ.exe",
                ProcessCategory::App,
                ProcessStatusFlags::empty(),
                tick * 500_000,
                100 * mib,
                tick * 250_000,
            );
            for i in 1..=4u32 {
                let status = if i == 1 {
                    ProcessStatusFlags::EFFICIENCY_MODE
                } else if i == 2 {
                    ProcessStatusFlags::SUSPENDED
                } else {
                    ProcessStatusFlags::empty()
                };
                make_detailed_sample(
                    s,
                    200 + i,
                    100 + u64::from(i) * 10,
                    if i == 4 { 9999 } else { 200 },
                    1,
                    "QQ.exe",
                    ProcessCategory::Background,
                    status,
                    tick * 250_000,
                    50 * mib,
                    if i == 1 { tick * 250_000 } else { 0 },
                );
            }
            make_detailed_sample(
                s,
                250,
                160,
                200,
                1,
                "crashpad_handler.exe",
                ProcessCategory::Background,
                ProcessStatusFlags::empty(),
                0,
                12 * mib,
                0,
            );

            // Weixin: 2 visible-window Weixin.exe + 3 windowless Weixin.exe = single Weixin (5) group!
            // Plus 10x WeChatAppEx.exe and crashpad_handler.exe children that MUST stay in Background!
            make_detailed_sample(
                s,
                300,
                200,
                100,
                1,
                "Weixin.exe",
                ProcessCategory::App,
                ProcessStatusFlags::empty(),
                tick * 500_000,
                120 * mib,
                0,
            );
            make_detailed_sample(
                s,
                301,
                210,
                300,
                1,
                "Weixin.exe",
                ProcessCategory::App,
                ProcessStatusFlags::empty(),
                0,
                80 * mib,
                0,
            );
            for i in 2..=4u32 {
                make_detailed_sample(
                    s,
                    300 + i,
                    200 + u64::from(i) * 10,
                    300,
                    1,
                    "Weixin.exe",
                    ProcessCategory::Background,
                    ProcessStatusFlags::empty(),
                    0,
                    40 * mib,
                    0,
                );
            }
            make_detailed_sample(
                s,
                350,
                260,
                300,
                1,
                "WeChatAppEx.exe",
                ProcessCategory::Background,
                ProcessStatusFlags::empty(),
                0,
                20 * mib,
                0,
            );
            for i in 1..10u32 {
                make_detailed_sample(
                    s,
                    350 + i,
                    260 + u64::from(i),
                    350,
                    1,
                    "WeChatAppEx.exe",
                    ProcessCategory::Background,
                    ProcessStatusFlags::empty(),
                    0,
                    10 * mib,
                    0,
                );
            }

            // A fully suspended 2-process app -> group status MUST include SUSPENDED.
            make_detailed_sample(
                s,
                400,
                300,
                100,
                1,
                "SuspendedApp.exe",
                ProcessCategory::App,
                ProcessStatusFlags::SUSPENDED,
                0,
                30 * mib,
                0,
            );
            make_detailed_sample(
                s,
                401,
                310,
                400,
                1,
                "SuspendedApp.exe",
                ProcessCategory::Background,
                ProcessStatusFlags::SUSPENDED,
                0,
                10 * mib,
                0,
            );
        };

        let mut scratch = ProcessSampleScratch::<()>::new();
        populate(&mut scratch, 0);
        engine.ingest_process_sample(Timestamp100Ns::new(100_000_000), &scratch, 4);

        // +250 ms elapsed (2_500_000 ticks), 4 CPUs => 10_000_000 ticks = 1000 permille.
        populate(&mut scratch, 1);
        engine.ingest_process_sample(Timestamp100Ns::new(102_500_000), &scratch, 4);

        let cold = ColdMetaCache::new();
        let mut cfg = ViewProjectionConfig {
            process_view_mode: ProcessViewMode::Grouped,
            process_sort_column: ProcessSortColumn::Memory,
            process_sort_direction: SortDirection::Descending,
            ..ViewProjectionConfig::default()
        };

        let slice = engine.project_processes(&cfg, &cold, 2);

        // 4 top-level App groups: Weixin (320 MiB), QQ (300 MiB), explorer (50 MiB), SuspendedApp (40 MiB).
        assert_eq!(
            slice.rows[0].kind,
            ProcessRowKind::SectionHeader {
                category: ProcessCategory::App,
                count: 4
            }
        );
        let app_names: Vec<(&str, u32, u64)> = slice.rows[1..5]
            .iter()
            .map(|r| (r.name.as_ref(), r.child_count, r.memory_bytes))
            .collect();
        assert_eq!(
            app_names,
            vec![
                ("Weixin.exe", 5, 320 * mib),
                ("QQ.exe", 5, 300 * mib),
                ("explorer.exe", 0, 50 * mib),
                ("SuspendedApp.exe", 2, 40 * mib),
            ]
        );

        let qq = &slice.rows[2];
        assert_eq!(qq.key, ProcessKey::new(200, 100));
        assert!(qq.has_children);
        assert!(!qq.is_expanded);
        assert_eq!(qq.child_count, 5);
        assert_eq!(qq.cpu_permille, 150);
        assert_eq!(qq.memory_bytes, 300 * mib);
        assert_eq!(qq.memory_permille, 150);
        assert_eq!(qq.disk_bytes_per_sec, 2_000_000);
        assert!(qq.status.contains(ProcessStatusFlags::EFFICIENCY_MODE));
        assert!(!qq.status.contains(ProcessStatusFlags::SUSPENDED));

        let suspended_app = &slice.rows[4];
        assert!(suspended_app.status.contains(ProcessStatusFlags::SUSPENDED));

        // Background section MUST contain WeChatAppEx.exe (10) and crashpad_handler.exe (not inside Weixin/QQ!).
        assert_eq!(
            slice.rows[5].kind,
            ProcessRowKind::SectionHeader {
                category: ProcessCategory::Background,
                count: 2
            }
        );
        let bg_names: Vec<(&str, u32)> = slice.rows[6..]
            .iter()
            .map(|r| (r.name.as_ref(), r.child_count))
            .collect();
        assert_eq!(
            bg_names,
            vec![("WeChatAppEx.exe", 10), ("crashpad_handler.exe", 0)]
        );

        // Expanding QQ reveals its 4 other QQ.exe members at depth 1 with zero duplicate ProcessKeys.
        cfg.process_expanded.insert(ProcessKey::new(200, 100));
        let expanded = engine.project_processes(&cfg, &cold, 3);
        let mut seen_keys = HashSet::new();
        for row in expanded
            .rows
            .iter()
            .filter(|r| r.kind == ProcessRowKind::Process)
        {
            assert!(seen_keys.insert(row.key), "duplicate key {:?}", row.key);
        }
        let qq_idx = expanded
            .rows
            .iter()
            .position(|r| r.key == ProcessKey::new(200, 100))
            .unwrap();
        assert!(expanded.rows[qq_idx].is_expanded);
        let qq_children: Vec<&str> = expanded.rows[qq_idx + 1..qq_idx + 5]
            .iter()
            .map(|r| r.name.as_ref())
            .collect();
        assert_eq!(qq_children, vec!["QQ.exe", "QQ.exe", "QQ.exe", "QQ.exe"]);
        assert!(
            expanded.rows[qq_idx + 1..qq_idx + 5]
                .iter()
                .all(|r| r.depth == 1)
        );

        // Searching by a child PID inside QQ auto-expands QQ so the matching child is visible.
        cfg.process_expanded.clear();
        cfg.search_query = "204".to_owned();
        let searched = engine.project_processes(&cfg, &cold, 4);
        let searched_pids: Vec<(u32, u16)> = searched
            .rows
            .iter()
            .filter(|r| r.kind == ProcessRowKind::Process)
            .map(|r| (r.key.pid, r.depth))
            .collect();
        assert_eq!(searched_pids, vec![(200, 0), (204, 1)]);
        assert!(searched.rows[1].is_expanded);
    }

    #[test]
    fn windows_grouped_packaged_apps_group_by_app_group_id() {
        use loom_types::ProcessColdMeta;

        let mut engine = ProjectionEngine::with_platform(PlatformProfile::WINDOWS);
        let mut scratch = ProcessSampleScratch::<()>::new();
        scratch.perf.logical_cpu_count = 4;

        make_detailed_sample(
            &mut scratch,
            500,
            100,
            100,
            1,
            "WindowsTerminal.exe",
            ProcessCategory::App,
            ProcessStatusFlags::empty(),
            0,
            80 * 1024 * 1024,
            0,
        );
        make_detailed_sample(
            &mut scratch,
            501,
            110,
            500,
            1,
            "OpenConsole.exe",
            ProcessCategory::Background,
            ProcessStatusFlags::EFFICIENCY_MODE,
            0,
            20 * 1024 * 1024,
            0,
        );
        make_detailed_sample(
            &mut scratch,
            502,
            120,
            500,
            1,
            "pwsh.exe",
            ProcessCategory::Background,
            ProcessStatusFlags::empty(),
            0,
            40 * 1024 * 1024,
            0,
        );
        engine.ingest_process_sample(Timestamp100Ns::new(1_000_000), &scratch, 4);

        let mut cold = ColdMetaCache::new();
        let pkg: Arc<str> = Arc::from("Microsoft.WindowsTerminal_8wekyb3d8bbwe");
        cold.insert_process(ProcessColdMeta {
            key: ProcessKey::new(500, 100),
            exe_path: Some(Arc::from(
                r"C:\Program Files\WindowsApps\WT\WindowsTerminal.exe",
            )),
            command_line: None,
            user_name: None,
            arch: ProcessArch::X64,
            uac_virtualization: UacVirtualization::NotAllowed,
            efficiency_mode: false,
            is_elevated: false,
            app_group_id: Some(Arc::clone(&pkg)),
            asset: None,
        });
        cold.insert_process(ProcessColdMeta {
            key: ProcessKey::new(501, 110),
            exe_path: Some(Arc::from(
                r"C:\Program Files\WindowsApps\WT\OpenConsole.exe",
            )),
            command_line: None,
            user_name: None,
            arch: ProcessArch::X64,
            uac_virtualization: UacVirtualization::NotAllowed,
            efficiency_mode: false,
            is_elevated: false,
            app_group_id: Some(pkg),
            asset: None,
        });

        let cfg = ViewProjectionConfig {
            process_view_mode: ProcessViewMode::Grouped,
            ..ViewProjectionConfig::default()
        };

        let slice = engine.project_processes(&cfg, &cold, 1);
        assert_eq!(
            slice.rows[0].kind,
            ProcessRowKind::SectionHeader {
                category: ProcessCategory::App,
                count: 1
            }
        );
        // WindowsTerminal.exe and OpenConsole.exe share app_group_id -> grouped as WindowsTerminal (2) (100 MiB).
        assert_eq!(slice.rows[1].name.as_ref(), "WindowsTerminal.exe");
        assert_eq!(slice.rows[1].child_count, 2);
        assert_eq!(slice.rows[1].memory_bytes, 100 * 1024 * 1024);
        assert!(
            slice.rows[1]
                .status
                .contains(ProcessStatusFlags::EFFICIENCY_MODE)
        );

        // Unpackaged pwsh.exe stays in Background!
        assert_eq!(
            slice.rows[2].kind,
            ProcessRowKind::SectionHeader {
                category: ProcessCategory::Background,
                count: 1
            }
        );
        assert_eq!(slice.rows[3].name.as_ref(), "pwsh.exe");
    }

    #[test]
    fn windows_tree_mode_preserves_explorer_parent_child_edges() {
        let mut engine = ProjectionEngine::with_platform(PlatformProfile::WINDOWS);
        let mut scratch = ProcessSampleScratch::<()>::new();
        scratch.perf.logical_cpu_count = 4;

        make_sample(
            &mut scratch,
            100,
            10,
            0,
            "explorer.exe",
            ProcessCategory::App,
            0,
            0,
        );
        make_sample(
            &mut scratch,
            200,
            20,
            100,
            "chrome.exe",
            ProcessCategory::App,
            0,
            0,
        );
        make_sample(
            &mut scratch,
            201,
            30,
            200,
            "chrome.exe",
            ProcessCategory::Background,
            0,
            0,
        );
        engine.ingest_process_sample(Timestamp100Ns::new(1_000_000), &scratch, 4);

        let cold = ColdMetaCache::new();
        let mut cfg = ViewProjectionConfig {
            process_view_mode: ProcessViewMode::Tree,
            ..ViewProjectionConfig::default()
        };
        cfg.process_expanded.insert(ProcessKey::new(100, 10));
        cfg.process_expanded.insert(ProcessKey::new(200, 20));

        let slice = engine.project_processes(&cfg, &cold, 1);
        let tree_shape: Vec<(u32, u16)> = slice.rows.iter().map(|r| (r.key.pid, r.depth)).collect();
        assert_eq!(tree_shape, vec![(100, 0), (200, 1), (201, 2)]);
    }

    #[test]
    fn linux_and_macos_preserve_standard_grouped_behavior() {
        for profile in [PlatformProfile::LINUX, PlatformProfile::MACOS] {
            let mut engine = ProjectionEngine::with_platform(profile);
            let mut scratch = ProcessSampleScratch::<()>::new();
            scratch.perf.logical_cpu_count = 4;

            make_sample(
                &mut scratch,
                100,
                10,
                0,
                "explorer.exe",
                ProcessCategory::App,
                0,
                0,
            );
            make_sample(
                &mut scratch,
                200,
                20,
                100,
                "chrome",
                ProcessCategory::App,
                0,
                0,
            );
            make_sample(
                &mut scratch,
                201,
                30,
                200,
                "chrome",
                ProcessCategory::Background,
                0,
                0,
            );
            engine.ingest_process_sample(Timestamp100Ns::new(1_000_000), &scratch, 4);

            let cold = ColdMetaCache::new();
            let cfg = ViewProjectionConfig {
                process_view_mode: ProcessViewMode::Grouped,
                ..ViewProjectionConfig::default()
            };

            let slice = engine.project_processes(&cfg, &cold, 1);
            // Non-Windows keeps category-local parent nesting (App child stays under App parent)
            // and keeps Background helpers in the Background section.
            assert_eq!(
                slice.rows[0].kind,
                ProcessRowKind::SectionHeader {
                    category: ProcessCategory::App,
                    count: 2
                },
                "failed for {profile:?}"
            );
            assert_eq!(slice.rows[1].key.pid, 100);
            assert!(slice.rows[1].has_children);
            assert_eq!(slice.rows[1].child_count, 1);
            assert_eq!(
                slice.rows[2].kind,
                ProcessRowKind::SectionHeader {
                    category: ProcessCategory::Background,
                    count: 1
                },
                "failed for {profile:?}"
            );
            assert_eq!(slice.rows[3].key.pid, 201);
        }
    }
}
