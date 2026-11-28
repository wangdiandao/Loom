//! Internal state, delta tracking, string interning, and reusable scratch buffers
//! for [`super::ProjectionEngine`].

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use loom_types::{
    DetailsSortColumn, DetailsViewMode, LeakWarning, ProcessArch, ProcessCategory, ProcessKey,
    ProcessPriority, ProcessSortColumn, ProcessStatusFlags, ProcessViewMode, SortDirection,
    Timestamp100Ns, UacVirtualization,
};

use super::deadband_sort::DenseDeadbandEntry;
use crate::query::{ParsedQuery, QueryMatchContext};
use crate::slow_lane::{CachedProcessMeta, ColdMetaCache};
use crate::tree::{FastBuildHasher, ProcessTree};

/// Maximum number of interned strings retained by the fast-lane interner.
const MAX_INTERNED_STRINGS: usize = 4_096;

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

pub(super) type ProcSortSignature = (ProcessViewMode, ProcessSortColumn, SortDirection, String);
pub(super) type DetailsSortSignature = (DetailsViewMode, DetailsSortColumn, SortDirection, String);

#[derive(Debug, Clone, Copy)]
struct PrevProcCounters {
    cpu_time_100ns: u64,
    disk_bytes: u64,
    net_bytes: u64,
    working_set_bytes: u64,
}

/// Double-buffered per-process delta calculator avoiding map reallocation across ticks.
#[derive(Debug, Default)]
pub(super) struct DeltaTracker {
    prev_timestamp: Option<Timestamp100Ns>,
    prev_counters: HashMap<ProcessKey, PrevProcCounters, FastBuildHasher>,
    next_counters: HashMap<ProcessKey, PrevProcCounters, FastBuildHasher>,
}

impl DeltaTracker {
    pub(super) fn with_capacity(cap: usize) -> Self {
        Self {
            prev_timestamp: None,
            prev_counters: HashMap::with_capacity_and_hasher(cap, FastBuildHasher),
            next_counters: HashMap::with_capacity_and_hasher(cap, FastBuildHasher),
        }
    }

    pub(super) fn begin_tick(&mut self, timestamp: Timestamp100Ns) -> u64 {
        self.next_counters.clear();
        self.prev_timestamp
            .map(|prev| timestamp.saturating_sub(prev).as_raw())
            .unwrap_or(0)
    }

    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn compute_for_process(
        &mut self,
        key: ProcessKey,
        cpu_time_100ns: u64,
        disk_bytes: u64,
        net_bytes: u64,
        working_set_bytes: u64,
        elapsed_100ns: u64,
        logical_cpus: u64,
    ) -> (u16, u64, u64, u64, u64, i64) {
        self.next_counters.insert(
            key,
            PrevProcCounters {
                cpu_time_100ns,
                disk_bytes,
                net_bytes,
                working_set_bytes,
            },
        );

        if elapsed_100ns == 0 {
            return (0, 0, 0, 0, 0, 0);
        }

        let Some(&prev) = self.prev_counters.get(&key) else {
            return (0, 0, 0, 0, 0, 0);
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

        let working_set_delta_bytes =
            (i128::from(working_set_bytes) - i128::from(prev.working_set_bytes))
                .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;

        (
            cpu_permille,
            disk_bps,
            net_bps,
            delta_cpu,
            delta_net,
            working_set_delta_bytes,
        )
    }

    pub(super) fn finish_tick(&mut self, timestamp: Timestamp100Ns) {
        std::mem::swap(&mut self.prev_counters, &mut self.next_counters);
        self.prev_timestamp = Some(timestamp);
    }
}

/// Fast-lane local string interner preventing steady-state heap allocations for repeated image names.
#[derive(Debug, Default)]
pub(super) struct StringInterner {
    pool: HashSet<Arc<str>, FastBuildHasher>,
}

impl StringInterner {
    #[inline]
    pub(super) fn intern(&mut self, s: &str) -> Arc<str> {
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
pub(super) struct LiveAppHistoryEntry {
    pub(super) name: Arc<str>,
    pub(super) cpu_time_100ns: u64,
    pub(super) network_bytes: u64,
    pub(super) metered_network_bytes: u64,
}

/// Per-process hot + computed delta snapshot cached on the fast-lane thread.
#[derive(Debug, Clone)]
pub(super) struct ComputedProcess {
    pub(super) key: ProcessKey,
    pub(super) session_id: u32,
    pub(super) thread_count: u32,
    pub(super) handle_count: u32,
    pub(super) cpu_time_100ns: u64,
    pub(super) delta_cpu_100ns: u64,
    pub(super) cpu_permille: u16,
    pub(super) private_working_set_bytes: u64,
    pub(super) working_set_bytes: u64,
    pub(super) working_set_delta_bytes: i64,
    pub(super) commit_bytes: u64,
    pub(super) memory_permille: u16,
    pub(super) disk_bytes_per_sec: u64,
    pub(super) net_bytes_per_sec: u64,
    pub(super) delta_net_bytes: u64,
    pub(super) gpu_dedicated_bytes: u64,
    pub(super) gpu_shared_bytes: u64,
    pub(super) gpu_util_permille: u16,
    pub(super) priority: ProcessPriority,
    pub(super) category: ProcessCategory,
    pub(super) status: ProcessStatusFlags,
    pub(super) name: Arc<str>,
    pub(super) window_title: Option<Arc<str>>,
}

/// Aggregated metrics and rolled-up flags for a collapsible Windows app or section group.
#[derive(Debug, Clone)]
pub(super) struct GroupAggregate {
    pub(super) cpu_permille: u16,
    pub(super) memory_bytes: u64,
    pub(super) memory_permille: u16,
    pub(super) disk_bytes_per_sec: u64,
    pub(super) net_bytes_per_sec: u64,
    pub(super) gpu_util_permille: u16,
    pub(super) gpu_dedicated_bytes: u64,
    pub(super) status: ProcessStatusFlags,
    pub(super) leak_warning: Option<LeakWarning>,
}

/// Zero-allocation intrusive hash-bucket index mapping `u64` hashes to FIFO chains
/// of `(group_idx, sample_proc_idx)` entries so callers can verify exact string equality
/// on hash collisions without storing lifetime-bound `&str` keys.
#[derive(Debug, Default)]
pub(super) struct HashGroupIndex {
    heads: HashMap<u64, (usize, usize), FastBuildHasher>,
    nodes: Vec<(usize, usize, usize)>, // (group_idx, sample_proc_idx, next_node_idx or usize::MAX)
}

impl HashGroupIndex {
    pub(super) fn clear(&mut self) {
        self.heads.clear();
        self.nodes.clear();
    }

    #[inline]
    pub(super) fn push(&mut self, hash: u64, group_idx: usize, sample_proc_idx: usize) {
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
    pub(super) fn find<F>(&self, hash: u64, mut predicate: F) -> Option<usize>
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
pub(super) struct FoldedKeyArena {
    buf: String,
    fold_tmp: String,
}

impl FoldedKeyArena {
    #[inline]
    pub(super) fn clear(&mut self) {
        self.buf.clear();
    }

    #[inline]
    pub(super) fn push_folded(&mut self, s: &str) -> (u32, u32) {
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
    pub(super) fn slice(&self, range: (u32, u32)) -> &str {
        &self.buf[range.0 as usize..range.1 as usize]
    }

    #[inline]
    pub(super) fn fold_ascii_lowercase_tmp(&mut self, s: &str) -> &str {
        self.fold_tmp.clear();
        self.fold_tmp.reserve(s.len());
        for ch in s.chars() {
            self.fold_tmp.push(ch.to_ascii_lowercase());
        }
        &self.fold_tmp
    }

    #[inline]
    pub(super) fn contains_icase(&mut self, haystack: &str, needle_lower: &str) -> bool {
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
pub(super) struct SchwartzEntry {
    pub(super) index: usize,
    pub(super) numeric: u64,
    pub(super) primary_str: (u32, u32),
    pub(super) tie_str: (u32, u32),
}

/// Cached sorted index order for static `Details` sort columns.
#[derive(Debug, Default)]
pub(super) struct DetailsStaticOrderCache {
    pub(super) valid: bool,
    pub(super) sort_column: DetailsSortColumn,
    pub(super) sort_direction: SortDirection,
    pub(super) hide_system: bool,
    pub(super) cold_version: u64,
    pub(super) computed_len: usize,
    pub(super) live_keys_hash: u64,
    pub(super) ordered_indices: Vec<usize>,
    pub(super) ordered_keys: Vec<ProcessKey>,
}

/// Persistent scratch buffers owned by [`super::ProjectionEngine`] and cleared in place across ticks
/// to eliminate steady-state per-tick heap allocations during grouping, sorting, and tree flattening.
#[derive(Debug, Default)]
pub(super) struct ProjectionScratch {
    pub(super) frozen_ranks: HashMap<ProcessKey, usize, FastBuildHasher>,
    pub(super) dense_frozen_ranks: Vec<usize>,
    pub(super) dense_deadband: Vec<DenseDeadbandEntry>,
    pub(super) bool_mask_a: Vec<bool>,
    pub(super) bool_mask_b: Vec<bool>,
    pub(super) bool_mask_c: Vec<bool>,
    pub(super) stack: Vec<(usize, u16)>,
    pub(super) section_roots: Vec<usize>,
    pub(super) visible_children: Vec<usize>,
    pub(super) by_category: [Vec<usize>; 3],
    pub(super) deferred_webview2: Vec<usize>,
    pub(super) subtree_descendants: Vec<usize>,
    pub(super) desc_stack: Vec<usize>,
    pub(super) group_agg: Vec<Option<GroupAggregate>>,
    pub(super) subtree_counts: Vec<u32>,
    pub(super) idx_to_app_group: Vec<Option<usize>>,
    pub(super) app_group_leaders: Vec<usize>,
    pub(super) app_group_children: Vec<Vec<usize>>,
    pub(super) visible_apps: Vec<(usize, usize, GroupAggregate)>,
    pub(super) pkg_to_group: HashGroupIndex,
    pub(super) exe_to_group: HashGroupIndex,
    pub(super) name_to_groups: HashGroupIndex,
    pub(super) folded_arena: FoldedKeyArena,
    pub(super) schwartz_entries: Vec<SchwartzEntry>,
    pub(super) schwartz_children: Vec<SchwartzEntry>,
    pub(super) details_static_order: DetailsStaticOrderCache,
}

impl ProjectionScratch {
    pub(super) fn with_capacity(cap: usize) -> Self {
        Self {
            frozen_ranks: HashMap::with_capacity_and_hasher(cap, FastBuildHasher),
            dense_frozen_ranks: Vec::with_capacity(cap),
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
            details_static_order: DetailsStaticOrderCache::default(),
        }
    }

    #[inline]
    pub(super) fn populate_dense_deadband(
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
    pub(super) fn populate_frozen_ranks(
        &mut self,
        frozen: bool,
        order: &[ProcessKey],
        computed: &[ComputedProcess],
    ) -> bool {
        self.frozen_ranks.clear();
        self.dense_frozen_ranks.clear();
        if !frozen || order.is_empty() {
            return false;
        }
        self.frozen_ranks.reserve(order.len());
        for (rank, &key) in order.iter().enumerate() {
            self.frozen_ranks.entry(key).or_insert(rank);
        }
        self.dense_frozen_ranks.reserve(computed.len());
        for proc in computed {
            self.dense_frozen_ranks.push(
                self.frozen_ranks
                    .get(&proc.key)
                    .copied()
                    .unwrap_or(usize::MAX),
            );
        }
        true
    }
}

pub(super) fn collect_matching_with_ancestors_into<F>(
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

pub(super) fn matches_query_with_ports(
    proc: &ComputedProcess,
    cold_cache: &ColdMetaCache,
    parsed_query: &ParsedQuery,
    listening_ports: &HashSet<(u32, u16)>,
    include_user_in_text: bool,
) -> bool {
    let cold_entry = cold_cache.get_entry(proc.key);
    let cold = cold_entry.map(|e| &e.meta);
    let asset = cold.and_then(|m| m.asset.as_ref());
    let display_name = process_display_name_ref_with_entry(proc, cold_entry);
    let status = effective_status_with_entry(proc, cold_entry, cold_cache);
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
pub(super) fn resolve_display_name_with_entry(
    proc: &ComputedProcess,
    cold_entry: Option<&CachedProcessMeta>,
) -> Arc<str> {
    cold_entry
        .and_then(|entry| entry.meta.asset.as_ref())
        .and_then(|asset| asset.file_description.as_ref())
        .filter(|desc| !desc.trim().is_empty())
        .cloned()
        .or_else(|| proc.window_title.clone())
        .unwrap_or_else(|| Arc::clone(&proc.name))
}

#[inline]
pub(super) fn effective_status_with_entry(
    proc: &ComputedProcess,
    cold_entry: Option<&CachedProcessMeta>,
    cold_cache: &ColdMetaCache,
) -> ProcessStatusFlags {
    let mut status = proc.status;
    if let Some(entry) = cold_entry {
        if entry.meta.efficiency_mode {
            status |= ProcessStatusFlags::EFFICIENCY_MODE;
        }
        if entry.meta.is_elevated {
            status |= ProcessStatusFlags::ELEVATED;
        }
    } else if cold_cache.efficiency_mode(proc.key) {
        status |= ProcessStatusFlags::EFFICIENCY_MODE;
    }
    status
}

#[inline]
pub(super) fn effective_status(
    proc: &ComputedProcess,
    cold_cache: &ColdMetaCache,
) -> ProcessStatusFlags {
    effective_status_with_entry(proc, cold_cache.get_entry(proc.key), cold_cache)
}

#[inline]
pub(super) fn compute_throughput_permille(bytes_per_sec: u64, total_bytes_per_sec: u64) -> u16 {
    if bytes_per_sec == 0 || total_bytes_per_sec == 0 {
        return 0;
    }
    let total = u128::from(total_bytes_per_sec);
    ((u128::from(bytes_per_sec) * 1_000 + (total / 2)) / total).min(1_000) as u16
}

#[inline]
pub(super) fn ratio_permille(used: u64, total: u64) -> u16 {
    if used == 0 || total == 0 {
        return 0;
    }
    ((u128::from(used) * 1_000) / u128::from(total)).min(1_000) as u16
}

#[inline]
pub(super) fn category_index(category: ProcessCategory) -> usize {
    match category {
        ProcessCategory::App => 0,
        ProcessCategory::Background => 1,
        ProcessCategory::System => 2,
    }
}

#[inline]
pub(super) fn category_label_arc(category: ProcessCategory) -> Arc<str> {
    static LABELS: OnceLock<[Arc<str>; 3]> = OnceLock::new();
    let labels = LABELS.get_or_init(|| {
        [
            Arc::from("Apps"),
            Arc::from("Background"),
            Arc::from("System"),
        ]
    });
    Arc::clone(&labels[category_index(category)])
}

#[inline]
pub(super) const fn arch_rank(arch: ProcessArch) -> u8 {
    match arch {
        ProcessArch::Unknown => 0,
        ProcessArch::X86 => 1,
        ProcessArch::X64 => 2,
        ProcessArch::Arm64 => 3,
    }
}

#[inline]
pub(super) const fn uac_rank(uac: UacVirtualization) -> u8 {
    match uac {
        UacVirtualization::Unknown => 0,
        UacVirtualization::NotAllowed => 1,
        UacVirtualization::Disabled => 2,
        UacVirtualization::Enabled => 3,
    }
}

#[inline]
pub(super) fn process_display_name_ref_with_entry<'a>(
    proc: &'a ComputedProcess,
    cold_entry: Option<&'a CachedProcessMeta>,
) -> &'a str {
    if let Some(desc) = cold_entry
        .and_then(|e| e.meta.asset.as_ref())
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
pub(super) fn process_display_name_ref<'a>(
    proc: &'a ComputedProcess,
    cold_cache: &'a ColdMetaCache,
) -> &'a str {
    process_display_name_ref_with_entry(proc, cold_cache.get_entry(proc.key))
}

#[inline]
pub(super) fn is_hidden_system_process(proc: &ComputedProcess, hide_system: bool) -> bool {
    if !hide_system || proc.category != ProcessCategory::System {
        return false;
    }
    !INTERACTIVE_SHELL_EXEMPTIONS
        .iter()
        .any(|exempt| proc.name.eq_ignore_ascii_case(exempt))
}

#[inline]
pub(super) fn cmp_str_ascii_ci(a: &str, b: &str) -> Ordering {
    for (ba, bb) in a.bytes().zip(b.bytes()) {
        let ord = ba.to_ascii_lowercase().cmp(&bb.to_ascii_lowercase());
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}
