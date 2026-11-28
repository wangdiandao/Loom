//! Fast-lane delta computation and pre-sorted, pre-filtered snapshot projections.
//!
//! All sorting, searching, grouping, and tree flattening happen off the UI thread
//! inside [`ProjectionEngine`] using dense process indices and reused scratch
//! buffers so virtualized UI tables perform `O(1)` indexing.

mod deadband_sort;
mod details;
mod mgmt_tables;
mod performance;
mod processes;
mod state;
mod windows_groups;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use loom_config::AppSettings;
use loom_pal_api::ProcessSampleScratch;
use loom_types::{
    AppHistorySortColumn, DetailsRowProjection, DetailsSortColumn, DetailsViewMode,
    DiskDeviceSlice, GpuAdapterSlice, HardwareStaticInfo, HistoryRing, NetAdapterSlice,
    PlatformProfile, ProcessCategory, ProcessKey, ProcessRowKind, ProcessRowProjection,
    ProcessSocketRecord, ProcessSortColumn, ProcessStatusFlags, ProcessViewMode,
    ServicesSortColumn, SortDirection, StartupSortColumn, SystemPerfSample, Timestamp100Ns,
    UsersSortColumn,
};

pub use deadband_sort::{
    CPU_SORT_DEADBAND_PERMILLE, GPU_SORT_DEADBAND_PERMILLE, RATE_SORT_DEADBAND_BPS,
};
use state::{
    ComputedProcess, DeltaTracker, DetailsSortSignature, LiveAppHistoryEntry, ProcSortSignature,
    ProjectionScratch, StringInterner, effective_status_with_entry,
};

use crate::leaks::{LeakDetector, LeakDetectorConfig, LeakSampleInput};
use crate::slow_lane::ColdMetaCache;
use crate::tree::{FastBuildHasher, ProcessTree};

/// View filtering, grouping, sorting, freeze, and tree expansion parameters.
#[derive(Debug, Clone)]
pub struct ViewProjectionConfig {
    /// Active structured / case-insensitive search query.
    pub search_query: String,
    /// Processes view mode (`Grouped` or `Tree`).
    pub process_view_mode: ProcessViewMode,
    /// Processes table sort column.
    pub process_sort_column: ProcessSortColumn,
    /// Processes table sort direction.
    pub process_sort_direction: SortDirection,
    /// Expanded rows in the Processes view.
    pub process_expanded: HashSet<ProcessKey>,
    /// Whether Processes table row order is currently frozen in place.
    pub process_order_frozen: bool,
    /// Captured [`ProcessKey`] order honored while `process_order_frozen` is `true`.
    pub frozen_process_order: Vec<ProcessKey>,
    /// Details view mode (`Flat` or `Tree`).
    pub details_view_mode: DetailsViewMode,
    /// Details table sort column.
    pub details_sort_column: DetailsSortColumn,
    /// Details table sort direction.
    pub details_sort_direction: SortDirection,
    /// Expanded rows in the Details tree view.
    pub details_expanded: HashSet<ProcessKey>,
    /// Whether Details table row order is currently frozen in place.
    pub details_order_frozen: bool,
    /// Captured [`ProcessKey`] order honored while `details_order_frozen` is `true`.
    pub frozen_details_order: Vec<ProcessKey>,
    /// Whether sort hysteresis deadband is enabled on high-frequency metric columns (`Cpu`, `Gpu`, `Disk`, `Network`).
    pub sort_deadband_enabled: bool,
    /// Services table sort column.
    pub services_sort_column: ServicesSortColumn,
    /// Services table sort direction.
    pub services_sort_direction: SortDirection,
    /// Optional hosting PID filter for the Services view.
    pub services_filter_pid: Option<u32>,
    /// Startup Applications table sort column.
    pub startup_sort_column: StartupSortColumn,
    /// Startup Applications table sort direction.
    pub startup_sort_direction: SortDirection,
    /// Users table sort column.
    pub users_sort_column: UsersSortColumn,
    /// Users table sort direction.
    pub users_sort_direction: SortDirection,
    /// Expanded OS session IDs in the Users view.
    pub users_expanded: HashSet<u32>,
    /// App History table sort column.
    pub app_history_sort_column: AppHistorySortColumn,
    /// App History table sort direction.
    pub app_history_sort_direction: SortDirection,
    /// Whether `msedgewebview2.exe` subprocesses are grouped under their host app on Windows.
    pub webview2_host_grouping: bool,
    /// Whether advisory resource-leak warnings are attached to projected rows.
    pub show_leak_warnings: bool,
    /// Case-insensitive executable paths or image names excluded from leak warnings.
    pub leak_ignored_paths: Vec<String>,
    /// Whether core OS system processes (except interactive shells) are hidden in process tables.
    pub hide_windows_system_processes: bool,
}

impl Default for ViewProjectionConfig {
    fn default() -> Self {
        Self {
            search_query: String::new(),
            process_view_mode: ProcessViewMode::default(),
            process_sort_column: ProcessSortColumn::default(),
            process_sort_direction: SortDirection::default(),
            process_expanded: HashSet::new(),
            process_order_frozen: false,
            frozen_process_order: Vec::new(),
            details_view_mode: DetailsViewMode::default(),
            details_sort_column: DetailsSortColumn::default(),
            details_sort_direction: SortDirection::default(),
            details_expanded: HashSet::new(),
            details_order_frozen: false,
            frozen_details_order: Vec::new(),
            sort_deadband_enabled: true,
            services_sort_column: ServicesSortColumn::Name,
            services_sort_direction: SortDirection::Ascending,
            services_filter_pid: None,
            startup_sort_column: StartupSortColumn::Name,
            startup_sort_direction: SortDirection::Ascending,
            users_sort_column: UsersSortColumn::Cpu,
            users_sort_direction: SortDirection::Descending,
            users_expanded: HashSet::new(),
            app_history_sort_column: AppHistorySortColumn::CpuTime,
            app_history_sort_direction: SortDirection::Descending,
            webview2_host_grouping: true,
            show_leak_warnings: true,
            leak_ignored_paths: Vec::new(),
            hide_windows_system_processes: false,
        }
    }
}

impl ViewProjectionConfig {
    /// Initializes a [`ViewProjectionConfig`] from persisted [`AppSettings`].
    pub fn from_settings(settings: &AppSettings) -> Self {
        Self {
            process_view_mode: settings.process_view_mode,
            webview2_host_grouping: settings.webview2_host_grouping,
            show_leak_warnings: settings.show_leak_warnings,
            leak_ignored_paths: settings.leak_ignored_paths.clone(),
            hide_windows_system_processes: settings.hide_windows_system_processes,
            ..Self::default()
        }
    }

    /// Captures the current displayed [`ProcessKey`] row order from a [`loom_types::ProcessTableSlice`]
    /// and enables [`Self::process_order_frozen`].
    pub fn freeze_process_order(&mut self, rows: &[ProcessRowProjection]) {
        self.frozen_process_order.clear();
        self.frozen_process_order.extend(
            rows.iter()
                .filter(|r| r.kind == ProcessRowKind::Process)
                .map(|r| r.key),
        );
        self.process_order_frozen = true;
    }

    /// Captures the current displayed [`ProcessKey`] row order from a [`loom_types::DetailsTableSlice`]
    /// and enables [`Self::details_order_frozen`].
    pub fn freeze_details_order(&mut self, rows: &[DetailsRowProjection]) {
        self.frozen_details_order.clear();
        self.frozen_details_order.extend(rows.iter().map(|r| r.key));
        self.details_order_frozen = true;
    }
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
    app_history_live: HashMap<Arc<str>, LiveAppHistoryEntry, FastBuildHasher>,
    listening_ports: HashSet<(u32, u16)>,
    leak_detector: LeakDetector,
    prev_proc_sort_signature: Option<ProcSortSignature>,
    prev_proc_deadband: HashMap<ProcessKey, (usize, u64), FastBuildHasher>,
    prev_details_sort_signature: Option<DetailsSortSignature>,
    prev_details_deadband: HashMap<ProcessKey, (usize, u64), FastBuildHasher>,
    next_deadband_scratch: HashMap<ProcessKey, (usize, u64), FastBuildHasher>,
    scratch: ProjectionScratch,
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
        Self::with_platform_and_leak_config(platform, LeakDetectorConfig::default())
    }

    /// Creates a fresh [`ProjectionEngine`] configured for `platform` and explicit [`LeakDetectorConfig`].
    pub fn with_platform_and_leak_config(
        platform: PlatformProfile,
        leak_config: LeakDetectorConfig,
    ) -> Self {
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
            app_history_live: HashMap::with_hasher(FastBuildHasher),
            listening_ports: HashSet::new(),
            leak_detector: LeakDetector::with_config(leak_config),
            prev_proc_sort_signature: None,
            prev_proc_deadband: HashMap::with_capacity_and_hasher(512, FastBuildHasher),
            prev_details_sort_signature: None,
            prev_details_deadband: HashMap::with_capacity_and_hasher(512, FastBuildHasher),
            next_deadband_scratch: HashMap::with_capacity_and_hasher(512, FastBuildHasher),
            scratch: ProjectionScratch::with_capacity(512),
        }
    }

    /// Replaces the cached on-demand listening `(pid, local_port)` lookup set.
    pub fn set_listening_ports(&mut self, sockets: &[ProcessSocketRecord]) {
        self.listening_ports.clear();
        self.listening_ports.reserve(sockets.len());
        for sock in sockets {
            self.listening_ports.insert((sock.pid, sock.local_port));
        }
    }

    /// Clears the cached on-demand listening port set when no `:PORT` query is active.
    pub fn clear_listening_ports(&mut self) {
        self.listening_ports.clear();
    }

    /// Evaluates the pure in-memory [`LeakDetector`] at `now` when its cadence has elapsed.
    ///
    /// Returns `true` if a leak-detection evaluation pass ran.
    pub fn update_leak_detector(&mut self, now: Instant, cold_cache: &ColdMetaCache) -> bool {
        if !self.leak_detector.should_evaluate(now) {
            return false;
        }
        let Self {
            computed,
            latest_tree,
            leak_detector,
            ..
        } = self;
        let iter = computed.iter().enumerate().map(|(idx, proc)| {
            let cold_entry = cold_cache.get_entry(proc.key);
            let exe_path = cold_entry.and_then(|e| e.meta.exe_path.as_deref());
            LeakSampleInput {
                key: proc.key,
                name: &proc.name,
                exe_path,
                category: proc.category,
                status: effective_status_with_entry(proc, cold_entry, cold_cache),
                has_window: proc.window_title.is_some() || proc.category == ProcessCategory::App,
                has_live_parent: latest_tree.parent_index(idx).is_some(),
                handle_count: proc.handle_count,
                child_count: latest_tree.children_indices(idx).len() as u32,
                cpu_time_100ns: proc.cpu_time_100ns,
            }
        });
        leak_detector.update_at_iter(now, computed.len(), iter)
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
                working_set_delta_bytes,
            ) = self.delta_tracker.compute_for_process(
                sample.key,
                cpu_time_100ns,
                disk_bytes,
                net_bytes,
                sample.working_set_bytes,
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
                working_set_delta_bytes,
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

    /// Iterates over `(key, category, private_working_set_bytes)` for all live processes
    /// so the coordinator can prioritize slow-lane metadata probes for foreground apps first.
    #[inline]
    pub fn live_probe_candidates(
        &self,
    ) -> impl Iterator<Item = (ProcessKey, ProcessCategory, u64)> + '_ {
        self.computed
            .iter()
            .map(|p| (p.key, p.category, p.private_working_set_bytes))
    }
}
