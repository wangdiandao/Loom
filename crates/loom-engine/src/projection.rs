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
    DetailsRowProjection, DetailsSortColumn, DetailsTableSlice, DetailsViewMode, HistoryRing,
    PerformanceSlice, ProcessArch, ProcessCategory, ProcessKey, ProcessPriority, ProcessRowKind,
    ProcessRowProjection, ProcessSortColumn, ProcessStatusFlags, ProcessTableSlice,
    ProcessViewMode, SortDirection, SystemPerfSample, Timestamp100Ns, UacVirtualization,
};

use crate::slow_lane::ColdMetaCache;
use crate::tree::{FastBuildHasher, ProcessTree};

/// Maximum number of interned strings retained by the fast-lane interner.
const MAX_INTERNED_STRINGS: usize = 4_096;

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
    ) -> (u16, u64, u64) {
        self.next_counters.insert(
            key,
            PrevProcCounters {
                cpu_time_100ns,
                disk_bytes,
                net_bytes,
            },
        );

        if elapsed_100ns == 0 {
            return (0, 0, 0);
        }

        let Some(&prev) = self.prev_counters.get(&key) else {
            return (0, 0, 0);
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

        (cpu_permille, disk_bps, net_bps)
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

/// Per-process hot + computed delta snapshot cached on the fast-lane thread.
#[derive(Debug, Clone)]
struct ComputedProcess {
    key: ProcessKey,
    session_id: u32,
    thread_count: u32,
    handle_count: u32,
    cpu_time_100ns: u64,
    cpu_permille: u16,
    private_working_set_bytes: u64,
    working_set_bytes: u64,
    commit_bytes: u64,
    memory_permille: u16,
    disk_bytes_per_sec: u64,
    net_bytes_per_sec: u64,
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
}

impl Default for ProjectionEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ProjectionEngine {
    /// Creates a fresh [`ProjectionEngine`] with pre-sized reusable buffers.
    pub fn new() -> Self {
        Self {
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
        }
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

            let (cpu_permille, disk_bytes_per_sec, net_bytes_per_sec) =
                self.delta_tracker.compute_for_process(
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
                cpu_permille,
                private_working_set_bytes: sample.private_working_set_bytes,
                working_set_bytes: sample.working_set_bytes,
                commit_bytes: sample.commit_bytes,
                memory_permille,
                disk_bytes_per_sec,
                net_bytes_per_sec,
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

        let perf_disk_bps = scratch
            .perf
            .disk_read_bytes_per_sec
            .saturating_add(scratch.perf.disk_write_bytes_per_sec);
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
        let asset = cold_cache
            .get_process(proc.key)
            .and_then(|meta| meta.asset.as_ref());

        let publisher = asset.and_then(|a| a.publisher.clone());
        let icon = asset.and_then(|a| a.icon.clone());
        let display_name = asset
            .and_then(|a| a.file_description.as_ref())
            .filter(|desc| !desc.trim().is_empty())
            .cloned()
            .or_else(|| proc.window_title.clone())
            .unwrap_or_else(|| Arc::clone(&proc.name));

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
            icon,
            category: proc.category,
            status: proc.status,
            cpu_permille: proc.cpu_permille,
            memory_bytes: proc.private_working_set_bytes,
            memory_permille: proc.memory_permille,
            disk_bytes_per_sec: proc.disk_bytes_per_sec,
            net_bytes_per_sec: proc.net_bytes_per_sec,
            gpu_util_permille: proc.gpu_util_permille,
            gpu_dedicated_bytes: proc.gpu_dedicated_bytes,
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

        DetailsRowProjection {
            key: proc.key,
            parent_key,
            depth,
            has_children,
            is_expanded,
            name: Arc::clone(&proc.name),
            description: asset.and_then(|a| a.file_description.clone()),
            icon: asset.and_then(|a| a.icon.clone()),
            status: proc.status,
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
        }
    }

    fn project_processes_grouped(
        &self,
        config: &ViewProjectionConfig,
        cold_cache: &ColdMetaCache,
        query_lower: &str,
        has_query: bool,
    ) -> Vec<ProcessRowProjection> {
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
            ProcessCategory::Windows,
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

            let section_name: Arc<str> = Arc::from(category_label(category));
            rows.push(ProcessRowProjection {
                kind: ProcessRowKind::SectionHeader {
                    category,
                    count: indices.len(),
                },
                key: ProcessKey::default(),
                depth: 0,
                has_children: false,
                is_expanded: true,
                child_count: indices.len() as u32,
                name: Arc::clone(&section_name),
                display_name: section_name,
                window_title: None,
                publisher: None,
                icon: None,
                category,
                status: ProcessStatusFlags::empty(),
                cpu_permille: 0,
                memory_bytes: 0,
                memory_permille: 0,
                disk_bytes_per_sec: 0,
                net_bytes_per_sec: 0,
                gpu_util_permille: 0,
                gpu_dedicated_bytes: 0,
            });

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
            let ord = match column {
                ProcessSortColumn::Name => {
                    let da = process_display_name_ref(a, cold_cache);
                    let db = process_display_name_ref(b, cold_cache);
                    cmp_str_ascii_ci(da, db)
                }
                ProcessSortColumn::Status => a.status.bits().cmp(&b.status.bits()),
                ProcessSortColumn::Cpu => a.cpu_permille.cmp(&b.cpu_permille),
                ProcessSortColumn::Memory => a
                    .private_working_set_bytes
                    .cmp(&b.private_working_set_bytes),
                ProcessSortColumn::Disk => a.disk_bytes_per_sec.cmp(&b.disk_bytes_per_sec),
                ProcessSortColumn::Network => a.net_bytes_per_sec.cmp(&b.net_bytes_per_sec),
                ProcessSortColumn::Gpu => (a.gpu_util_permille, a.gpu_dedicated_bytes)
                    .cmp(&(b.gpu_util_permille, b.gpu_dedicated_bytes)),
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
                DetailsSortColumn::Status => a.status.bits().cmp(&b.status.bits()),
                DetailsSortColumn::UserName => {
                    let ua = ca.and_then(|m| m.user_name.as_deref()).unwrap_or("");
                    let ub = cb.and_then(|m| m.user_name.as_deref()).unwrap_or("");
                    cmp_str_ascii_ci(ua, ub)
                }
                DetailsSortColumn::Cpu => a.cpu_permille.cmp(&b.cpu_permille),
                DetailsSortColumn::Memory => a
                    .private_working_set_bytes
                    .cmp(&b.private_working_set_bytes),
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
                DetailsSortColumn::GpuMemory => a.gpu_dedicated_bytes.cmp(&b.gpu_dedicated_bytes),
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
fn category_index(category: ProcessCategory) -> usize {
    match category {
        ProcessCategory::App => 0,
        ProcessCategory::Background => 1,
        ProcessCategory::Windows => 2,
    }
}

#[inline]
const fn category_label(category: ProcessCategory) -> &'static str {
    match category {
        ProcessCategory::App => "Apps",
        ProcessCategory::Background => "Background",
        ProcessCategory::Windows => "Windows",
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
            ProcessCategory::Windows,
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
}
