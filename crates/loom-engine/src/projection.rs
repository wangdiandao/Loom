//! Fast-lane delta computation and pre-sorted, pre-filtered snapshot projections.
//!
//! All sorting, searching, grouping, and tree flattening happen off the UI thread
//! inside [`ProjectionEngine`] using dense process indices and reused scratch
//! buffers so virtualized UI tables perform `O(1)` indexing.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::hash::Hasher as _;
use std::sync::Arc;
use std::time::Instant;

use loom_config::AppSettings;
use loom_pal_api::ProcessSampleScratch;
use loom_types::{
    AppHistoryRecord, AppHistorySlice, AppHistorySortColumn, DetailsRowProjection,
    DetailsSortColumn, DetailsTableSlice, DetailsViewMode, DiskDeviceSample, DiskDeviceSlice,
    GpuAdapterSample, GpuAdapterSlice, GpuEngineSlice, HardwareStaticInfo, HistoryRing,
    LeakWarning, NetAdapterSample, NetAdapterSlice, PerformanceSlice, PlatformKind,
    PlatformProfile, ProcessArch, ProcessCategory, ProcessKey, ProcessPriority, ProcessRowKind,
    ProcessRowProjection, ProcessSocketRecord, ProcessSortColumn, ProcessStatusFlags,
    ProcessTableSlice, ProcessViewMode, ServiceRecord, ServiceState, ServicesSlice,
    ServicesSortColumn, SortDirection, StartupEntry, StartupImpact, StartupSlice,
    StartupSortColumn, SystemPerfSample, ThroughputRing, Timestamp100Ns, UacVirtualization,
    UserSessionProcess, UserSessionRecord, UserSessionState, UsersRowProjection, UsersSlice,
    UsersSortColumn,
};

use crate::leaks::{LeakDetector, LeakDetectorConfig, LeakSampleInput};
use crate::query::{ParsedQuery, QueryMatchContext};
use crate::slow_lane::ColdMetaCache;
use crate::tree::{FastBuildHasher, FastHasher, ProcessTree};
use crate::webview2::{classify_webview2, is_webview2_image};

/// Maximum number of interned strings retained by the fast-lane interner.
const MAX_INTERNED_STRINGS: usize = 4_096;

/// Maximum number of distinct application keys retained in the live App History accumulator.
const MAX_LIVE_APP_HISTORY_ENTRIES: usize = 2_048;

/// Hysteresis deadband for CPU sort columns (`2` permille = `0.2%`).
pub const CPU_SORT_DEADBAND_PERMILLE: u16 = 2;

/// Hysteresis deadband for GPU utilization sort column (`2` permille = `0.2%`).
pub const GPU_SORT_DEADBAND_PERMILLE: u16 = 2;

/// Hysteresis deadband for disk and network throughput sort columns (`16 KiB/s`).
pub const RATE_SORT_DEADBAND_BPS: u64 = 16 * 1024;

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

    /// Captures the current displayed [`ProcessKey`] row order from a [`ProcessTableSlice`]
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

    /// Captures the current displayed [`ProcessKey`] row order from a [`DetailsTableSlice`]
    /// and enables [`Self::details_order_frozen`].
    pub fn freeze_details_order(&mut self, rows: &[DetailsRowProjection]) {
        self.frozen_details_order.clear();
        self.frozen_details_order.extend(rows.iter().map(|r| r.key));
        self.details_order_frozen = true;
    }
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

type ProcSortSignature = (ProcessViewMode, ProcessSortColumn, SortDirection, String);
type DetailsSortSignature = (DetailsViewMode, DetailsSortColumn, SortDirection, String);

/// Zero-allocation intrusive hash-bucket index mapping `u64` hashes to FIFO chains
/// of `(group_idx, sample_proc_idx)` entries so callers can verify exact string equality
/// on hash collisions without storing lifetime-bound `&str` keys.
#[derive(Debug, Default)]
struct HashGroupIndex {
    heads: HashMap<u64, (usize, usize), FastBuildHasher>,
    nodes: Vec<(usize, usize, usize)>, // (group_idx, sample_proc_idx, next_node_idx or usize::MAX)
}

impl HashGroupIndex {
    fn clear(&mut self) {
        self.heads.clear();
        self.nodes.clear();
    }

    #[inline]
    fn push(&mut self, hash: u64, group_idx: usize, sample_proc_idx: usize) {
        let node_idx = self.nodes.len();
        self.nodes.push((group_idx, sample_proc_idx, usize::MAX));
        match self.heads.get_mut(&hash) {
            Some((_head, tail)) => {
                self.nodes[*tail].2 = node_idx;
                *tail = node_idx;
            }
            None => {
                self.heads.insert(hash, (node_idx, node_idx));
            }
        }
    }

    #[inline]
    fn find<F>(&self, hash: u64, mut predicate: F) -> Option<usize>
    where
        F: FnMut(usize, usize) -> bool,
    {
        let &(mut curr, _) = self.heads.get(&hash)?;
        while curr != usize::MAX {
            let (group_idx, sample_proc_idx, next) = self.nodes[curr];
            if predicate(group_idx, sample_proc_idx) {
                return Some(group_idx);
            }
            curr = next;
        }
        None
    }
}

/// Reusable contiguous string arena for zero-allocation Unicode/ASCII case-folded Schwartzian sort keys.
#[derive(Debug, Default)]
struct FoldedKeyArena {
    buf: String,
    fold_tmp: String,
}

impl FoldedKeyArena {
    #[inline]
    fn clear(&mut self) {
        self.buf.clear();
    }

    #[inline]
    fn push_folded(&mut self, s: &str) -> (u32, u32) {
        let start = self.buf.len() as u32;
        if s.is_ascii() {
            self.buf.reserve(s.len());
            for b in s.bytes() {
                self.buf.push(b.to_ascii_lowercase() as char);
            }
        } else if s.contains('\u{03A3}') {
            // Preserve exact Greek final-sigma behavior of `str::to_lowercase`.
            self.buf.push_str(&s.to_lowercase());
        } else {
            self.buf.reserve(s.len());
            for ch in s.chars() {
                if ch.is_ascii() {
                    self.buf.push(ch.to_ascii_lowercase());
                } else {
                    for lc in ch.to_lowercase() {
                        self.buf.push(lc);
                    }
                }
            }
        }
        let end = self.buf.len() as u32;
        (start, end)
    }

    #[inline]
    fn slice(&self, range: (u32, u32)) -> &str {
        &self.buf[range.0 as usize..range.1 as usize]
    }

    #[inline]
    fn contains_icase(&mut self, haystack: &str, needle_lower: &str) -> bool {
        if needle_lower.is_empty() {
            return true;
        }
        if haystack.is_ascii() && needle_lower.is_ascii() {
            let h = haystack.as_bytes();
            let n = needle_lower.as_bytes();
            if n.len() > h.len() {
                return false;
            }
            return h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n));
        }
        self.fold_tmp.clear();
        if haystack.contains('\u{03A3}') {
            self.fold_tmp.push_str(&haystack.to_lowercase());
        } else {
            self.fold_tmp.reserve(haystack.len());
            for ch in haystack.chars() {
                if ch.is_ascii() {
                    self.fold_tmp.push(ch.to_ascii_lowercase());
                } else {
                    for lc in ch.to_lowercase() {
                        self.fold_tmp.push(lc);
                    }
                }
            }
        }
        self.fold_tmp.contains(needle_lower)
    }
}

/// Decorated entry for zero-allocation Schwartzian sorting across Services, Startup, Users, and AppHistory.
#[derive(Debug, Clone, Copy)]
struct SchwartzEntry {
    index: usize,
    numeric: u64,
    primary_str: (u32, u32),
    tie_str: (u32, u32),
}

/// Dense per-process previous-tick deadband state indexed directly by `computed` index
/// so `slice::sort_by` performs `O(1)` array indexing with zero `HashMap` lookups.
#[derive(Debug, Clone, Copy, Default)]
struct DenseDeadbandEntry {
    prev_rank: usize,
    prev_sticky: u64,
    has_prev: bool,
}

/// Borrowed sort hysteresis deadband state for a single projection pass.
#[derive(Clone, Copy)]
struct DeadbandContext<'a> {
    deadband: u64,
    dense_prev: &'a [DenseDeadbandEntry],
}

/// Persistent scratch buffers owned by [`ProjectionEngine`] and cleared in place across ticks
/// to eliminate steady-state per-tick heap allocations during grouping, sorting, and tree flattening.
#[derive(Debug, Default)]
struct ProjectionScratch {
    frozen_ranks: HashMap<ProcessKey, usize, FastBuildHasher>,
    dense_deadband: Vec<DenseDeadbandEntry>,
    bool_mask_a: Vec<bool>,
    bool_mask_b: Vec<bool>,
    bool_mask_c: Vec<bool>,
    stack: Vec<(usize, u16)>,
    section_roots: Vec<usize>,
    visible_children: Vec<usize>,
    by_category: [Vec<usize>; 3],
    deferred_webview2: Vec<usize>,
    subtree_descendants: Vec<usize>,
    desc_stack: Vec<usize>,
    group_agg: Vec<Option<GroupAggregate>>,
    subtree_counts: Vec<u32>,
    idx_to_app_group: Vec<Option<usize>>,
    app_group_leaders: Vec<usize>,
    app_group_children: Vec<Vec<usize>>,
    visible_apps: Vec<(usize, usize, GroupAggregate)>,
    pkg_to_group: HashGroupIndex,
    exe_to_group: HashGroupIndex,
    name_to_groups: HashGroupIndex,
    folded_arena: FoldedKeyArena,
    schwartz_entries: Vec<SchwartzEntry>,
    schwartz_children: Vec<SchwartzEntry>,
}

impl ProjectionScratch {
    fn with_capacity(cap: usize) -> Self {
        Self {
            frozen_ranks: HashMap::with_capacity_and_hasher(cap, FastBuildHasher),
            dense_deadband: Vec::with_capacity(cap),
            bool_mask_a: Vec::with_capacity(cap),
            bool_mask_b: Vec::with_capacity(cap),
            bool_mask_c: Vec::with_capacity(cap),
            stack: Vec::with_capacity(cap),
            section_roots: Vec::with_capacity(cap),
            visible_children: Vec::with_capacity(64),
            by_category: [
                Vec::with_capacity(64),
                Vec::with_capacity(cap),
                Vec::with_capacity(128),
            ],
            deferred_webview2: Vec::with_capacity(32),
            subtree_descendants: Vec::with_capacity(64),
            desc_stack: Vec::with_capacity(64),
            group_agg: Vec::with_capacity(cap),
            subtree_counts: Vec::with_capacity(cap),
            idx_to_app_group: Vec::with_capacity(cap),
            app_group_leaders: Vec::with_capacity(64),
            app_group_children: Vec::with_capacity(64),
            visible_apps: Vec::with_capacity(64),
            pkg_to_group: HashGroupIndex::default(),
            exe_to_group: HashGroupIndex::default(),
            name_to_groups: HashGroupIndex::default(),
            folded_arena: FoldedKeyArena::default(),
            schwartz_entries: Vec::with_capacity(cap),
            schwartz_children: Vec::with_capacity(64),
        }
    }

    #[inline]
    fn populate_dense_deadband(
        &mut self,
        computed: &[ComputedProcess],
        prev_map: &HashMap<ProcessKey, (usize, u64), FastBuildHasher>,
    ) {
        self.dense_deadband.clear();
        self.dense_deadband.reserve(computed.len());
        if prev_map.is_empty() {
            self.dense_deadband.resize(
                computed.len(),
                DenseDeadbandEntry {
                    prev_rank: usize::MAX,
                    prev_sticky: 0,
                    has_prev: false,
                },
            );
            return;
        }
        for proc in computed {
            if let Some(&(prev_rank, prev_sticky)) = prev_map.get(&proc.key) {
                self.dense_deadband.push(DenseDeadbandEntry {
                    prev_rank,
                    prev_sticky,
                    has_prev: true,
                });
            } else {
                self.dense_deadband.push(DenseDeadbandEntry {
                    prev_rank: usize::MAX,
                    prev_sticky: 0,
                    has_prev: false,
                });
            }
        }
    }

    #[inline]
    fn populate_frozen_ranks(&mut self, frozen: bool, order: &[ProcessKey]) -> bool {
        self.frozen_ranks.clear();
        if !frozen || order.is_empty() {
            return false;
        }
        self.frozen_ranks.reserve(order.len());
        for (rank, &key) in order.iter().enumerate() {
            self.frozen_ranks.entry(key).or_insert(rank);
        }
        true
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
    app_history_live: HashMap<Arc<str>, LiveAppHistoryEntry>,
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
            app_history_live: HashMap::new(),
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
            let exe_path = cold_cache
                .get_process(proc.key)
                .and_then(|meta| meta.exe_path.as_deref());
            LeakSampleInput {
                key: proc.key,
                name: &proc.name,
                exe_path,
                category: proc.category,
                status: effective_status(proc, cold_cache),
                has_window: proc.window_title.is_some() || proc.category == ProcessCategory::App,
                has_live_parent: latest_tree.parent_index(idx).is_some(),
                handle_count: proc.handle_count,
                child_count: latest_tree.children_indices(idx).len() as u32,
                cpu_time_100ns: proc.cpu_time_100ns,
            }
        });
        leak_detector.update_at_iter(now, computed.len(), iter)
    }

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
        &mut self,
        config: &ViewProjectionConfig,
        cold_cache: &ColdMetaCache,
        revision: u64,
    ) -> ProcessTableSlice {
        let parsed_query = ParsedQuery::parse(&config.search_query);
        let use_frozen = self
            .scratch
            .populate_frozen_ranks(config.process_order_frozen, &config.frozen_process_order);

        let proc_deadband = if !use_frozen && config.sort_deadband_enabled {
            process_column_deadband(config.process_sort_column)
        } else {
            None
        };

        if proc_deadband.is_some() {
            let sig_matches = self.prev_proc_sort_signature.as_ref().is_some_and(|sig| {
                sig.0 == config.process_view_mode
                    && sig.1 == config.process_sort_column
                    && sig.2 == config.process_sort_direction
                    && sig.3 == config.search_query
            });
            if !sig_matches {
                self.prev_proc_deadband.clear();
                self.prev_proc_sort_signature = Some((
                    config.process_view_mode,
                    config.process_sort_column,
                    config.process_sort_direction,
                    config.search_query.clone(),
                ));
            }
            self.scratch
                .populate_dense_deadband(&self.computed, &self.prev_proc_deadband);
        } else if self.prev_proc_sort_signature.is_some() {
            self.prev_proc_sort_signature = None;
            self.prev_proc_deadband.clear();
        }

        let rows = match config.process_view_mode {
            ProcessViewMode::Grouped => self.project_processes_grouped(
                config,
                cold_cache,
                &parsed_query,
                use_frozen,
                proc_deadband,
            ),
            ProcessViewMode::Tree => self.project_processes_tree(
                config,
                cold_cache,
                &parsed_query,
                use_frozen,
                proc_deadband,
            ),
        };

        if let Some(deadband) = proc_deadband {
            self.next_deadband_scratch.clear();
            self.next_deadband_scratch.reserve(rows.len());
            let mut rank = 0usize;
            for row in &rows {
                if row.kind != ProcessRowKind::Process {
                    continue;
                }
                let live = process_row_deadband_metric(row, config.process_sort_column);
                let sticky = if let Some(&(_, prev_val)) = self.prev_proc_deadband.get(&row.key)
                    && live.abs_diff(prev_val) < deadband
                {
                    prev_val
                } else {
                    live
                };
                self.next_deadband_scratch.insert(row.key, (rank, sticky));
                rank += 1;
            }
            std::mem::swap(
                &mut self.prev_proc_deadband,
                &mut self.next_deadband_scratch,
            );
        }

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
        &mut self,
        config: &ViewProjectionConfig,
        cold_cache: &ColdMetaCache,
        revision: u64,
    ) -> DetailsTableSlice {
        let parsed_query = ParsedQuery::parse(&config.search_query);
        let use_frozen = self
            .scratch
            .populate_frozen_ranks(config.details_order_frozen, &config.frozen_details_order);

        let details_deadband = if !use_frozen && config.sort_deadband_enabled {
            details_column_deadband(config.details_sort_column)
        } else {
            None
        };

        if details_deadband.is_some() {
            let sig_matches = self
                .prev_details_sort_signature
                .as_ref()
                .is_some_and(|sig| {
                    sig.0 == config.details_view_mode
                        && sig.1 == config.details_sort_column
                        && sig.2 == config.details_sort_direction
                        && sig.3 == config.search_query
                });
            if !sig_matches {
                self.prev_details_deadband.clear();
                self.prev_details_sort_signature = Some((
                    config.details_view_mode,
                    config.details_sort_column,
                    config.details_sort_direction,
                    config.search_query.clone(),
                ));
            }
            self.scratch
                .populate_dense_deadband(&self.computed, &self.prev_details_deadband);
        } else if self.prev_details_sort_signature.is_some() {
            self.prev_details_sort_signature = None;
            self.prev_details_deadband.clear();
        }

        let rows = match config.details_view_mode {
            DetailsViewMode::Flat => self.project_details_flat(
                config,
                cold_cache,
                &parsed_query,
                use_frozen,
                details_deadband,
            ),
            DetailsViewMode::Tree => self.project_details_tree(
                config,
                cold_cache,
                &parsed_query,
                use_frozen,
                details_deadband,
            ),
        };

        if let Some(deadband) = details_deadband {
            self.next_deadband_scratch.clear();
            self.next_deadband_scratch.reserve(rows.len());
            for (rank, row) in rows.iter().enumerate() {
                let live = u64::from(row.cpu_permille);
                let sticky = if let Some(&(_, prev_val)) = self.prev_details_deadband.get(&row.key)
                    && live.abs_diff(prev_val) < deadband
                {
                    prev_val
                } else {
                    live
                };
                self.next_deadband_scratch.insert(row.key, (rank, sticky));
            }
            std::mem::swap(
                &mut self.prev_details_deadband,
                &mut self.next_deadband_scratch,
            );
        }

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
    ///
    /// Returns `true` if any non-zero usage delta was accumulated on this tick.
    pub fn accumulate_app_history(&mut self, cold_cache: &ColdMetaCache) -> bool {
        let mut accumulated_any = false;
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
                accumulated_any = true;
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
                accumulated_any = true;
            }
        }
        accumulated_any
    }

    /// Clears all in-memory cumulative App History counters.
    pub fn clear_app_history(&mut self) {
        self.app_history_live.clear();
    }

    /// Filters and sorts [`ServiceRecord`] entries off the UI thread using pre-folded
    /// Schwartzian sort keys so virtualized UI tables perform O(1) indexing.
    pub fn project_services(
        &mut self,
        items: &[ServiceRecord],
        config: &ViewProjectionConfig,
        revision: u64,
    ) -> ServicesSlice {
        let query_lower = config.search_query.trim().to_lowercase();
        let filter_pid = config.services_filter_pid;
        let sort_col = config.services_sort_column;
        let sort_dir = config.services_sort_direction;

        let arena = &mut self.scratch.folded_arena;
        let entries = &mut self.scratch.schwartz_entries;
        arena.clear();
        entries.clear();

        let is_str_col = matches!(
            sort_col,
            ServicesSortColumn::Name | ServicesSortColumn::Description | ServicesSortColumn::Group
        );

        for (idx, record) in items.iter().enumerate() {
            if !matches_service_filter(record, filter_pid, &query_lower, arena) {
                continue;
            }
            let tie_str = arena.push_folded(&record.name);
            let (numeric, primary_str) = match sort_col {
                ServicesSortColumn::Name => (0, tie_str),
                ServicesSortColumn::Pid => (u64::from(record.pid.unwrap_or(0)), (0, 0)),
                ServicesSortColumn::Description => {
                    (0, arena.push_folded(service_description_text(record)))
                }
                ServicesSortColumn::Status => (u64::from(service_state_rank(record.state)), (0, 0)),
                ServicesSortColumn::Group => {
                    (0, arena.push_folded(record.group.as_deref().unwrap_or("")))
                }
            };
            entries.push(SchwartzEntry {
                index: idx,
                numeric,
                primary_str,
                tie_str,
            });
        }

        entries.sort_by(|a, b| {
            let ord = if is_str_col {
                arena.slice(a.primary_str).cmp(arena.slice(b.primary_str))
            } else {
                a.numeric.cmp(&b.numeric)
            };
            let directed = match sort_dir {
                SortDirection::Ascending => ord,
                SortDirection::Descending => ord.reverse(),
            };
            directed
                .then_with(|| arena.slice(a.tie_str).cmp(arena.slice(b.tie_str)))
                .then_with(|| a.index.cmp(&b.index))
        });

        let visible_indices = entries.iter().map(|e| e.index).collect();

        ServicesSlice {
            revision,
            timestamp: self.latest_timestamp,
            sort_column: sort_col,
            sort_direction: sort_dir,
            filter_pid,
            items: items.to_vec(),
            visible_indices,
        }
    }

    /// Filters and sorts [`StartupEntry`] items off the UI thread using pre-folded
    /// Schwartzian sort keys so virtualized UI tables perform O(1) indexing.
    pub fn project_startup(
        &mut self,
        last_bios_time_ms: Option<u64>,
        items: &[StartupEntry],
        config: &ViewProjectionConfig,
        revision: u64,
    ) -> StartupSlice {
        let query_lower = config.search_query.trim().to_lowercase();
        let sort_col = config.startup_sort_column;
        let sort_dir = config.startup_sort_direction;

        let arena = &mut self.scratch.folded_arena;
        let entries = &mut self.scratch.schwartz_entries;
        arena.clear();
        entries.clear();

        let is_str_col = matches!(
            sort_col,
            StartupSortColumn::Name | StartupSortColumn::Publisher
        );

        for (idx, entry) in items.iter().enumerate() {
            if !matches_startup_query(entry, &query_lower, arena) {
                continue;
            }
            let tie_str = arena.push_folded(&entry.name);
            let (numeric, primary_str) = match sort_col {
                StartupSortColumn::Name => (0, tie_str),
                StartupSortColumn::Publisher => (
                    0,
                    arena.push_folded(entry.publisher.as_deref().unwrap_or("")),
                ),
                StartupSortColumn::Status => (u64::from(entry.enabled), (0, 0)),
                StartupSortColumn::Impact => (u64::from(startup_impact_rank(entry.impact)), (0, 0)),
            };
            entries.push(SchwartzEntry {
                index: idx,
                numeric,
                primary_str,
                tie_str,
            });
        }

        entries.sort_by(|a, b| {
            let ord = if is_str_col {
                arena.slice(a.primary_str).cmp(arena.slice(b.primary_str))
            } else {
                a.numeric.cmp(&b.numeric)
            };
            let directed = match sort_dir {
                SortDirection::Ascending => ord,
                SortDirection::Descending => ord.reverse(),
            };
            directed
                .then_with(|| arena.slice(a.tie_str).cmp(arena.slice(b.tie_str)))
                .then_with(|| items[a.index].id.cmp(&items[b.index].id))
        });

        let visible_indices = entries.iter().map(|e| e.index).collect();

        StartupSlice {
            revision,
            timestamp: self.latest_timestamp,
            last_bios_time_ms,
            sort_column: sort_col,
            sort_direction: sort_dir,
            items: items.to_vec(),
            visible_indices,
        }
    }

    /// Builds an [`AppHistorySlice`] by merging persisted baseline entries with live
    /// in-memory cumulative usage counters, and computes pre-filtered, Schwartzian-sorted
    /// `visible_indices` according to `config`.
    pub fn project_app_history(
        &mut self,
        since_epoch_secs: Option<u64>,
        persisted: &[AppHistoryRecord],
        config: &ViewProjectionConfig,
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
                .then_with(|| cmp_str_ascii_ci(&a.name, &b.name))
                .then_with(|| a.app_id.cmp(&b.app_id))
        });

        let query_lower = config.search_query.trim().to_lowercase();
        let sort_col = config.app_history_sort_column;
        let sort_dir = config.app_history_sort_direction;
        let is_str_col = matches!(sort_col, AppHistorySortColumn::Name);

        let arena = &mut self.scratch.folded_arena;
        let entries = &mut self.scratch.schwartz_entries;
        arena.clear();
        entries.clear();

        for (idx, record) in items.iter().enumerate() {
            if !query_lower.is_empty()
                && !arena.contains_icase(&record.name, &query_lower)
                && !arena.contains_icase(&record.app_id, &query_lower)
            {
                continue;
            }
            let tie_str = arena.push_folded(&record.name);
            let (numeric, primary_str) = match sort_col {
                AppHistorySortColumn::Name => (0, tie_str),
                AppHistorySortColumn::CpuTime => (record.cpu_time_100ns, (0, 0)),
                AppHistorySortColumn::Network => (record.network_bytes, (0, 0)),
                AppHistorySortColumn::MeteredNetwork => (record.metered_network_bytes, (0, 0)),
            };
            entries.push(SchwartzEntry {
                index: idx,
                numeric,
                primary_str,
                tie_str,
            });
        }

        entries.sort_by(|a, b| {
            let ord = if is_str_col {
                arena.slice(a.primary_str).cmp(arena.slice(b.primary_str))
            } else {
                a.numeric.cmp(&b.numeric)
            };
            let directed = match sort_dir {
                SortDirection::Ascending => ord,
                SortDirection::Descending => ord.reverse(),
            };
            directed
                .then_with(|| arena.slice(a.tie_str).cmp(arena.slice(b.tie_str)))
                .then_with(|| items[a.index].app_id.cmp(&items[b.index].app_id))
        });

        let visible_indices = entries.iter().map(|e| e.index).collect();

        AppHistorySlice {
            revision,
            timestamp: self.latest_timestamp,
            since_epoch_secs,
            sort_column: sort_col,
            sort_direction: sort_dir,
            items,
            visible_indices,
        }
    }

    /// Enriches OS user sessions with live per-session process aggregates, filters and sorts
    /// sessions and expanded child processes using Schwartzian folded keys, and flattens
    /// them into [`UsersRowProjection`] rows ready for O(1) virtualized rendering.
    pub fn project_users(
        &mut self,
        base_sessions: &[UserSessionRecord],
        config: &ViewProjectionConfig,
        cold_cache: &ColdMetaCache,
        revision: u64,
    ) -> UsersSlice {
        let mut sessions = Vec::with_capacity(base_sessions.len());

        for base in base_sessions {
            if self.computed.is_empty() && !base.processes.is_empty() {
                sessions.push(base.clone());
                continue;
            }
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
                    .then_with(|| cmp_str_ascii_ci(&a.name, &b.name))
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

        let query_lower = config.search_query.trim().to_lowercase();
        let sort_col = config.users_sort_column;
        let sort_dir = config.users_sort_direction;
        let is_str_col = matches!(sort_col, UsersSortColumn::User);

        let arena = &mut self.scratch.folded_arena;
        let session_entries = &mut self.scratch.schwartz_entries;
        let child_entries = &mut self.scratch.schwartz_children;
        arena.clear();
        session_entries.clear();

        for (idx, session) in sessions.iter().enumerate() {
            let (numeric, primary_str) = match sort_col {
                UsersSortColumn::User => (0, arena.push_folded(&session.user_name)),
                UsersSortColumn::Status => {
                    (u64::from(user_session_state_rank(session.state)), (0, 0))
                }
                UsersSortColumn::Cpu => (u64::from(session.cpu_permille), (0, 0)),
                UsersSortColumn::Memory => (session.memory_bytes, (0, 0)),
                UsersSortColumn::Disk => (session.disk_bytes_per_sec, (0, 0)),
                UsersSortColumn::Network => (session.net_bytes_per_sec, (0, 0)),
            };
            session_entries.push(SchwartzEntry {
                index: idx,
                numeric,
                primary_str,
                tie_str: (0, 0),
            });
        }

        session_entries.sort_by(|a, b| {
            let ord = if is_str_col {
                arena.slice(a.primary_str).cmp(arena.slice(b.primary_str))
            } else {
                a.numeric.cmp(&b.numeric)
            };
            let directed = match sort_dir {
                SortDirection::Ascending => ord,
                SortDirection::Descending => ord.reverse(),
            };
            directed.then_with(|| {
                sessions[a.index]
                    .session_id
                    .cmp(&sessions[b.index].session_id)
            })
        });

        let mut rows = Vec::with_capacity(sessions.len().saturating_mul(2));
        for entry in session_entries.iter() {
            let s_idx = entry.index;
            let session = &sessions[s_idx];
            let has_children = !session.processes.is_empty();

            if query_lower.is_empty() {
                let is_expanded =
                    has_children && config.users_expanded.contains(&session.session_id);
                rows.push(UsersRowProjection::Session {
                    session_id: session.session_id,
                    user_name: Arc::clone(&session.user_name),
                    state: session.state,
                    process_count: session.process_count,
                    cpu_permille: session.cpu_permille,
                    memory_bytes: session.memory_bytes,
                    disk_bytes_per_sec: session.disk_bytes_per_sec,
                    net_bytes_per_sec: session.net_bytes_per_sec,
                    has_children,
                    is_expanded,
                });
                if is_expanded {
                    push_sorted_user_children(
                        session,
                        None,
                        sort_col,
                        sort_dir,
                        arena,
                        child_entries,
                        &mut rows,
                    );
                }
            } else {
                let header_matches = arena.contains_icase(&session.user_name, &query_lower)
                    || u32_contains_substr(session.session_id, &query_lower);

                child_entries.clear();
                let mut has_matching_child = false;
                for (p_idx, proc_item) in session.processes.iter().enumerate() {
                    if arena.contains_icase(&proc_item.display_name, &query_lower)
                        || arena.contains_icase(&proc_item.name, &query_lower)
                        || u32_contains_substr(proc_item.key.pid, &query_lower)
                    {
                        has_matching_child = true;
                        let tie_str = arena.push_folded(&proc_item.display_name);
                        let (numeric, primary_str) =
                            user_child_sort_key(proc_item, sort_col, tie_str);
                        child_entries.push(SchwartzEntry {
                            index: p_idx,
                            numeric,
                            primary_str,
                            tie_str,
                        });
                    }
                }

                if !header_matches && !has_matching_child {
                    continue;
                }

                let auto_expand = has_matching_child;
                let is_expanded = auto_expand
                    || (has_children && config.users_expanded.contains(&session.session_id));

                rows.push(UsersRowProjection::Session {
                    session_id: session.session_id,
                    user_name: Arc::clone(&session.user_name),
                    state: session.state,
                    process_count: session.process_count,
                    cpu_permille: session.cpu_permille,
                    memory_bytes: session.memory_bytes,
                    disk_bytes_per_sec: session.disk_bytes_per_sec,
                    net_bytes_per_sec: session.net_bytes_per_sec,
                    has_children,
                    is_expanded,
                });

                if auto_expand {
                    sort_and_emit_user_children(
                        session,
                        sort_col,
                        sort_dir,
                        arena,
                        child_entries,
                        &mut rows,
                    );
                } else if is_expanded {
                    push_sorted_user_children(
                        session,
                        None,
                        sort_col,
                        sort_dir,
                        arena,
                        child_entries,
                        &mut rows,
                    );
                }
            }
        }

        UsersSlice {
            revision,
            timestamp: self.latest_timestamp,
            sort_column: sort_col,
            sort_direction: sort_dir,
            sessions,
            rows,
        }
    }

    fn project_processes_grouped(
        &mut self,
        config: &ViewProjectionConfig,
        cold_cache: &ColdMetaCache,
        parsed_query: &ParsedQuery,
        use_frozen: bool,
        deadband: Option<u64>,
    ) -> Vec<ProcessRowProjection> {
        if self.platform.kind == PlatformKind::Windows {
            return self.project_processes_grouped_windows(
                config,
                cold_cache,
                parsed_query,
                use_frozen,
                deadband,
            );
        }

        let has_query = !parsed_query.is_empty();
        let Self {
            computed,
            latest_tree,
            listening_ports,
            leak_detector,
            total_disk_bytes_per_sec,
            total_net_bytes_per_sec,
            scratch,
            ..
        } = self;

        let n = computed.len();
        let total_disk_bps = *total_disk_bytes_per_sec;
        let total_net_bps = *total_net_bytes_per_sec;

        let ProjectionScratch {
            ref frozen_ranks,
            ref dense_deadband,
            bool_mask_a: ref mut in_any_section,
            bool_mask_b: ref mut visited,
            ref mut stack,
            ref mut section_roots,
            ref mut visible_children,
            ref mut by_category,
            ..
        } = *scratch;
        let frozen_ranks = use_frozen.then_some(frozen_ranks);
        let deadband_ctx = deadband.map(|db| DeadbandContext {
            deadband: db,
            dense_prev: dense_deadband.as_slice(),
        });

        for vec in by_category.iter_mut() {
            vec.clear();
        }
        in_any_section.clear();
        in_any_section.resize(n, false);

        for (idx, proc) in computed.iter().enumerate() {
            if is_hidden_system_process(proc, config.hide_windows_system_processes) {
                continue;
            }
            if has_query
                && !matches_query_with_ports(proc, cold_cache, parsed_query, listening_ports, false)
            {
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
        visited.clear();
        visited.resize(n, false);
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
                let has_same_cat_parent = latest_tree.parent_index(idx).is_some_and(|p_idx| {
                    in_any_section[p_idx] && computed[p_idx].category == category
                });
                if !has_same_cat_parent {
                    section_roots.push(idx);
                }
            }

            sort_process_indices_slice(
                computed,
                section_roots,
                cold_cache,
                config.process_sort_column,
                config.process_sort_direction,
                frozen_ranks,
                deadband_ctx,
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

                let proc = &computed[idx];
                visible_children.clear();
                for &c_idx in latest_tree.children_indices(idx) {
                    if in_any_section[c_idx] && computed[c_idx].category == category {
                        visible_children.push(c_idx);
                    }
                }

                let has_children = !visible_children.is_empty();
                let is_expanded = has_children
                    && (has_query || (has_expanded && config.process_expanded.contains(&proc.key)));
                let child_count = visible_children.len() as u32;

                rows.push(build_process_row_from_parts(
                    proc,
                    latest_tree,
                    leak_detector,
                    total_disk_bps,
                    total_net_bps,
                    config,
                    cold_cache,
                    depth,
                    has_children,
                    is_expanded,
                    child_count,
                ));

                if is_expanded {
                    sort_process_indices_slice(
                        computed,
                        visible_children,
                        cold_cache,
                        config.process_sort_column,
                        config.process_sort_direction,
                        frozen_ranks,
                        deadband_ctx,
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
    /// - When `config.webview2_host_grouping` is `true`, attributes `msedgewebview2.exe`
    ///   processes to their matching host `App` group via `--webview-exe-name=` or their
    ///   verified `ProcessKey` ancestor chain.
    /// - Never pulls other different-executable child processes (e.g. `crashpad_handler.exe`,
    ///   `WeChatAppEx.exe`) into an `App` group; those remain in `Background`.
    /// - Every unique `App` identity appears once at depth 0 in `Apps` (never nested
    ///   under `explorer.exe` or another app) with a deterministic leader, total group
    ///   count (`child_count = total_members` when `>= 2`), aggregated resource metrics,
    ///   and rolled-up status flags (`EFFICIENCY_MODE` / `NOT_RESPONDING` on any member,
    ///   `SUSPENDED` only when all members are suspended).
    /// - `explorer.exe` is treated as a desktop shell launcher across all sections and
    ///   never adopts different-image child processes.
    fn project_processes_grouped_windows(
        &mut self,
        config: &ViewProjectionConfig,
        cold_cache: &ColdMetaCache,
        parsed_query: &ParsedQuery,
        use_frozen: bool,
        deadband: Option<u64>,
    ) -> Vec<ProcessRowProjection> {
        let has_query = !parsed_query.is_empty();
        let Self {
            computed,
            latest_tree,
            listening_ports,
            leak_detector,
            latest_perf,
            total_disk_bytes_per_sec,
            total_net_bytes_per_sec,
            scratch,
            ..
        } = self;

        let n = computed.len();
        let mem_total_bytes = latest_perf.mem_total_bytes;
        let total_disk_bps = *total_disk_bytes_per_sec;
        let total_net_bps = *total_net_bytes_per_sec;

        let ProjectionScratch {
            ref frozen_ranks,
            ref dense_deadband,
            bool_mask_a: ref mut in_remaining_section,
            bool_mask_b: ref mut visited,
            ref mut stack,
            ref mut section_roots,
            ref mut visible_children,
            ref mut by_category,
            ref mut deferred_webview2,
            ref mut subtree_descendants,
            ref mut desc_stack,
            ref mut group_agg,
            ref mut subtree_counts,
            ref mut idx_to_app_group,
            ref mut app_group_leaders,
            ref mut app_group_children,
            ref mut visible_apps,
            ref mut pkg_to_group,
            ref mut exe_to_group,
            ref mut name_to_groups,
            ..
        } = *scratch;
        let frozen_ranks = use_frozen.then_some(frozen_ranks);
        let deadband_ctx = deadband.map(|db| DeadbandContext {
            deadband: db,
            dense_prev: dense_deadband.as_slice(),
        });

        app_group_leaders.clear();
        idx_to_app_group.clear();
        idx_to_app_group.resize(n, None);
        pkg_to_group.clear();
        exe_to_group.clear();
        name_to_groups.clear();

        // Step 1: Build unique App-identity groups from all visible-window App processes.
        for (idx, proc) in computed.iter().enumerate() {
            if proc.category != ProcessCategory::App {
                continue;
            }
            let pkg = process_app_group_id(cold_cache, proc.key);
            let exe = process_normalized_exe_slice(cold_cache, proc.key);

            let matched_group = pkg
                .and_then(|p| {
                    let h = hash_session_ascii_ci(proc.session_id, p);
                    pkg_to_group.find(h, |_g_idx, sample_idx| {
                        let s = &computed[sample_idx];
                        s.session_id == proc.session_id
                            && process_app_group_id(cold_cache, s.key)
                                .is_some_and(|sp| sp.eq_ignore_ascii_case(p))
                    })
                })
                .or_else(|| {
                    exe.and_then(|e| {
                        let h = hash_session_ascii_ci(proc.session_id, e);
                        exe_to_group.find(h, |_g_idx, sample_idx| {
                            let s = &computed[sample_idx];
                            s.session_id == proc.session_id
                                && process_normalized_exe_slice(cold_cache, s.key)
                                    .is_some_and(|se| se.eq_ignore_ascii_case(e))
                        })
                    })
                })
                .or_else(|| {
                    if proc.name.is_empty() {
                        return None;
                    }
                    let h = hash_session_ascii_ci(proc.session_id, &proc.name);
                    name_to_groups.find(h, |g_idx, sample_idx| {
                        let s = &computed[sample_idx];
                        if s.session_id != proc.session_id
                            || !s.name.eq_ignore_ascii_case(&proc.name)
                        {
                            return false;
                        }
                        let leader_key = computed[app_group_leaders[g_idx]].key;
                        exe_paths_compatible(cold_cache, leader_key, proc.key)
                            && app_group_ids_compatible(cold_cache, leader_key, proc.key)
                    })
                });

            let g_idx = if let Some(g_idx) = matched_group {
                let leader_idx = &mut app_group_leaders[g_idx];
                if is_better_app_leader(proc, &computed[*leader_idx]) {
                    let prev_leader = *leader_idx;
                    *leader_idx = idx;
                    app_group_children[g_idx].push(prev_leader);
                } else {
                    app_group_children[g_idx].push(idx);
                }
                g_idx
            } else {
                let g_idx = app_group_leaders.len();
                app_group_leaders.push(idx);
                if g_idx < app_group_children.len() {
                    app_group_children[g_idx].clear();
                } else {
                    app_group_children.push(Vec::new());
                }
                if !proc.name.is_empty() {
                    let h = hash_session_ascii_ci(proc.session_id, &proc.name);
                    name_to_groups.push(h, g_idx, idx);
                }
                g_idx
            };

            idx_to_app_group[idx] = Some(g_idx);

            if let Some(p) = pkg {
                let h = hash_session_ascii_ci(proc.session_id, p);
                let exists = pkg_to_group
                    .find(h, |_g, sample_idx| {
                        let s = &computed[sample_idx];
                        s.session_id == proc.session_id
                            && process_app_group_id(cold_cache, s.key)
                                .is_some_and(|sp| sp.eq_ignore_ascii_case(p))
                    })
                    .is_some();
                if !exists {
                    pkg_to_group.push(h, g_idx, idx);
                }
            }
            if let Some(e) = exe {
                let h = hash_session_ascii_ci(proc.session_id, e);
                let exists = exe_to_group
                    .find(h, |_g, sample_idx| {
                        let s = &computed[sample_idx];
                        s.session_id == proc.session_id
                            && process_normalized_exe_slice(cold_cache, s.key)
                                .is_some_and(|se| se.eq_ignore_ascii_case(e))
                    })
                    .is_some();
                if !exists {
                    exe_to_group.push(h, g_idx, idx);
                }
            }
        }

        // Step 2: Attach windowless Background processes strictly by matching App identity
        // (plus optional WebView2 host attribution when `webview2_host_grouping` is enabled).
        let [_, bg_indices, sys_indices] = by_category;
        bg_indices.clear();
        sys_indices.clear();
        in_remaining_section.clear();
        in_remaining_section.resize(n, false);
        deferred_webview2.clear();

        for (idx, proc) in computed.iter().enumerate() {
            match proc.category {
                ProcessCategory::App => {}
                ProcessCategory::Background => {
                    if config.webview2_host_grouping && is_webview2_image(&proc.name) {
                        deferred_webview2.push(idx);
                        continue;
                    }

                    let pkg = process_app_group_id(cold_cache, proc.key);
                    let exe = process_normalized_exe_slice(cold_cache, proc.key);

                    let matched_group = pkg
                        .and_then(|p| {
                            let h = hash_session_ascii_ci(proc.session_id, p);
                            pkg_to_group.find(h, |_g_idx, sample_idx| {
                                let s = &computed[sample_idx];
                                s.session_id == proc.session_id
                                    && process_app_group_id(cold_cache, s.key)
                                        .is_some_and(|sp| sp.eq_ignore_ascii_case(p))
                            })
                        })
                        .or_else(|| {
                            exe.and_then(|e| {
                                let h = hash_session_ascii_ci(proc.session_id, e);
                                exe_to_group.find(h, |_g_idx, sample_idx| {
                                    let s = &computed[sample_idx];
                                    s.session_id == proc.session_id
                                        && process_normalized_exe_slice(cold_cache, s.key)
                                            .is_some_and(|se| se.eq_ignore_ascii_case(e))
                                })
                            })
                        })
                        .or_else(|| {
                            if proc.name.is_empty() || is_windows_shell_launcher(&proc.name) {
                                return None;
                            }
                            let h = hash_session_ascii_ci(proc.session_id, &proc.name);
                            name_to_groups.find(h, |g_idx, sample_idx| {
                                let s = &computed[sample_idx];
                                if s.session_id != proc.session_id
                                    || !s.name.eq_ignore_ascii_case(&proc.name)
                                {
                                    return false;
                                }
                                let leader_key = computed[app_group_leaders[g_idx]].key;
                                exe_paths_compatible(cold_cache, leader_key, proc.key)
                                    && app_group_ids_compatible(cold_cache, leader_key, proc.key)
                            })
                        });

                    if let Some(g_idx) = matched_group {
                        app_group_children[g_idx].push(idx);
                        idx_to_app_group[idx] = Some(g_idx);
                    } else if !has_query
                        || matches_query_with_ports(
                            proc,
                            cold_cache,
                            parsed_query,
                            listening_ports,
                            false,
                        )
                    {
                        in_remaining_section[idx] = true;
                        bg_indices.push(idx);
                    }
                }
                ProcessCategory::System => {
                    if is_hidden_system_process(proc, config.hide_windows_system_processes) {
                        continue;
                    }
                    if !has_query
                        || matches_query_with_ports(
                            proc,
                            cold_cache,
                            parsed_query,
                            listening_ports,
                            false,
                        )
                    {
                        in_remaining_section[idx] = true;
                        sys_indices.push(idx);
                    }
                }
            }
        }

        // Step 2b: Attribute `msedgewebview2.exe` processes to their host App group after
        // all host app members are indexed in `idx_to_app_group`.
        for &idx in deferred_webview2.iter() {
            let proc = &computed[idx];
            let host_group = resolve_webview2_host_group_from_parts(
                computed,
                latest_tree,
                idx,
                cold_cache,
                name_to_groups,
                idx_to_app_group,
            );
            if let Some(g_idx) = host_group {
                app_group_children[g_idx].push(idx);
                idx_to_app_group[idx] = Some(g_idx);
            } else if !has_query
                || matches_query_with_ports(proc, cold_cache, parsed_query, listening_ports, false)
            {
                in_remaining_section[idx] = true;
                bg_indices.push(idx);
            }
        }

        let mut rows = Vec::with_capacity(n + 3);
        let has_expanded = !config.process_expanded.is_empty();

        // 1. Emit Apps section with aggregated metrics and rolled-up status flags.
        visible_apps.clear();
        for g_idx in 0..app_group_leaders.len() {
            let leader_idx = app_group_leaders[g_idx];
            let agg = compute_group_aggregate_from_parts(
                computed,
                leak_detector,
                mem_total_bytes,
                leader_idx,
                &app_group_children[g_idx],
                config,
                cold_cache,
            );
            if has_query {
                let root_matches = matches_query_with_ports(
                    &computed[leader_idx],
                    cold_cache,
                    parsed_query,
                    listening_ports,
                    false,
                );
                app_group_children[g_idx].retain(|&c_idx| {
                    matches_query_with_ports(
                        &computed[c_idx],
                        cold_cache,
                        parsed_query,
                        listening_ports,
                        false,
                    )
                });
                if root_matches || !app_group_children[g_idx].is_empty() {
                    visible_apps.push((leader_idx, g_idx, agg));
                }
            } else {
                visible_apps.push((leader_idx, g_idx, agg));
            }
        }

        if !visible_apps.is_empty() {
            rows.push(section_header_row(ProcessCategory::App, visible_apps.len()));

            let sort_col = config.process_sort_column;
            let sort_dir = config.process_sort_direction;
            visible_apps.sort_by(|(ia, _, agg_a), (ib, _, agg_b)| {
                compare_process_items(
                    *ia,
                    *ib,
                    computed,
                    Some(agg_a),
                    Some(agg_b),
                    cold_cache,
                    sort_col,
                    sort_dir,
                    frozen_ranks,
                    deadband_ctx,
                )
            });

            for (leader_idx, g_idx, agg) in visible_apps.iter() {
                let leader = &computed[*leader_idx];
                let children = &mut app_group_children[*g_idx];
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

                rows.push(build_aggregated_process_row_from_parts(
                    leader,
                    agg,
                    latest_tree,
                    leak_detector,
                    total_disk_bps,
                    total_net_bps,
                    config,
                    cold_cache,
                    0,
                    has_children,
                    is_expanded,
                    child_count,
                ));

                if is_expanded {
                    sort_process_indices_slice(
                        computed,
                        children,
                        cold_cache,
                        config.process_sort_column,
                        config.process_sort_direction,
                        frozen_ranks,
                        deadband_ctx,
                    );
                    for &c_idx in children.iter() {
                        rows.push(build_process_row_from_parts(
                            &computed[c_idx],
                            latest_tree,
                            leak_detector,
                            total_disk_bps,
                            total_net_bps,
                            config,
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
        visited.clear();
        visited.resize(n, false);
        group_agg.clear();
        group_agg.resize(n, None);
        subtree_counts.clear();
        subtree_counts.resize(n, 0);

        for (category, indices) in [
            (ProcessCategory::Background, bg_indices.as_slice()),
            (ProcessCategory::System, sys_indices.as_slice()),
        ] {
            if indices.is_empty() {
                continue;
            }

            section_roots.clear();
            for &idx in indices {
                let has_same_cat_parent = latest_tree.parent_index(idx).is_some_and(|p_idx| {
                    in_remaining_section[p_idx]
                        && computed[p_idx].category == category
                        && same_windows_app_identity(&computed[p_idx], &computed[idx], cold_cache)
                });
                if !has_same_cat_parent {
                    section_roots.push(idx);
                }
            }

            // Precompute aggregated metrics and total member counts for collapsible section rows.
            for &idx in indices {
                let proc = &computed[idx];
                subtree_descendants.clear();
                desc_stack.clear();
                for &c_idx in latest_tree.children_indices(idx) {
                    if in_remaining_section[c_idx]
                        && computed[c_idx].category == category
                        && same_windows_app_identity(proc, &computed[c_idx], cold_cache)
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
                        let d_proc = &computed[d_idx];
                        for &gc_idx in latest_tree.children_indices(d_idx) {
                            if in_remaining_section[gc_idx]
                                && computed[gc_idx].category == category
                                && same_windows_app_identity(d_proc, &computed[gc_idx], cold_cache)
                            {
                                desc_stack.push(gc_idx);
                            }
                        }
                    }
                    subtree_counts[idx] = (subtree_descendants.len() + 1) as u32;
                    group_agg[idx] = Some(compute_group_aggregate_from_parts(
                        computed,
                        leak_detector,
                        mem_total_bytes,
                        idx,
                        subtree_descendants,
                        config,
                        cold_cache,
                    ));
                }
            }

            rows.push(section_header_row(category, section_roots.len()));

            sort_process_indices_with_aggregates_slice(
                computed,
                section_roots,
                group_agg,
                cold_cache,
                config.process_sort_column,
                config.process_sort_direction,
                frozen_ranks,
                deadband_ctx,
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

                let proc = &computed[idx];
                visible_children.clear();
                for &c_idx in latest_tree.children_indices(idx) {
                    if in_remaining_section[c_idx]
                        && computed[c_idx].category == category
                        && same_windows_app_identity(proc, &computed[c_idx], cold_cache)
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

                if let Some(ref agg) = group_agg[idx] {
                    rows.push(build_aggregated_process_row_from_parts(
                        proc,
                        agg,
                        latest_tree,
                        leak_detector,
                        total_disk_bps,
                        total_net_bps,
                        config,
                        cold_cache,
                        depth,
                        has_children,
                        is_expanded,
                        child_count,
                    ));
                } else {
                    rows.push(build_process_row_from_parts(
                        proc,
                        latest_tree,
                        leak_detector,
                        total_disk_bps,
                        total_net_bps,
                        config,
                        cold_cache,
                        depth,
                        has_children,
                        is_expanded,
                        child_count,
                    ));
                }

                if is_expanded {
                    sort_process_indices_with_aggregates_slice(
                        computed,
                        visible_children,
                        group_agg,
                        cold_cache,
                        config.process_sort_column,
                        config.process_sort_direction,
                        frozen_ranks,
                        deadband_ctx,
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
        &mut self,
        config: &ViewProjectionConfig,
        cold_cache: &ColdMetaCache,
        parsed_query: &ParsedQuery,
        use_frozen: bool,
        deadband: Option<u64>,
    ) -> Vec<ProcessRowProjection> {
        let has_query = !parsed_query.is_empty();
        let Self {
            computed,
            latest_tree,
            listening_ports,
            leak_detector,
            total_disk_bytes_per_sec,
            total_net_bytes_per_sec,
            scratch,
            ..
        } = self;

        let n = computed.len();
        let total_disk_bps = *total_disk_bytes_per_sec;
        let total_net_bps = *total_net_bytes_per_sec;

        let ProjectionScratch {
            ref frozen_ranks,
            ref dense_deadband,
            bool_mask_a: ref mut visible_mask,
            bool_mask_b: ref mut visited,
            bool_mask_c: ref mut included_mask,
            ref mut stack,
            section_roots: ref mut roots,
            ref mut visible_children,
            ..
        } = *scratch;
        let frozen_ranks = use_frozen.then_some(frozen_ranks);
        let deadband_ctx = deadband.map(|db| DeadbandContext {
            deadband: db,
            dense_prev: dense_deadband.as_slice(),
        });

        visible_mask.clear();
        visible_mask.extend(
            computed
                .iter()
                .map(|p| !is_hidden_system_process(p, config.hide_windows_system_processes)),
        );
        if has_query {
            collect_matching_with_ancestors_into(
                computed,
                latest_tree,
                visible_mask,
                included_mask,
                |proc| {
                    matches_query_with_ports(proc, cold_cache, parsed_query, listening_ports, false)
                },
            );
        }

        let is_included =
            |idx: usize| -> bool { visible_mask[idx] && (!has_query || included_mask[idx]) };

        // Any included node whose verified parent is absent or hidden/excluded becomes a tree root.
        roots.clear();
        for idx in 0..n {
            if is_included(idx)
                && latest_tree
                    .parent_index(idx)
                    .is_none_or(|p_idx| !is_included(p_idx))
            {
                roots.push(idx);
            }
        }

        sort_process_indices_slice(
            computed,
            roots,
            cold_cache,
            config.process_sort_column,
            config.process_sort_direction,
            frozen_ranks,
            deadband_ctx,
        );

        let mut rows = Vec::with_capacity(n);
        visited.clear();
        visited.resize(n, false);
        stack.clear();
        let has_expanded = !config.process_expanded.is_empty();

        for &root_idx in roots.iter().rev() {
            stack.push((root_idx, 0));
        }

        while let Some((idx, depth)) = stack.pop() {
            if visited[idx] {
                continue;
            }
            visited[idx] = true;

            let proc = &computed[idx];
            visible_children.clear();
            for &c_idx in latest_tree.children_indices(idx) {
                if is_included(c_idx) {
                    visible_children.push(c_idx);
                }
            }

            let has_children = !visible_children.is_empty();
            let is_expanded = has_children
                && (has_query || (has_expanded && config.process_expanded.contains(&proc.key)));
            let child_count = visible_children.len() as u32;

            rows.push(build_process_row_from_parts(
                proc,
                latest_tree,
                leak_detector,
                total_disk_bps,
                total_net_bps,
                config,
                cold_cache,
                depth,
                has_children,
                is_expanded,
                child_count,
            ));

            if is_expanded {
                sort_process_indices_slice(
                    computed,
                    visible_children,
                    cold_cache,
                    config.process_sort_column,
                    config.process_sort_direction,
                    frozen_ranks,
                    deadband_ctx,
                );
                for &c_idx in visible_children.iter().rev() {
                    stack.push((c_idx, depth.saturating_add(1)));
                }
            }
        }

        rows
    }

    fn project_details_flat(
        &mut self,
        config: &ViewProjectionConfig,
        cold_cache: &ColdMetaCache,
        parsed_query: &ParsedQuery,
        use_frozen: bool,
        deadband: Option<u64>,
    ) -> Vec<DetailsRowProjection> {
        let has_query = !parsed_query.is_empty();
        let Self {
            computed,
            latest_tree,
            listening_ports,
            leak_detector,
            scratch,
            ..
        } = self;

        let ProjectionScratch {
            ref frozen_ranks,
            ref dense_deadband,
            section_roots: ref mut indices,
            ..
        } = *scratch;
        let frozen_ranks = use_frozen.then_some(frozen_ranks);
        let deadband_ctx = deadband.map(|db| DeadbandContext {
            deadband: db,
            dense_prev: dense_deadband.as_slice(),
        });

        indices.clear();
        for (idx, proc) in computed.iter().enumerate() {
            if !is_hidden_system_process(proc, config.hide_windows_system_processes)
                && (!has_query
                    || matches_query_with_ports(
                        proc,
                        cold_cache,
                        parsed_query,
                        listening_ports,
                        true,
                    ))
            {
                indices.push(idx);
            }
        }

        sort_details_indices_slice(
            computed,
            indices,
            cold_cache,
            config.details_sort_column,
            config.details_sort_direction,
            frozen_ranks,
            deadband_ctx,
        );

        let mut rows = Vec::with_capacity(indices.len());
        for &idx in indices.iter() {
            rows.push(build_details_row_from_parts(
                computed,
                latest_tree,
                leak_detector,
                idx,
                config,
                cold_cache,
                0,
                false,
                false,
            ));
        }
        rows
    }

    fn project_details_tree(
        &mut self,
        config: &ViewProjectionConfig,
        cold_cache: &ColdMetaCache,
        parsed_query: &ParsedQuery,
        use_frozen: bool,
        deadband: Option<u64>,
    ) -> Vec<DetailsRowProjection> {
        let has_query = !parsed_query.is_empty();
        let Self {
            computed,
            latest_tree,
            listening_ports,
            leak_detector,
            scratch,
            ..
        } = self;

        let n = computed.len();

        let ProjectionScratch {
            ref frozen_ranks,
            ref dense_deadband,
            bool_mask_a: ref mut visible_mask,
            bool_mask_b: ref mut visited,
            bool_mask_c: ref mut included_mask,
            ref mut stack,
            section_roots: ref mut roots,
            ref mut visible_children,
            ..
        } = *scratch;
        let frozen_ranks = use_frozen.then_some(frozen_ranks);
        let deadband_ctx = deadband.map(|db| DeadbandContext {
            deadband: db,
            dense_prev: dense_deadband.as_slice(),
        });

        visible_mask.clear();
        visible_mask.extend(
            computed
                .iter()
                .map(|p| !is_hidden_system_process(p, config.hide_windows_system_processes)),
        );
        if has_query {
            collect_matching_with_ancestors_into(
                computed,
                latest_tree,
                visible_mask,
                included_mask,
                |proc| {
                    matches_query_with_ports(proc, cold_cache, parsed_query, listening_ports, true)
                },
            );
        }

        let is_included =
            |idx: usize| -> bool { visible_mask[idx] && (!has_query || included_mask[idx]) };

        roots.clear();
        for idx in 0..n {
            if is_included(idx)
                && latest_tree
                    .parent_index(idx)
                    .is_none_or(|p_idx| !is_included(p_idx))
            {
                roots.push(idx);
            }
        }

        sort_details_indices_slice(
            computed,
            roots,
            cold_cache,
            config.details_sort_column,
            config.details_sort_direction,
            frozen_ranks,
            deadband_ctx,
        );

        let mut rows = Vec::with_capacity(n);
        visited.clear();
        visited.resize(n, false);
        stack.clear();
        let has_expanded = !config.details_expanded.is_empty();

        for &root_idx in roots.iter().rev() {
            stack.push((root_idx, 0));
        }

        while let Some((idx, depth)) = stack.pop() {
            if visited[idx] {
                continue;
            }
            visited[idx] = true;

            let key = computed[idx].key;
            visible_children.clear();
            for &c_idx in latest_tree.children_indices(idx) {
                if is_included(c_idx) {
                    visible_children.push(c_idx);
                }
            }

            let has_children = !visible_children.is_empty();
            let is_expanded = has_children
                && (has_query || (has_expanded && config.details_expanded.contains(&key)));

            rows.push(build_details_row_from_parts(
                computed,
                latest_tree,
                leak_detector,
                idx,
                config,
                cold_cache,
                depth,
                has_children,
                is_expanded,
            ));

            if is_expanded {
                sort_details_indices_slice(
                    computed,
                    visible_children,
                    cold_cache,
                    config.details_sort_column,
                    config.details_sort_direction,
                    frozen_ranks,
                    deadband_ctx,
                );
                for &c_idx in visible_children.iter().rev() {
                    stack.push((c_idx, depth.saturating_add(1)));
                }
            }
        }

        rows
    }
}

#[inline]
const fn process_column_deadband(column: ProcessSortColumn) -> Option<u64> {
    match column {
        ProcessSortColumn::Cpu => Some(CPU_SORT_DEADBAND_PERMILLE as u64),
        ProcessSortColumn::Gpu => Some(GPU_SORT_DEADBAND_PERMILLE as u64),
        ProcessSortColumn::Disk | ProcessSortColumn::Network => Some(RATE_SORT_DEADBAND_BPS),
        _ => None,
    }
}

#[inline]
const fn details_column_deadband(column: DetailsSortColumn) -> Option<u64> {
    match column {
        DetailsSortColumn::Cpu => Some(CPU_SORT_DEADBAND_PERMILLE as u64),
        _ => None,
    }
}

#[inline]
const fn process_row_deadband_metric(row: &ProcessRowProjection, column: ProcessSortColumn) -> u64 {
    match column {
        ProcessSortColumn::Cpu => row.cpu_permille as u64,
        ProcessSortColumn::Gpu => row.gpu_util_permille as u64,
        ProcessSortColumn::Disk => row.disk_bytes_per_sec,
        ProcessSortColumn::Network => row.net_bytes_per_sec,
        _ => 0,
    }
}

/// Computes the hysteresis-stabilized effective sort metric from a pre-populated dense entry.
///
/// Because this depends solely on the element's own live value and immutable previous-tick
/// sticky metric (never on pairs of compared elements), sorting by the returned value defines
/// a strict total order and can never violate `slice::sort_by` transitivity, while avoiding
/// any `HashMap` lookup inside the `O(N log N)` comparator.
#[inline(always)]
fn resolve_sticky_metric_dense(entry: DenseDeadbandEntry, live_val: u64, deadband: u64) -> u64 {
    if entry.has_prev && live_val.abs_diff(entry.prev_sticky) < deadband {
        entry.prev_sticky
    } else {
        live_val
    }
}

#[inline]
fn hash_session_ascii_ci(session_id: u32, s: &str) -> u64 {
    let mut h = FastHasher::new();
    h.write_u32(session_id);
    for b in s.bytes() {
        h.write_u8(b.to_ascii_lowercase());
    }
    h.write_u8(0xff);
    h.finish()
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn build_process_row_from_parts(
    proc: &ComputedProcess,
    latest_tree: &ProcessTree,
    leak_detector: &LeakDetector,
    total_disk_bytes_per_sec: u64,
    total_net_bytes_per_sec: u64,
    config: &ViewProjectionConfig,
    cold_cache: &ColdMetaCache,
    depth: u16,
    has_children: bool,
    is_expanded: bool,
    child_count: u32,
) -> ProcessRowProjection {
    let cold_entry = cold_cache.get_entry(proc.key);
    let cold = cold_entry.map(|e| &e.meta);
    let asset = cold.and_then(|meta| meta.asset.as_ref());

    let publisher = asset.and_then(|a| a.publisher.clone());
    let exe_path = cold.and_then(|meta| meta.exe_path.clone());
    let command_line = cold.and_then(|meta| meta.command_line.clone());
    let icon = asset.and_then(|a| a.icon.clone());
    let display_name = resolve_display_name(proc, cold_cache);
    let status = effective_status(proc, cold_cache);
    let disk_permille =
        compute_throughput_permille(proc.disk_bytes_per_sec, total_disk_bytes_per_sec);
    let net_permille = compute_throughput_permille(proc.net_bytes_per_sec, total_net_bytes_per_sec);
    let parent_key = latest_tree.parent(proc.key);
    let webview2_role = if is_webview2_image(&proc.name) {
        cold_entry.and_then(|e| e.webview2_role).or_else(|| {
            cold.filter(|m| m.exe_path.is_none())
                .and_then(|m| classify_webview2(&proc.name, m.command_line.as_deref()))
                .map(|parsed| parsed.role)
        })
    } else {
        None
    };
    let leak_warning = leak_detector.warning_for(
        proc.key,
        &proc.name,
        exe_path.as_deref(),
        config.show_leak_warnings,
        &config.leak_ignored_paths,
    );

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
        parent_key,
        leak_warning,
        webview2_role,
    }
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn build_details_row_from_parts(
    computed: &[ComputedProcess],
    latest_tree: &ProcessTree,
    leak_detector: &LeakDetector,
    idx: usize,
    config: &ViewProjectionConfig,
    cold_cache: &ColdMetaCache,
    depth: u16,
    has_children: bool,
    is_expanded: bool,
) -> DetailsRowProjection {
    let proc = &computed[idx];
    let cold_entry = cold_cache.get_entry(proc.key);
    let cold = cold_entry.map(|e| &e.meta);
    let asset = cold.and_then(|m| m.asset.as_ref());
    let parent_key = latest_tree.parent_index(idx).map(|p| latest_tree.key_at(p));
    let status = effective_status(proc, cold_cache);
    let exe_path = cold.and_then(|m| m.exe_path.clone());
    let command_line = cold.and_then(|m| m.command_line.clone());
    let webview2_role = if is_webview2_image(&proc.name) {
        cold_entry.and_then(|e| e.webview2_role).or_else(|| {
            cold.filter(|m| m.exe_path.is_none())
                .and_then(|m| classify_webview2(&proc.name, m.command_line.as_deref()))
                .map(|parsed| parsed.role)
        })
    } else {
        None
    };
    let leak_warning = leak_detector.warning_for(
        proc.key,
        &proc.name,
        exe_path.as_deref(),
        config.show_leak_warnings,
        &config.leak_ignored_paths,
    );

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
        exe_path,
        command_line,
        leak_warning,
        webview2_role,
    }
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn build_aggregated_process_row_from_parts(
    proc: &ComputedProcess,
    agg: &GroupAggregate,
    latest_tree: &ProcessTree,
    leak_detector: &LeakDetector,
    total_disk_bytes_per_sec: u64,
    total_net_bytes_per_sec: u64,
    config: &ViewProjectionConfig,
    cold_cache: &ColdMetaCache,
    depth: u16,
    has_children: bool,
    is_expanded: bool,
    child_count: u32,
) -> ProcessRowProjection {
    let mut row = build_process_row_from_parts(
        proc,
        latest_tree,
        leak_detector,
        total_disk_bytes_per_sec,
        total_net_bytes_per_sec,
        config,
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
        compute_throughput_permille(agg.disk_bytes_per_sec, total_disk_bytes_per_sec);
    row.net_bytes_per_sec = agg.net_bytes_per_sec;
    row.net_permille = compute_throughput_permille(agg.net_bytes_per_sec, total_net_bytes_per_sec);
    row.gpu_util_permille = agg.gpu_util_permille;
    row.gpu_dedicated_bytes = agg.gpu_dedicated_bytes;
    if row.leak_warning.is_none() {
        row.leak_warning = agg.leak_warning.clone();
    }
    row
}

fn compute_group_aggregate_from_parts(
    computed: &[ComputedProcess],
    leak_detector: &LeakDetector,
    mem_total_bytes: u64,
    root_idx: usize,
    children: &[usize],
    config: &ViewProjectionConfig,
    cold_cache: &ColdMetaCache,
) -> GroupAggregate {
    let root = &computed[root_idx];
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
    let mut rolled_leak = leak_detector.warning_for(
        root.key,
        &root.name,
        cold_cache
            .get_process(root.key)
            .and_then(|m| m.exe_path.as_deref()),
        config.show_leak_warnings,
        &config.leak_ignored_paths,
    );

    for &c_idx in children {
        let child = &computed[c_idx];
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
        if rolled_leak.is_none() {
            rolled_leak = leak_detector.warning_for(
                child.key,
                &child.name,
                cold_cache
                    .get_process(child.key)
                    .and_then(|m| m.exe_path.as_deref()),
                config.show_leak_warnings,
                &config.leak_ignored_paths,
            );
        }
    }

    if all_suspended {
        rolled_status |= ProcessStatusFlags::SUSPENDED;
    }

    let memory_permille = if mem_total_bytes > 0 {
        ((u128::from(sum_mem) * 1_000) / u128::from(mem_total_bytes)).min(1_000) as u16
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
        leak_warning: rolled_leak,
    }
}

fn resolve_webview2_host_group_from_parts(
    computed: &[ComputedProcess],
    latest_tree: &ProcessTree,
    idx: usize,
    cold_cache: &ColdMetaCache,
    name_to_groups: &HashGroupIndex,
    idx_to_app_group: &[Option<usize>],
) -> Option<usize> {
    let proc = &computed[idx];
    let session = proc.session_id;

    // 1. Check `--webview-exe-name=` on this process or any `msedgewebview2.exe` ancestor.
    let mut cur_opt = Some(idx);
    let mut hops = 0usize;
    while let Some(cur) = cur_opt {
        if hops >= 16 {
            break;
        }
        hops += 1;
        let cur_proc = &computed[cur];
        if cur_proc.session_id != session {
            break;
        }
        if is_webview2_image(&cur_proc.name) {
            let host_exe_opt = cold_cache.webview2_host_exe(cur_proc.key).or_else(|| {
                let meta = cold_cache.get_process(cur_proc.key)?;
                if meta.exe_path.is_some() {
                    return None;
                }
                classify_webview2(&cur_proc.name, meta.command_line.as_deref())
                    .and_then(|parsed| parsed.host_exe_name)
            });
            if let Some(host_exe) = host_exe_opt {
                let h = hash_session_ascii_ci(session, host_exe);
                if let Some(g_idx) = name_to_groups.find(h, |_g_idx, sample_idx| {
                    let s = &computed[sample_idx];
                    s.session_id == session && s.name.eq_ignore_ascii_case(host_exe)
                }) {
                    return Some(g_idx);
                }
            }
        }
        cur_opt = latest_tree.parent_index(cur);
    }

    // 2. Walk verified `ProcessKey` ancestor chain looking for any owning `App` group.
    let mut cur_opt = latest_tree.parent_index(idx);
    let mut hops = 0usize;
    while let Some(anc) = cur_opt {
        if hops >= 32 {
            break;
        }
        hops += 1;
        let anc_proc = &computed[anc];
        if anc_proc.session_id != session || is_windows_shell_launcher(&anc_proc.name) {
            break;
        }
        if let Some(g_idx) = idx_to_app_group[anc] {
            return Some(g_idx);
        }
        cur_opt = latest_tree.parent_index(anc);
    }

    None
}

fn collect_matching_with_ancestors_into<F>(
    computed: &[ComputedProcess],
    latest_tree: &ProcessTree,
    visible_mask: &[bool],
    included: &mut Vec<bool>,
    predicate: F,
) where
    F: Fn(&ComputedProcess) -> bool,
{
    included.clear();
    included.resize(computed.len(), false);
    for (idx, proc) in computed.iter().enumerate() {
        if !visible_mask[idx] {
            continue;
        }
        if predicate(proc) {
            let mut curr = idx;
            while !included[curr] {
                if visible_mask[curr] {
                    included[curr] = true;
                }
                if let Some(p_idx) = latest_tree.parent_index(curr) {
                    curr = p_idx;
                } else {
                    break;
                }
            }
        }
    }
}

fn matches_query_with_ports(
    proc: &ComputedProcess,
    cold_cache: &ColdMetaCache,
    parsed_query: &ParsedQuery,
    listening_ports: &HashSet<(u32, u16)>,
    include_user_in_text: bool,
) -> bool {
    let cold = cold_cache.get_process(proc.key);
    let asset = cold.and_then(|m| m.asset.as_ref());
    let display_name = process_display_name_ref(proc, cold_cache);
    let status = effective_status(proc, cold_cache);
    let ports_opt = (!listening_ports.is_empty()).then_some(listening_ports);

    let ctx = QueryMatchContext {
        pid: proc.key.pid,
        name: &proc.name,
        display_name,
        window_title: proc.window_title.as_deref(),
        publisher: asset.and_then(|a| a.publisher.as_deref()),
        exe_path: cold.and_then(|m| m.exe_path.as_deref()),
        command_line: cold.and_then(|m| m.command_line.as_deref()),
        user_name: cold.and_then(|m| m.user_name.as_deref()),
        include_user_in_text,
        status,
        cpu_permille: proc.cpu_permille,
        memory_bytes: proc.private_working_set_bytes,
        listening_ports: ports_opt,
    };
    parsed_query.matches(&ctx)
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn compare_process_items(
    ia: usize,
    ib: usize,
    computed: &[ComputedProcess],
    agg_a: Option<&GroupAggregate>,
    agg_b: Option<&GroupAggregate>,
    cold_cache: &ColdMetaCache,
    column: ProcessSortColumn,
    direction: SortDirection,
    frozen_ranks: Option<&HashMap<ProcessKey, usize, FastBuildHasher>>,
    deadband_ctx: Option<DeadbandContext<'_>>,
) -> Ordering {
    let a = &computed[ia];
    let b = &computed[ib];
    if let Some(ranks) = frozen_ranks {
        let ra = ranks.get(&a.key).copied().unwrap_or(usize::MAX);
        let rb = ranks.get(&b.key).copied().unwrap_or(usize::MAX);
        let frozen_ord = ra.cmp(&rb);
        if frozen_ord != Ordering::Equal {
            return frozen_ord;
        }
    }
    let ord = cmp_process_sort_column(ia, ib, a, b, agg_a, agg_b, cold_cache, column, deadband_ctx);
    let directed = match direction {
        SortDirection::Ascending => ord,
        SortDirection::Descending => ord.reverse(),
    };
    directed
        .then_with(|| {
            if let Some(db) = deadband_ctx {
                db.dense_prev[ia]
                    .prev_rank
                    .cmp(&db.dense_prev[ib].prev_rank)
            } else {
                Ordering::Equal
            }
        })
        .then_with(|| cmp_str_ascii_ci(&a.name, &b.name))
        .then_with(|| a.key.cmp(&b.key))
}

#[allow(clippy::too_many_arguments)]
fn sort_process_indices_with_aggregates_slice(
    computed: &[ComputedProcess],
    indices: &mut [usize],
    group_agg: &[Option<GroupAggregate>],
    cold_cache: &ColdMetaCache,
    column: ProcessSortColumn,
    direction: SortDirection,
    frozen_ranks: Option<&HashMap<ProcessKey, usize, FastBuildHasher>>,
    deadband_ctx: Option<DeadbandContext<'_>>,
) {
    indices.sort_by(|&ia, &ib| {
        compare_process_items(
            ia,
            ib,
            computed,
            group_agg[ia].as_ref(),
            group_agg[ib].as_ref(),
            cold_cache,
            column,
            direction,
            frozen_ranks,
            deadband_ctx,
        )
    });
}

#[allow(clippy::too_many_arguments)]
fn sort_process_indices_slice(
    computed: &[ComputedProcess],
    indices: &mut [usize],
    cold_cache: &ColdMetaCache,
    column: ProcessSortColumn,
    direction: SortDirection,
    frozen_ranks: Option<&HashMap<ProcessKey, usize, FastBuildHasher>>,
    deadband_ctx: Option<DeadbandContext<'_>>,
) {
    indices.sort_by(|&ia, &ib| {
        compare_process_items(
            ia,
            ib,
            computed,
            None,
            None,
            cold_cache,
            column,
            direction,
            frozen_ranks,
            deadband_ctx,
        )
    });
}

#[allow(clippy::too_many_arguments)]
fn sort_details_indices_slice(
    computed: &[ComputedProcess],
    indices: &mut [usize],
    cold_cache: &ColdMetaCache,
    column: DetailsSortColumn,
    direction: SortDirection,
    frozen_ranks: Option<&HashMap<ProcessKey, usize, FastBuildHasher>>,
    deadband_ctx: Option<DeadbandContext<'_>>,
) {
    indices.sort_by(|&ia, &ib| {
        let a = &computed[ia];
        let b = &computed[ib];
        if let Some(ranks) = frozen_ranks {
            let ra = ranks.get(&a.key).copied().unwrap_or(usize::MAX);
            let rb = ranks.get(&b.key).copied().unwrap_or(usize::MAX);
            let frozen_ord = ra.cmp(&rb);
            if frozen_ord != Ordering::Equal {
                return frozen_ord;
            }
        }
        let ord = match column {
            DetailsSortColumn::Name => cmp_str_ascii_ci(&a.name, &b.name),
            DetailsSortColumn::Pid => a.key.pid.cmp(&b.key.pid),
            DetailsSortColumn::Status => effective_status(a, cold_cache)
                .contains(ProcessStatusFlags::SUSPENDED)
                .cmp(&effective_status(b, cold_cache).contains(ProcessStatusFlags::SUSPENDED)),
            DetailsSortColumn::UserName => {
                let ua = cold_cache
                    .get_entry(a.key)
                    .and_then(|e| e.user_name_short.as_deref())
                    .unwrap_or("");
                let ub = cold_cache
                    .get_entry(b.key)
                    .and_then(|e| e.user_name_short.as_deref())
                    .unwrap_or("");
                cmp_str_ascii_ci(ua, ub)
            }
            DetailsSortColumn::SessionId => a.session_id.cmp(&b.session_id),
            DetailsSortColumn::Cpu => {
                if let Some(db) = deadband_ctx {
                    let sa = resolve_sticky_metric_dense(
                        db.dense_prev[ia],
                        u64::from(a.cpu_permille),
                        db.deadband,
                    );
                    let sb = resolve_sticky_metric_dense(
                        db.dense_prev[ib],
                        u64::from(b.cpu_permille),
                        db.deadband,
                    );
                    sa.cmp(&sb)
                } else {
                    a.cpu_permille.cmp(&b.cpu_permille)
                }
            }
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
                let aa = arch_rank(
                    cold_cache
                        .get_process(a.key)
                        .map_or(ProcessArch::Unknown, |m| m.arch),
                );
                let ab = arch_rank(
                    cold_cache
                        .get_process(b.key)
                        .map_or(ProcessArch::Unknown, |m| m.arch),
                );
                aa.cmp(&ab)
            }
            DetailsSortColumn::UacVirtualization => {
                let va = uac_rank(
                    cold_cache
                        .get_process(a.key)
                        .map_or(UacVirtualization::Unknown, |m| m.uac_virtualization),
                );
                let vb = uac_rank(
                    cold_cache
                        .get_process(b.key)
                        .map_or(UacVirtualization::Unknown, |m| m.uac_virtualization),
                );
                va.cmp(&vb)
            }
            DetailsSortColumn::Elevated => effective_status(a, cold_cache)
                .contains(ProcessStatusFlags::ELEVATED)
                .cmp(&effective_status(b, cold_cache).contains(ProcessStatusFlags::ELEVATED)),
            DetailsSortColumn::GpuMemory => a.gpu_dedicated_bytes.cmp(&b.gpu_dedicated_bytes),
            DetailsSortColumn::Publisher => {
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
            DetailsSortColumn::Description => {
                let da = cold_cache
                    .get_process(a.key)
                    .and_then(|m| m.asset.as_ref())
                    .and_then(|asset| asset.file_description.as_deref())
                    .unwrap_or("");
                let db = cold_cache
                    .get_process(b.key)
                    .and_then(|m| m.asset.as_ref())
                    .and_then(|asset| asset.file_description.as_deref())
                    .unwrap_or("");
                cmp_str_ascii_ci(da, db)
            }
            DetailsSortColumn::ExePath => {
                let ea = cold_cache
                    .get_process(a.key)
                    .and_then(|m| m.exe_path.as_deref())
                    .unwrap_or("");
                let eb = cold_cache
                    .get_process(b.key)
                    .and_then(|m| m.exe_path.as_deref())
                    .unwrap_or("");
                cmp_str_ascii_ci(ea, eb)
            }
            DetailsSortColumn::CommandLine => {
                let cmd_a = cold_cache
                    .get_process(a.key)
                    .and_then(|m| m.command_line.as_deref())
                    .unwrap_or("");
                let cmd_b = cold_cache
                    .get_process(b.key)
                    .and_then(|m| m.command_line.as_deref())
                    .unwrap_or("");
                cmp_str_ascii_ci(cmd_a, cmd_b)
            }
        };
        let directed = match direction {
            SortDirection::Ascending => ord,
            SortDirection::Descending => ord.reverse(),
        };
        directed
            .then_with(|| {
                if let Some(db) = deadband_ctx {
                    db.dense_prev[ia]
                        .prev_rank
                        .cmp(&db.dense_prev[ib].prev_rank)
                } else {
                    Ordering::Equal
                }
            })
            .then_with(|| cmp_str_ascii_ci(&a.name, &b.name))
            .then_with(|| a.key.cmp(&b.key))
    });
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn cmp_process_sort_column(
    ia: usize,
    ib: usize,
    a: &ComputedProcess,
    b: &ComputedProcess,
    agg_a: Option<&GroupAggregate>,
    agg_b: Option<&GroupAggregate>,
    cold_cache: &ColdMetaCache,
    column: ProcessSortColumn,
    deadband_ctx: Option<DeadbandContext<'_>>,
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
        ProcessSortColumn::Cpu => {
            let va = u64::from(agg_a.map_or(a.cpu_permille, |g| g.cpu_permille));
            let vb = u64::from(agg_b.map_or(b.cpu_permille, |g| g.cpu_permille));
            if let Some(db) = deadband_ctx {
                let sa = resolve_sticky_metric_dense(db.dense_prev[ia], va, db.deadband);
                let sb = resolve_sticky_metric_dense(db.dense_prev[ib], vb, db.deadband);
                sa.cmp(&sb)
            } else {
                va.cmp(&vb)
            }
        }
        ProcessSortColumn::Memory => agg_a
            .map_or(a.private_working_set_bytes, |g| g.memory_bytes)
            .cmp(&agg_b.map_or(b.private_working_set_bytes, |g| g.memory_bytes)),
        ProcessSortColumn::Disk => {
            let va = agg_a.map_or(a.disk_bytes_per_sec, |g| g.disk_bytes_per_sec);
            let vb = agg_b.map_or(b.disk_bytes_per_sec, |g| g.disk_bytes_per_sec);
            if let Some(db) = deadband_ctx {
                let sa = resolve_sticky_metric_dense(db.dense_prev[ia], va, db.deadband);
                let sb = resolve_sticky_metric_dense(db.dense_prev[ib], vb, db.deadband);
                sa.cmp(&sb)
            } else {
                va.cmp(&vb)
            }
        }
        ProcessSortColumn::Network => {
            let va = agg_a.map_or(a.net_bytes_per_sec, |g| g.net_bytes_per_sec);
            let vb = agg_b.map_or(b.net_bytes_per_sec, |g| g.net_bytes_per_sec);
            if let Some(db) = deadband_ctx {
                let sa = resolve_sticky_metric_dense(db.dense_prev[ia], va, db.deadband);
                let sb = resolve_sticky_metric_dense(db.dense_prev[ib], vb, db.deadband);
                sa.cmp(&sb)
            } else {
                va.cmp(&vb)
            }
        }
        ProcessSortColumn::Gpu => {
            let (ua, vram_a) = agg_a.map_or((a.gpu_util_permille, a.gpu_dedicated_bytes), |g| {
                (g.gpu_util_permille, g.gpu_dedicated_bytes)
            });
            let (ub, vram_b) = agg_b.map_or((b.gpu_util_permille, b.gpu_dedicated_bytes), |g| {
                (g.gpu_util_permille, g.gpu_dedicated_bytes)
            });
            if let Some(db) = deadband_ctx {
                let sa = resolve_sticky_metric_dense(db.dense_prev[ia], u64::from(ua), db.deadband);
                let sb = resolve_sticky_metric_dense(db.dense_prev[ib], u64::from(ub), db.deadband);
                (sa, vram_a).cmp(&(sb, vram_b))
            } else {
                (ua, vram_a).cmp(&(ub, vram_b))
            }
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

#[inline]
const fn service_state_rank(state: ServiceState) -> u8 {
    match state {
        ServiceState::Running => 7,
        ServiceState::StartPending => 6,
        ServiceState::ContinuePending => 5,
        ServiceState::Paused => 4,
        ServiceState::PausePending => 3,
        ServiceState::StopPending => 2,
        ServiceState::Stopped => 1,
        ServiceState::Unknown => 0,
    }
}

#[inline]
fn service_description_text(record: &ServiceRecord) -> &str {
    if let Some(desc) = record
        .description
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return desc;
    }
    let display = record.display_name.trim();
    if !display.is_empty() {
        return display;
    }
    "—"
}

#[inline]
fn u32_contains_substr(val: u32, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if !needle_lower.is_ascii() || needle_lower.len() > 10 {
        return false;
    }
    let mut buf = [0u8; 10];
    let mut n = val;
    let mut pos = buf.len();
    loop {
        pos -= 1;
        buf[pos] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    let digits = &buf[pos..];
    let needle = needle_lower.as_bytes();
    if needle.len() > digits.len() {
        return false;
    }
    digits.windows(needle.len()).any(|w| w == needle)
}

fn matches_service_filter(
    record: &ServiceRecord,
    filter_pid: Option<u32>,
    query_lower: &str,
    arena: &mut FoldedKeyArena,
) -> bool {
    if let Some(target_pid) = filter_pid
        && record.pid != Some(target_pid)
    {
        return false;
    }
    if query_lower.is_empty() {
        return true;
    }
    if arena.contains_icase(&record.name, query_lower)
        || arena.contains_icase(&record.display_name, query_lower)
    {
        return true;
    }
    if record
        .description
        .as_deref()
        .is_some_and(|d| arena.contains_icase(d, query_lower))
    {
        return true;
    }
    if record
        .group
        .as_deref()
        .is_some_and(|g| arena.contains_icase(g, query_lower))
    {
        return true;
    }
    record
        .pid
        .is_some_and(|pid| u32_contains_substr(pid, query_lower))
}

#[inline]
const fn startup_impact_rank(impact: StartupImpact) -> u8 {
    match impact {
        StartupImpact::High => 4,
        StartupImpact::Medium => 3,
        StartupImpact::Low => 2,
        StartupImpact::NotMeasured => 1,
        StartupImpact::None => 0,
    }
}

fn matches_startup_query(
    entry: &StartupEntry,
    query_lower: &str,
    arena: &mut FoldedKeyArena,
) -> bool {
    if query_lower.is_empty() {
        return true;
    }
    if arena.contains_icase(&entry.name, query_lower) {
        return true;
    }
    if entry
        .publisher
        .as_deref()
        .is_some_and(|p| arena.contains_icase(p, query_lower))
    {
        return true;
    }
    entry
        .command
        .as_deref()
        .is_some_and(|c| arena.contains_icase(c, query_lower))
}

#[inline]
const fn user_session_state_rank(state: UserSessionState) -> u8 {
    match state {
        UserSessionState::Active => 3,
        UserSessionState::Idle => 2,
        UserSessionState::Disconnected => 1,
        UserSessionState::Unknown => 0,
    }
}

#[inline]
fn user_child_sort_key(
    proc_item: &UserSessionProcess,
    sort_col: UsersSortColumn,
    tie_str: (u32, u32),
) -> (u64, (u32, u32)) {
    match sort_col {
        UsersSortColumn::User => (0, tie_str),
        UsersSortColumn::Status => (
            u64::from(!proc_item.status.contains(ProcessStatusFlags::SUSPENDED)),
            (0, 0),
        ),
        UsersSortColumn::Cpu => (u64::from(proc_item.cpu_permille), (0, 0)),
        UsersSortColumn::Memory => (proc_item.memory_bytes, (0, 0)),
        UsersSortColumn::Disk => (proc_item.disk_bytes_per_sec, (0, 0)),
        UsersSortColumn::Network => (proc_item.net_bytes_per_sec, (0, 0)),
    }
}

fn push_sorted_user_children(
    session: &UserSessionRecord,
    filter_indices: Option<&[usize]>,
    sort_col: UsersSortColumn,
    sort_dir: SortDirection,
    arena: &mut FoldedKeyArena,
    child_entries: &mut Vec<SchwartzEntry>,
    rows: &mut Vec<UsersRowProjection>,
) {
    child_entries.clear();
    if let Some(indices) = filter_indices {
        for &p_idx in indices {
            let proc_item = &session.processes[p_idx];
            let tie_str = arena.push_folded(&proc_item.display_name);
            let (numeric, primary_str) = user_child_sort_key(proc_item, sort_col, tie_str);
            child_entries.push(SchwartzEntry {
                index: p_idx,
                numeric,
                primary_str,
                tie_str,
            });
        }
    } else {
        for (p_idx, proc_item) in session.processes.iter().enumerate() {
            let tie_str = arena.push_folded(&proc_item.display_name);
            let (numeric, primary_str) = user_child_sort_key(proc_item, sort_col, tie_str);
            child_entries.push(SchwartzEntry {
                index: p_idx,
                numeric,
                primary_str,
                tie_str,
            });
        }
    }
    sort_and_emit_user_children(session, sort_col, sort_dir, arena, child_entries, rows);
}

fn sort_and_emit_user_children(
    session: &UserSessionRecord,
    sort_col: UsersSortColumn,
    sort_dir: SortDirection,
    arena: &FoldedKeyArena,
    child_entries: &mut [SchwartzEntry],
    rows: &mut Vec<UsersRowProjection>,
) {
    let is_str_col = matches!(sort_col, UsersSortColumn::User);
    child_entries.sort_by(|a, b| {
        let ord = if is_str_col {
            arena.slice(a.primary_str).cmp(arena.slice(b.primary_str))
        } else {
            a.numeric.cmp(&b.numeric)
        };
        let directed = match sort_dir {
            SortDirection::Ascending => ord,
            SortDirection::Descending => ord.reverse(),
        };
        directed
            .then_with(|| arena.slice(a.tie_str).cmp(arena.slice(b.tie_str)))
            .then_with(|| {
                session.processes[a.index]
                    .key
                    .cmp(&session.processes[b.index].key)
            })
    });
    for entry in child_entries.iter() {
        rows.push(UsersRowProjection::Process {
            session_id: session.session_id,
            process: session.processes[entry.index].clone(),
        });
    }
}

#[derive(Debug, Clone)]
struct GroupAggregate {
    cpu_permille: u16,
    memory_bytes: u64,
    memory_permille: u16,
    disk_bytes_per_sec: u64,
    net_bytes_per_sec: u64,
    gpu_util_permille: u16,
    gpu_dedicated_bytes: u64,
    status: ProcessStatusFlags,
    leak_warning: Option<LeakWarning>,
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

/// Interactive Windows shell tools that remain visible even when
/// `hide_windows_system_processes` is enabled.
const INTERACTIVE_SHELL_EXEMPTIONS: &[&str] = &[
    "explorer.exe",
    "cmd.exe",
    "conhost.exe",
    "openconsole.exe",
    "powershell.exe",
    "pwsh.exe",
    "wt.exe",
];

#[inline]
fn is_hidden_system_process(proc: &ComputedProcess, hide_system: bool) -> bool {
    if !hide_system || proc.category != ProcessCategory::System {
        return false;
    }
    !INTERACTIVE_SHELL_EXEMPTIONS
        .iter()
        .any(|exempt| proc.name.eq_ignore_ascii_case(exempt))
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

    #[test]
    fn windows_grouped_webview2_host_attribution_and_role_population() {
        use loom_types::{ProcessColdMeta, WebView2Role};

        let mut engine = ProjectionEngine::with_platform(PlatformProfile::WINDOWS);
        let mut scratch = ProcessSampleScratch::<()>::new();
        scratch.perf.logical_cpu_count = 4;

        // Host App: Teams.exe (PID 100) + two msedgewebview2.exe subprocesses (200 by flag, 201 by ancestor chain).
        make_detailed_sample(
            &mut scratch,
            100,
            10,
            1,
            1,
            "Teams.exe",
            ProcessCategory::App,
            ProcessStatusFlags::empty(),
            0,
            150 * 1024 * 1024,
            0,
        );
        make_detailed_sample(
            &mut scratch,
            200,
            20,
            999, // detached parent PID, matched via `--webview-exe-name=Teams.exe`
            1,
            "msedgewebview2.exe",
            ProcessCategory::Background,
            ProcessStatusFlags::empty(),
            0,
            80 * 1024 * 1024,
            0,
        );
        make_detailed_sample(
            &mut scratch,
            201,
            30,
            100, // child of Teams.exe, no `--webview-exe-name=` flag -> matched via ancestor chain
            1,
            "msedgewebview2.exe",
            ProcessCategory::Background,
            ProcessStatusFlags::empty(),
            0,
            60 * 1024 * 1024,
            0,
        );

        engine.ingest_process_sample(Timestamp100Ns::new(10_000_000), &scratch, 4);

        let mut cold = ColdMetaCache::new();
        cold.insert_process(ProcessColdMeta {
            key: ProcessKey::new(200, 20),
            exe_path: Some(Arc::from(r"C:\Program Files\WebView2\msedgewebview2.exe")),
            command_line: Some(Arc::from(
                r#"msedgewebview2.exe --type=renderer --webview-exe-name=Teams.exe"#,
            )),
            user_name: None,
            arch: ProcessArch::X64,
            uac_virtualization: UacVirtualization::NotAllowed,
            efficiency_mode: false,
            is_elevated: false,
            app_group_id: None,
            asset: None,
        });
        cold.insert_process(ProcessColdMeta {
            key: ProcessKey::new(201, 30),
            exe_path: Some(Arc::from(r"C:\Program Files\WebView2\msedgewebview2.exe")),
            command_line: Some(Arc::from(
                r#"msedgewebview2.exe --type=utility --utility-sub-type=network.mojom.NetworkService"#,
            )),
            user_name: None,
            arch: ProcessArch::X64,
            uac_virtualization: UacVirtualization::NotAllowed,
            efficiency_mode: false,
            is_elevated: false,
            app_group_id: None,
            asset: None,
        });

        let mut cfg = ViewProjectionConfig {
            process_view_mode: ProcessViewMode::Grouped,
            webview2_host_grouping: true,
            ..ViewProjectionConfig::default()
        };
        cfg.process_expanded.insert(ProcessKey::new(100, 10));

        let grouped = engine.project_processes(&cfg, &cold, 1);
        // Teams.exe absorbs both WebView2 processes -> total group count 3, zero Background section.
        assert_eq!(
            grouped.rows[0].kind,
            ProcessRowKind::SectionHeader {
                category: ProcessCategory::App,
                count: 1
            }
        );
        assert_eq!(grouped.rows[1].key, ProcessKey::new(100, 10));
        assert_eq!(grouped.rows[1].child_count, 3);
        assert_eq!(grouped.rows[1].memory_bytes, 290 * 1024 * 1024);
        assert_eq!(grouped.rows.len(), 4);

        let wv_renderer = grouped
            .rows
            .iter()
            .find(|r| r.key == ProcessKey::new(200, 20))
            .unwrap();
        assert_eq!(wv_renderer.depth, 1);
        assert_eq!(wv_renderer.webview2_role, Some(WebView2Role::Renderer));

        let wv_net = grouped
            .rows
            .iter()
            .find(|r| r.key == ProcessKey::new(201, 30))
            .unwrap();
        assert_eq!(wv_net.depth, 1);
        assert_eq!(wv_net.webview2_role, Some(WebView2Role::Network));
        assert_eq!(wv_net.parent_key, Some(ProcessKey::new(100, 10)));

        // Disabling `webview2_host_grouping` moves WebView2 processes back to Background
        // while still populating `webview2_role`.
        cfg.webview2_host_grouping = false;
        let ungrouped = engine.project_processes(&cfg, &cold, 2);
        let teams = ungrouped
            .rows
            .iter()
            .find(|r| r.key == ProcessKey::new(100, 10))
            .unwrap();
        assert_eq!(teams.child_count, 0);
        assert!(ungrouped.rows.iter().any(|r| matches!(
            r.kind,
            ProcessRowKind::SectionHeader {
                category: ProcessCategory::Background,
                ..
            }
        )));
        let details = engine.project_details(&cfg, &cold, 1);
        assert_eq!(
            details
                .rows
                .iter()
                .find(|r| r.key == ProcessKey::new(200, 20))
                .unwrap()
                .webview2_role,
            Some(WebView2Role::Renderer)
        );
    }

    #[test]
    fn hide_windows_system_processes_filters_core_os_but_keeps_interactive_shells() {
        let mut engine = ProjectionEngine::with_platform(PlatformProfile::WINDOWS);
        let mut scratch = ProcessSampleScratch::<()>::new();
        scratch.perf.logical_cpu_count = 4;

        for (pid, name, cat) in [
            (4, "System", ProcessCategory::System),
            (100, "svchost.exe", ProcessCategory::System),
            (101, "csrss.exe", ProcessCategory::System),
            (102, "explorer.exe", ProcessCategory::System),
            (103, "cmd.exe", ProcessCategory::System),
            (104, "pwsh.exe", ProcessCategory::System),
            (105, "wt.exe", ProcessCategory::System),
            (200, "myapp.exe", ProcessCategory::App),
        ] {
            make_sample(&mut scratch, pid, 10, 4, name, cat, 0, 0);
        }
        engine.ingest_process_sample(Timestamp100Ns::new(10_000_000), &scratch, 4);

        let cold = ColdMetaCache::new();
        let cfg = ViewProjectionConfig {
            hide_windows_system_processes: true,
            ..ViewProjectionConfig::default()
        };
        let slice = engine.project_processes(&cfg, &cold, 1);
        let visible_names: Vec<&str> = slice
            .rows
            .iter()
            .filter(|r| r.kind == ProcessRowKind::Process)
            .map(|r| r.name.as_ref())
            .collect();
        assert!(visible_names.contains(&"myapp.exe"));
        assert!(visible_names.contains(&"explorer.exe"));
        assert!(visible_names.contains(&"cmd.exe"));
        assert!(visible_names.contains(&"pwsh.exe"));
        assert!(visible_names.contains(&"wt.exe"));
        assert!(!visible_names.contains(&"System"));
        assert!(!visible_names.contains(&"svchost.exe"));
        assert!(!visible_names.contains(&"csrss.exe"));

        // In Tree mode, visible children whose hidden parent (PID 4) is filtered out are promoted to roots.
        let tree_cfg = ViewProjectionConfig {
            process_view_mode: ProcessViewMode::Tree,
            hide_windows_system_processes: true,
            ..ViewProjectionConfig::default()
        };
        let tree_slice = engine.project_processes(&tree_cfg, &cold, 2);
        assert!(
            tree_slice
                .rows
                .iter()
                .any(|r| r.name.as_ref() == "myapp.exe" && r.depth == 0)
        );
        assert!(
            !tree_slice
                .rows
                .iter()
                .any(|r| r.name.as_ref() == "svchost.exe")
        );

        let details = engine.project_details(&cfg, &cold, 1);
        assert!(
            !details
                .rows
                .iter()
                .any(|r| r.name.as_ref() == "svchost.exe")
        );
        assert!(details.rows.iter().any(|r| r.name.as_ref() == "pwsh.exe"));
    }

    #[test]
    fn order_freeze_locks_row_order_while_updating_metrics_and_pruning_exited_rows() {
        let mut engine = ProjectionEngine::new();
        let mut scratch = ProcessSampleScratch::<()>::new();
        scratch.perf.logical_cpu_count = 4;

        let k_a = ProcessKey::new(10, 100);
        let k_b = ProcessKey::new(20, 100);
        let k_c = ProcessKey::new(30, 100);
        make_detailed_sample(
            &mut scratch,
            10,
            100,
            0,
            1,
            "alpha.exe",
            ProcessCategory::Background,
            ProcessStatusFlags::empty(),
            0,
            300 * 1024 * 1024,
            0,
        );
        make_detailed_sample(
            &mut scratch,
            20,
            100,
            0,
            1,
            "beta.exe",
            ProcessCategory::Background,
            ProcessStatusFlags::empty(),
            0,
            200 * 1024 * 1024,
            0,
        );
        make_detailed_sample(
            &mut scratch,
            30,
            100,
            0,
            1,
            "gamma.exe",
            ProcessCategory::Background,
            ProcessStatusFlags::empty(),
            0,
            100 * 1024 * 1024,
            0,
        );
        engine.ingest_process_sample(Timestamp100Ns::new(10_000_000), &scratch, 4);

        let cold = ColdMetaCache::new();
        let mut cfg = ViewProjectionConfig {
            process_sort_column: ProcessSortColumn::Memory,
            process_sort_direction: SortDirection::Descending,
            ..ViewProjectionConfig::default()
        };

        let s1 = engine.project_processes(&cfg, &cold, 1);
        let order1: Vec<ProcessKey> = s1
            .rows
            .iter()
            .filter(|r| r.kind == ProcessRowKind::Process)
            .map(|r| r.key)
            .collect();
        assert_eq!(order1, vec![k_a, k_b, k_c]);

        // Freeze row order, then reverse memory usage (gamma > alpha) and remove exited beta.
        cfg.freeze_process_order(&s1.rows);
        scratch.clear();
        make_detailed_sample(
            &mut scratch,
            10,
            100,
            0,
            1,
            "alpha.exe",
            ProcessCategory::Background,
            ProcessStatusFlags::empty(),
            0,
            50 * 1024 * 1024,
            0,
        );
        make_detailed_sample(
            &mut scratch,
            30,
            100,
            0,
            1,
            "gamma.exe",
            ProcessCategory::Background,
            ProcessStatusFlags::empty(),
            0,
            900 * 1024 * 1024,
            0,
        );
        engine.ingest_process_sample(Timestamp100Ns::new(20_000_000), &scratch, 4);

        let s2 = engine.project_processes(&cfg, &cold, 2);
        let rows2: Vec<(ProcessKey, u64)> = s2
            .rows
            .iter()
            .filter(|r| r.kind == ProcessRowKind::Process)
            .map(|r| (r.key, r.memory_bytes))
            .collect();
        // Order stays alpha -> gamma (beta removed cleanly) while live memory updates in place!
        assert_eq!(
            rows2,
            vec![(k_a, 50 * 1024 * 1024), (k_c, 900 * 1024 * 1024)]
        );

        // Unfreezing immediately re-sorts gamma ahead of alpha.
        cfg.process_order_frozen = false;
        cfg.frozen_process_order.clear();
        let s3 = engine.project_processes(&cfg, &cold, 3);
        let order3: Vec<ProcessKey> = s3
            .rows
            .iter()
            .filter(|r| r.kind == ProcessRowKind::Process)
            .map(|r| r.key)
            .collect();
        assert_eq!(order3, vec![k_c, k_a]);
    }

    #[test]
    fn sort_deadband_suppresses_small_jitter_reorders_on_threshold_and_resets_on_config_change() {
        let mut engine = ProjectionEngine::new();
        let mut scratch = ProcessSampleScratch::<()>::new();
        let cold = ColdMetaCache::new();
        let k_a = ProcessKey::new(10, 100);
        let k_b = ProcessKey::new(20, 100);

        // 1s tick (10_000_000 ticks), 1 logical CPU => 10_000 ticks = 1 permille (0.1%).
        let mut cpu_a: u64 = 0;
        let mut cpu_b: u64 = 0;
        let mut disk_a: u64 = 0;
        let mut disk_b: u64 = 0;
        let mut ts: u64 = 100_000_000;

        let mut ingest = |eng: &mut ProjectionEngine,
                          da_permille: u64,
                          db_permille: u64,
                          da_disk: u64,
                          db_disk: u64| {
            cpu_a += da_permille * 10_000;
            cpu_b += db_permille * 10_000;
            disk_a += da_disk;
            disk_b += db_disk;
            ts += 10_000_000;
            scratch.clear();
            scratch.perf.logical_cpu_count = 1;
            make_detailed_sample(
                &mut scratch,
                10,
                100,
                0,
                1,
                "alpha.exe",
                ProcessCategory::Background,
                ProcessStatusFlags::empty(),
                cpu_a,
                100 * 1024 * 1024,
                disk_a,
            );
            make_detailed_sample(
                &mut scratch,
                20,
                100,
                0,
                1,
                "beta.exe",
                ProcessCategory::Background,
                ProcessStatusFlags::empty(),
                cpu_b,
                100 * 1024 * 1024,
                disk_b,
            );
            eng.ingest_process_sample(Timestamp100Ns::new(ts), &scratch, 1);
        };

        // Tick 0 (baseline).
        ingest(&mut engine, 0, 0, 0, 0);
        let mut cfg = ViewProjectionConfig {
            process_sort_column: ProcessSortColumn::Cpu,
            process_sort_direction: SortDirection::Descending,
            details_sort_column: DetailsSortColumn::Cpu,
            details_sort_direction: SortDirection::Descending,
            ..ViewProjectionConfig::default()
        };
        assert!(cfg.sort_deadband_enabled);

        // Tick 1: both at 10 permille (1.0%) -> alphabetical tie-break [A, B].
        ingest(&mut engine, 10, 10, 0, 0);
        let s1 = engine.project_processes(&cfg, &cold, 1);
        let p_order = |s: &ProcessTableSlice| -> Vec<ProcessKey> {
            s.rows
                .iter()
                .filter(|r| r.kind == ProcessRowKind::Process)
                .map(|r| r.key)
                .collect()
        };
        assert_eq!(p_order(&s1), vec![k_a, k_b]);
        let d1 = engine.project_details(&cfg, &cold, 1);
        assert_eq!(
            d1.rows.iter().map(|r| r.key).collect::<Vec<_>>(),
            vec![k_a, k_b]
        );

        // Tick 2: B rises by 0.1% (11 vs 10 permille, abs_diff = 1 < 2) -> order MUST stay [A, B].
        ingest(&mut engine, 10, 11, 0, 0);
        let s2 = engine.project_processes(&cfg, &cold, 2);
        assert_eq!(p_order(&s2), vec![k_a, k_b]);
        let d2 = engine.project_details(&cfg, &cold, 2);
        assert_eq!(
            d2.rows.iter().map(|r| r.key).collect::<Vec<_>>(),
            vec![k_a, k_b]
        );

        // Tick 3: B jumps to 12 permille (abs_diff(12, 10) == 2 >= CPU_SORT_DEADBAND_PERMILLE) -> MUST reorder [B, A].
        ingest(&mut engine, 10, 12, 0, 0);
        let s3 = engine.project_processes(&cfg, &cold, 3);
        assert_eq!(p_order(&s3), vec![k_b, k_a]);
        let d3 = engine.project_details(&cfg, &cold, 3);
        assert_eq!(
            d3.rows.iter().map(|r| r.key).collect::<Vec<_>>(),
            vec![k_b, k_a]
        );

        // Tick 4: Small flip back (A=11, B=11 -> abs_diff from sticky (10, 12) is 1 < 2) -> stays [B, A].
        ingest(&mut engine, 11, 11, 0, 0);
        let s4 = engine.project_processes(&cfg, &cold, 4);
        assert_eq!(p_order(&s4), vec![k_b, k_a]);

        // Changing sort direction resets deadband immediately -> live tie (11, 11) breaks alphabetically [A, B].
        cfg.process_sort_direction = SortDirection::Ascending;
        let s5 = engine.project_processes(&cfg, &cold, 5);
        assert_eq!(p_order(&s5), vec![k_a, k_b]);

        // Test Disk rate deadband (16 KiB/s): 100 KiB/s vs 108 KiB/s stays stable; >= 16 KiB/s reorders.
        cfg.process_sort_column = ProcessSortColumn::Disk;
        cfg.process_sort_direction = SortDirection::Descending;
        ingest(&mut engine, 0, 0, 100 * 1024, 100 * 1024);
        let sd1 = engine.project_processes(&cfg, &cold, 6);
        assert_eq!(p_order(&sd1), vec![k_a, k_b]);

        ingest(&mut engine, 0, 0, 100 * 1024, 108 * 1024);
        let sd2 = engine.project_processes(&cfg, &cold, 7);
        assert_eq!(p_order(&sd2), vec![k_a, k_b]);

        ingest(&mut engine, 0, 0, 100 * 1024, 116 * 1024);
        let sd3 = engine.project_processes(&cfg, &cold, 8);
        assert_eq!(p_order(&sd3), vec![k_b, k_a]);

        // Disabling sort_deadband_enabled immediately sorts strictly by live metrics.
        cfg.sort_deadband_enabled = false;
        ingest(&mut engine, 0, 0, 101 * 1024, 100 * 1024);
        let sd4 = engine.project_processes(&cfg, &cold, 9);
        assert_eq!(p_order(&sd4), vec![k_a, k_b]);
    }

    #[test]
    fn sort_deadband_randomized_stress_maintains_strict_weak_ordering() {
        let mut engine = ProjectionEngine::with_platform(PlatformProfile::WINDOWS);
        let mut scratch = ProcessSampleScratch::<()>::with_capacity(120, 4096);
        let cold = ColdMetaCache::new();
        let mut rng: u64 = 0x9e37_79b9_7f4a_7c15;
        let next_u64 = |state: &mut u64| -> u64 {
            let mut x = *state;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *state = x;
            x
        };

        let mut acc_cpu = vec![0u64; 120];
        let mut acc_disk = vec![0u64; 120];
        let mut cfg = ViewProjectionConfig {
            process_sort_column: ProcessSortColumn::Cpu,
            process_sort_direction: SortDirection::Descending,
            details_sort_column: DetailsSortColumn::Cpu,
            details_sort_direction: SortDirection::Descending,
            sort_deadband_enabled: true,
            ..ViewProjectionConfig::default()
        };

        for tick in 1..=80u64 {
            scratch.clear();
            scratch.perf.logical_cpu_count = 4;
            scratch.perf.mem_total_bytes = 16 * 1024 * 1024 * 1024;
            for i in 0..120usize {
                let jitter_permille = next_u64(&mut rng) % 6; // 0..=5 permille around deadband boundary
                let jitter_disk = (next_u64(&mut rng) % 40) * 1024; // 0..40 KiB/s
                acc_cpu[i] += jitter_permille * 40_000;
                acc_disk[i] += jitter_disk;
                let cat = match i % 3 {
                    0 => ProcessCategory::App,
                    1 => ProcessCategory::Background,
                    _ => ProcessCategory::System,
                };
                let name = match i % 6 {
                    0 => "app_a.exe",
                    1 => "app_b.exe",
                    2 => "worker.exe",
                    3 => "svc.exe",
                    4 => "helper.exe",
                    _ => "daemon.exe",
                };
                make_detailed_sample(
                    &mut scratch,
                    100 + i as u32,
                    1_000 + i as u64,
                    0,
                    1,
                    name,
                    cat,
                    ProcessStatusFlags::empty(),
                    acc_cpu[i],
                    (i as u64 + 1) * 1024 * 1024,
                    acc_disk[i],
                );
            }
            engine.ingest_process_sample(
                Timestamp100Ns::new(100_000_000 + tick * 10_000_000),
                &scratch,
                4,
            );

            // Cycle across deadband columns and view modes to exercise every comparator path.
            cfg.process_sort_column = match tick % 4 {
                0 => ProcessSortColumn::Cpu,
                1 => ProcessSortColumn::Disk,
                2 => ProcessSortColumn::Network,
                _ => ProcessSortColumn::Gpu,
            };
            cfg.process_view_mode = if tick % 2 == 0 {
                ProcessViewMode::Grouped
            } else {
                ProcessViewMode::Tree
            };
            let ps = engine.project_processes(&cfg, &cold, tick);
            assert_eq!(ps.total_process_count, 120);

            cfg.details_view_mode = if tick % 2 == 0 {
                DetailsViewMode::Flat
            } else {
                DetailsViewMode::Tree
            };
            let ds = engine.project_details(&cfg, &cold, tick);
            assert_eq!(ds.rows.len(), 120);
        }
    }

    #[test]
    fn offloaded_services_startup_users_and_app_history_cjk_schwartzian_filtering_and_sorting() {
        let mut engine = ProjectionEngine::new();
        let cold = ColdMetaCache::new();
        let mut cfg = ViewProjectionConfig::default();

        // 1. Services: CJK descriptions, PID filter, and all sort columns.
        let services = vec![
            ServiceRecord {
                name: Arc::from("Spooler"),
                display_name: Arc::from("Print Spooler"),
                description: Some(Arc::from("打印后台处理程序服务")),
                pid: Some(2040),
                state: ServiceState::Running,
                group: Some(Arc::from("SpoolerGroup")),
            },
            ServiceRecord {
                name: Arc::from("Dhcp"),
                display_name: Arc::from("DHCP 客户端"),
                description: None,
                pid: Some(1200),
                state: ServiceState::Running,
                group: Some(Arc::from("LocalServiceNetworkRestricted")),
            },
            ServiceRecord {
                name: Arc::from("wuauserv"),
                display_name: Arc::from("Windows Update"),
                description: Some(Arc::from("启用 Windows 更新的检测、下载和安装")),
                pid: None,
                state: ServiceState::Stopped,
                group: Some(Arc::from("netsvcs")),
            },
        ];

        cfg.services_sort_column = ServicesSortColumn::Name;
        cfg.services_sort_direction = SortDirection::Ascending;
        let svc_name = engine.project_services(&services, &cfg, 1);
        assert_eq!(svc_name.visible_indices, vec![1, 0, 2]);

        cfg.services_sort_column = ServicesSortColumn::Pid;
        cfg.services_sort_direction = SortDirection::Descending;
        let svc_pid = engine.project_services(&services, &cfg, 2);
        assert_eq!(svc_pid.visible_indices, vec![0, 1, 2]);

        cfg.services_sort_column = ServicesSortColumn::Description;
        cfg.services_sort_direction = SortDirection::Ascending;
        let svc_desc = engine.project_services(&services, &cfg, 3);
        assert_eq!(svc_desc.visible_indices.len(), 3);
        // ASCII "DHCP..." sorts before CJK "启用..." and "打印..."
        assert_eq!(svc_desc.visible_indices[0], 1);

        // CJK search query matches Chinese description.
        cfg.search_query = "打印后台".to_owned();
        let svc_cjk = engine.project_services(&services, &cfg, 4);
        assert_eq!(svc_cjk.visible_indices, vec![0]);

        // Strict PID filter overrides non-matching services.
        cfg.search_query.clear();
        cfg.services_filter_pid = Some(1200);
        let svc_pid_filter = engine.project_services(&services, &cfg, 5);
        assert_eq!(svc_pid_filter.visible_indices, vec![1]);
        cfg.services_filter_pid = None;

        // 2. Startup: CJK + publisher/command search + Impact/Status sorting.
        let startup = vec![
            StartupEntry {
                id: Arc::from("hkcu:wechat"),
                name: Arc::from("微信 WeChat"),
                publisher: Some(Arc::from("Tencent")),
                command: Some(Arc::from(r"C:\Tencent\WeChat.exe --autorun")),
                file_path: None,
                enabled: true,
                impact: StartupImpact::High,
            },
            StartupEntry {
                id: Arc::from("hkcu:discord"),
                name: Arc::from("Discord"),
                publisher: Some(Arc::from("Discord Inc.")),
                command: Some(Arc::from(r"C:\Discord\Update.exe --processStart")),
                file_path: None,
                enabled: false,
                impact: StartupImpact::Low,
            },
            StartupEntry {
                id: Arc::from("hklm:security"),
                name: Arc::from("安全中心"),
                publisher: Some(Arc::from("Microsoft Corporation")),
                command: None,
                file_path: None,
                enabled: true,
                impact: StartupImpact::NotMeasured,
            },
        ];

        cfg.startup_sort_column = StartupSortColumn::Impact;
        cfg.startup_sort_direction = SortDirection::Descending;
        let st_impact = engine.project_startup(Some(4_200), &startup, &cfg, 1);
        assert_eq!(st_impact.visible_indices, vec![0, 1, 2]);
        assert_eq!(st_impact.last_bios_time_ms, Some(4_200));

        cfg.search_query = "微信".to_owned();
        let st_cjk = engine.project_startup(Some(4_200), &startup, &cfg, 2);
        assert_eq!(st_cjk.visible_indices, vec![0]);

        // 3. Users: session sorting, explicit expansion, and search auto-expand.
        let sessions = vec![
            UserSessionRecord {
                session_id: 1,
                user_name: Arc::from("Alice"),
                state: UserSessionState::Active,
                process_count: 2,
                cpu_permille: 120,
                memory_bytes: 512 * 1024 * 1024,
                disk_bytes_per_sec: 1024,
                net_bytes_per_sec: 2048,
                processes: vec![
                    UserSessionProcess {
                        key: ProcessKey::new(101, 100),
                        name: Arc::from("code.exe"),
                        display_name: Arc::from("Visual Studio Code"),
                        status: ProcessStatusFlags::empty(),
                        cpu_permille: 100,
                        memory_bytes: 400 * 1024 * 1024,
                        disk_bytes_per_sec: 0,
                        net_bytes_per_sec: 0,
                    },
                    UserSessionProcess {
                        key: ProcessKey::new(102, 200),
                        name: Arc::from("notepad.exe"),
                        display_name: Arc::from("记事本"),
                        status: ProcessStatusFlags::empty(),
                        cpu_permille: 20,
                        memory_bytes: 112 * 1024 * 1024,
                        disk_bytes_per_sec: 0,
                        net_bytes_per_sec: 0,
                    },
                ],
            },
            UserSessionRecord {
                session_id: 2,
                user_name: Arc::from("Bob"),
                state: UserSessionState::Disconnected,
                process_count: 1,
                cpu_permille: 10,
                memory_bytes: 64 * 1024 * 1024,
                disk_bytes_per_sec: 0,
                net_bytes_per_sec: 0,
                processes: vec![UserSessionProcess {
                    key: ProcessKey::new(201, 300),
                    name: Arc::from("cmd.exe"),
                    display_name: Arc::from("cmd.exe"),
                    status: ProcessStatusFlags::empty(),
                    cpu_permille: 10,
                    memory_bytes: 64 * 1024 * 1024,
                    disk_bytes_per_sec: 0,
                    net_bytes_per_sec: 0,
                }],
            },
        ];

        cfg.search_query.clear();
        cfg.users_sort_column = UsersSortColumn::Cpu;
        cfg.users_sort_direction = SortDirection::Descending;
        let users_collapsed = engine.project_users(&sessions, &cfg, &cold, 1);
        assert_eq!(users_collapsed.rows.len(), 2);
        assert!(matches!(
            users_collapsed.rows[0],
            UsersRowProjection::Session {
                session_id: 1,
                is_expanded: false,
                has_children: true,
                ..
            }
        ));

        cfg.users_expanded.insert(1);
        let users_expanded = engine.project_users(&sessions, &cfg, &cold, 2);
        assert_eq!(users_expanded.rows.len(), 4);
        assert!(matches!(
            &users_expanded.rows[1],
            UsersRowProjection::Process { process, .. } if process.key.pid == 101
        ));
        assert!(matches!(
            &users_expanded.rows[2],
            UsersRowProjection::Process { process, .. } if process.key.pid == 102
        ));

        // Search for CJK child display name auto-expands collapsed session 1.
        cfg.users_expanded.clear();
        cfg.search_query = "记事本".to_owned();
        let users_auto = engine.project_users(&sessions, &cfg, &cold, 3);
        assert_eq!(users_auto.rows.len(), 2);
        assert!(matches!(
            users_auto.rows[0],
            UsersRowProjection::Session {
                session_id: 1,
                is_expanded: true,
                ..
            }
        ));
        assert!(matches!(
            &users_auto.rows[1],
            UsersRowProjection::Process { process, .. } if process.key.pid == 102
        ));

        // 4. AppHistory: CJK filtering and column sorting.
        let history = vec![
            AppHistoryRecord {
                app_id: Arc::from("app.alpha"),
                name: Arc::from("浏览器"),
                cpu_time_100ns: 1_200_000_000,
                network_bytes: 10_000,
                metered_network_bytes: 500,
            },
            AppHistoryRecord {
                app_id: Arc::from("app.beta"),
                name: Arc::from("Beta Studio"),
                cpu_time_100ns: 36_000_000_000,
                network_bytes: 500_000,
                metered_network_bytes: 10_000,
            },
            AppHistoryRecord {
                app_id: Arc::from("app.gamma"),
                name: Arc::from("下载工具"),
                cpu_time_100ns: 300_000_000,
                network_bytes: 2_000_000,
                metered_network_bytes: 0,
            },
        ];

        cfg.search_query.clear();
        cfg.app_history_sort_column = AppHistorySortColumn::Network;
        cfg.app_history_sort_direction = SortDirection::Descending;
        let hist_net = engine.project_app_history(Some(1_700_000_000), &history, &cfg, 1);
        assert_eq!(
            hist_net
                .visible_indices
                .iter()
                .map(|&i| hist_net.items[i].app_id.as_ref())
                .collect::<Vec<_>>(),
            vec!["app.gamma", "app.beta", "app.alpha"]
        );

        cfg.search_query = "下载".to_owned();
        let hist_cjk = engine.project_app_history(Some(1_700_000_000), &history, &cfg, 2);
        assert_eq!(hist_cjk.visible_indices.len(), 1);
        assert_eq!(
            hist_cjk.items[hist_cjk.visible_indices[0]].app_id.as_ref(),
            "app.gamma"
        );
    }

    #[test]
    fn stress_1000_processes_and_500_cjk_services_projection_is_fast_and_deterministic() {
        let mut engine = ProjectionEngine::with_platform(PlatformProfile::WINDOWS);
        let mut scratch = ProcessSampleScratch::<()>::with_capacity(1_000, 32 * 1024);
        let cold = ColdMetaCache::new();

        let fill_1000 = |s: &mut ProcessSampleScratch<()>, tick: u64| {
            s.clear();
            s.perf.logical_cpu_count = 16;
            s.perf.mem_total_bytes = 32 * 1024 * 1024 * 1024;
            for i in 1..=1_000u32 {
                let (cat, name) = match i % 5 {
                    0 => (ProcessCategory::App, "chrome.exe"),
                    1 => (ProcessCategory::Background, "chrome.exe"),
                    2 => (ProcessCategory::Background, "code.exe"),
                    3 => (ProcessCategory::System, "svchost.exe"),
                    _ => (ProcessCategory::Background, "worker.exe"),
                };
                make_detailed_sample(
                    s,
                    i * 4,
                    u64::from(i) * 100,
                    if i > 20 { (i % 20 + 1) * 4 } else { 0 },
                    1,
                    name,
                    cat,
                    ProcessStatusFlags::empty(),
                    tick * u64::from(i) * 1_000,
                    u64::from(i) * 1024 * 1024,
                    tick * u64::from(i) * 256,
                );
            }
        };

        let services: Vec<ServiceRecord> = (0..500u32)
            .map(|i| ServiceRecord {
                name: Arc::from(format!("LoomSvc_{i:04}")),
                display_name: Arc::from(format!("系统服务组件 {i:04}")),
                description: Some(Arc::from(format!(
                    "{} — 负责处理第 {:04} 号后台任务与网络代理",
                    match i % 4 {
                        0 => "打印后台处理程序",
                        1 => "网络位置感知服务",
                        2 => "安全中心监控服务",
                        _ => "高性能索引服务",
                    },
                    i
                ))),
                pid: (i % 2 == 0).then_some(1000 + i),
                state: if i % 2 == 0 {
                    ServiceState::Running
                } else {
                    ServiceState::Stopped
                },
                group: Some(Arc::from("netsvcs")),
            })
            .collect();

        let cfg = ViewProjectionConfig {
            process_view_mode: ProcessViewMode::Grouped,
            process_sort_column: ProcessSortColumn::Cpu,
            process_sort_direction: SortDirection::Descending,
            services_sort_column: ServicesSortColumn::Description,
            services_sort_direction: SortDirection::Ascending,
            ..ViewProjectionConfig::default()
        };

        // Warm up caches and scratch buffers.
        fill_1000(&mut scratch, 0);
        engine.ingest_process_sample(Timestamp100Ns::new(100_000_000), &scratch, 16);
        let _ = engine.project_processes(&cfg, &cold, 1);
        let baseline_services = engine.project_services(&services, &cfg, 1);
        assert_eq!(baseline_services.visible_indices.len(), 500);

        let mut durations = Vec::with_capacity(10);
        for tick in 1..=10u64 {
            fill_1000(&mut scratch, tick);
            let started = Instant::now();
            engine.ingest_process_sample(
                Timestamp100Ns::new(100_000_000 + tick * 10_000_000),
                &scratch,
                16,
            );
            let proc_slice = engine.project_processes(&cfg, &cold, tick + 1);
            let svc_slice = engine.project_services(&services, &cfg, tick + 1);
            durations.push(started.elapsed());

            assert_eq!(proc_slice.total_process_count, 1_000);
            assert_eq!(svc_slice.visible_indices, baseline_services.visible_indices);
        }

        durations.sort();
        let median = durations[durations.len() / 2];
        assert!(
            median < std::time::Duration::from_millis(50),
            "1000-process + 500-CJK-service projection median latency {median:?} exceeded 50 ms debug budget (all: {durations:?})"
        );
    }
}
