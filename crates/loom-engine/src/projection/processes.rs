//! `Processes` table projections (platform-generic grouped mode, parent-child tree mode,
//! and shared process row/aggregate builders).

use std::sync::Arc;

use loom_types::{
    PlatformKind, ProcessCategory, ProcessKey, ProcessRowKind, ProcessRowProjection,
    ProcessStatusFlags, ProcessTableSlice, ProcessViewMode,
};

use super::deadband_sort::{
    DeadbandContext, process_column_deadband, process_row_deadband_metric,
    sort_process_indices_slice,
};
use super::state::{
    ComputedProcess, GroupAggregate, ProjectionScratch, category_index, category_label_arc,
    collect_matching_with_ancestors_into, compute_throughput_permille, effective_status_with_entry,
    is_hidden_system_process, matches_query_with_ports, resolve_display_name_with_entry,
};
use super::{ProjectionEngine, ViewProjectionConfig};
use crate::leaks::LeakDetector;
use crate::query::ParsedQuery;
use crate::slow_lane::ColdMetaCache;
use crate::tree::ProcessTree;
use crate::webview2::{classify_webview2, is_webview2_image};

impl ProjectionEngine {
    /// Builds a pre-sorted, pre-filtered [`ProcessTableSlice`].
    pub fn project_processes(
        &mut self,
        config: &ViewProjectionConfig,
        cold_cache: &ColdMetaCache,
        revision: u64,
    ) -> ProcessTableSlice {
        let parsed_query = ParsedQuery::parse(&config.search_query);
        self.project_processes_with_query(config, &parsed_query, cold_cache, revision)
    }

    /// Builds a pre-sorted, pre-filtered [`ProcessTableSlice`] reusing an already-parsed
    /// [`ParsedQuery`] to avoid re-tokenizing the search string on every tick.
    pub fn project_processes_with_query(
        &mut self,
        config: &ViewProjectionConfig,
        parsed_query: &ParsedQuery,
        cold_cache: &ColdMetaCache,
        revision: u64,
    ) -> ProcessTableSlice {
        let use_frozen = self.scratch.populate_frozen_ranks(
            config.process_order_frozen,
            &config.frozen_process_order,
            &self.computed,
        );

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
                parsed_query,
                use_frozen,
                proc_deadband,
            ),
            ProcessViewMode::Tree => self.project_processes_tree(
                config,
                cold_cache,
                parsed_query,
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
            ref dense_frozen_ranks,
            ref dense_deadband,
            bool_mask_a: ref mut in_any_section,
            bool_mask_b: ref mut visited,
            ref mut stack,
            ref mut section_roots,
            ref mut visible_children,
            ref mut by_category,
            ..
        } = *scratch;
        let frozen_ranks = use_frozen.then_some(dense_frozen_ranks.as_slice());
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
                    computed,
                    idx,
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
            ref dense_frozen_ranks,
            ref dense_deadband,
            bool_mask_a: ref mut visible_mask,
            bool_mask_b: ref mut visited,
            bool_mask_c: ref mut included_mask,
            ref mut stack,
            section_roots: ref mut roots,
            ref mut visible_children,
            ..
        } = *scratch;
        let frozen_ranks = use_frozen.then_some(dense_frozen_ranks.as_slice());
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
                computed,
                idx,
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
}

#[inline]
#[allow(clippy::too_many_arguments)]
pub(super) fn build_process_row_from_parts(
    computed: &[ComputedProcess],
    idx: usize,
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
    let proc = &computed[idx];
    let cold_entry = cold_cache.get_entry(proc.key);
    let cold = cold_entry.map(|e| &e.meta);
    let asset = cold.and_then(|meta| meta.asset.as_ref());

    let publisher = asset.and_then(|a| a.publisher.clone());
    let exe_path = cold.and_then(|meta| meta.exe_path.clone());
    let command_line = cold.and_then(|meta| meta.command_line.clone());
    let icon = asset.and_then(|a| a.icon.clone());
    let display_name = resolve_display_name_with_entry(proc, cold_entry);
    let status = effective_status_with_entry(proc, cold_entry, cold_cache);
    let disk_permille =
        compute_throughput_permille(proc.disk_bytes_per_sec, total_disk_bytes_per_sec);
    let net_permille = compute_throughput_permille(proc.net_bytes_per_sec, total_net_bytes_per_sec);
    let parent_key = latest_tree.parent_index(idx).map(|p| latest_tree.key_at(p));
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
pub(super) fn build_aggregated_process_row_from_parts(
    computed: &[ComputedProcess],
    idx: usize,
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
        computed,
        idx,
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

pub(super) fn compute_group_aggregate_from_parts(
    computed: &[ComputedProcess],
    leak_detector: &LeakDetector,
    mem_total_bytes: u64,
    root_idx: usize,
    children: &[usize],
    config: &ViewProjectionConfig,
    cold_cache: &ColdMetaCache,
) -> GroupAggregate {
    let root = &computed[root_idx];
    let root_entry = cold_cache.get_entry(root.key);
    let root_status = effective_status_with_entry(root, root_entry, cold_cache);
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
        root_entry.and_then(|e| e.meta.exe_path.as_deref()),
        config.show_leak_warnings,
        &config.leak_ignored_paths,
    );

    for &c_idx in children {
        let child = &computed[c_idx];
        let child_entry = cold_cache.get_entry(child.key);
        let child_status = effective_status_with_entry(child, child_entry, cold_cache);
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
                child_entry.and_then(|e| e.meta.exe_path.as_deref()),
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

pub(super) fn section_header_row(category: ProcessCategory, count: usize) -> ProcessRowProjection {
    let section_name = category_label_arc(category);
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
