//! `Details` table flat and tree view projections with static-column order caching.

use std::sync::Arc;

use loom_types::{
    DetailsRowProjection, DetailsTableSlice, DetailsViewMode, ProcessArch, UacVirtualization,
};

use super::deadband_sort::{
    DeadbandContext, compute_details_static_keys_hash, details_column_deadband,
    details_column_is_static, sort_details_indices_slice,
};
use super::state::{
    ComputedProcess, ProjectionScratch, collect_matching_with_ancestors_into,
    effective_status_with_entry, is_hidden_system_process, matches_query_with_ports,
};
use super::{ProjectionEngine, ViewProjectionConfig};
use crate::leaks::LeakDetector;
use crate::query::ParsedQuery;
use crate::slow_lane::ColdMetaCache;
use crate::tree::ProcessTree;
use crate::webview2::{classify_webview2, is_webview2_image};

impl ProjectionEngine {
    /// Builds a pre-sorted, pre-filtered [`DetailsTableSlice`].
    pub fn project_details(
        &mut self,
        config: &ViewProjectionConfig,
        cold_cache: &ColdMetaCache,
        revision: u64,
    ) -> DetailsTableSlice {
        let parsed_query = ParsedQuery::parse(&config.search_query);
        self.project_details_with_query(config, &parsed_query, cold_cache, revision)
    }

    /// Builds a pre-sorted, pre-filtered [`DetailsTableSlice`] reusing an already-parsed
    /// [`ParsedQuery`] to avoid re-tokenizing the search string on every tick.
    pub fn project_details_with_query(
        &mut self,
        config: &ViewProjectionConfig,
        parsed_query: &ParsedQuery,
        cold_cache: &ColdMetaCache,
        revision: u64,
    ) -> DetailsTableSlice {
        let use_frozen = self.scratch.populate_frozen_ranks(
            config.details_order_frozen,
            &config.frozen_details_order,
            &self.computed,
        );

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
                parsed_query,
                use_frozen,
                details_deadband,
            ),
            DetailsViewMode::Tree => self.project_details_tree(
                config,
                cold_cache,
                parsed_query,
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
            ref dense_frozen_ranks,
            ref dense_deadband,
            section_roots: ref mut indices,
            ref mut details_static_order,
            ..
        } = *scratch;
        let frozen_ranks = use_frozen.then_some(dense_frozen_ranks.as_slice());
        let deadband_ctx = deadband.map(|db| DeadbandContext {
            deadband: db,
            dense_prev: dense_deadband.as_slice(),
        });

        indices.clear();

        let can_use_static_cache =
            !has_query && !use_frozen && details_column_is_static(config.details_sort_column);

        let mut cache_hit = false;
        let mut live_keys_hash = 0u64;
        let cold_version = cold_cache.version();
        if can_use_static_cache {
            live_keys_hash = compute_details_static_keys_hash(computed);
            if details_static_order.valid
                && details_static_order.sort_column == config.details_sort_column
                && details_static_order.sort_direction == config.details_sort_direction
                && details_static_order.hide_system == config.hide_windows_system_processes
                && details_static_order.cold_version == cold_version
                && details_static_order.computed_len == computed.len()
                && details_static_order.live_keys_hash == live_keys_hash
                && details_static_order.ordered_indices.len()
                    == details_static_order.ordered_keys.len()
            {
                let slots_match = details_static_order
                    .ordered_indices
                    .iter()
                    .zip(details_static_order.ordered_keys.iter())
                    .all(|(&idx, &expected_key)| {
                        computed
                            .get(idx)
                            .is_some_and(|proc| proc.key == expected_key)
                    });
                if slots_match {
                    indices.extend_from_slice(&details_static_order.ordered_indices);
                    cache_hit = true;
                }
            }
        }

        if !cache_hit {
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

            if can_use_static_cache {
                details_static_order.valid = true;
                details_static_order.sort_column = config.details_sort_column;
                details_static_order.sort_direction = config.details_sort_direction;
                details_static_order.hide_system = config.hide_windows_system_processes;
                details_static_order.cold_version = cold_version;
                details_static_order.computed_len = computed.len();
                details_static_order.live_keys_hash = live_keys_hash;
                details_static_order.ordered_indices.clear();
                details_static_order
                    .ordered_indices
                    .extend_from_slice(indices);
                details_static_order.ordered_keys.clear();
                details_static_order
                    .ordered_keys
                    .extend(indices.iter().map(|&idx| computed[idx].key));
            }
        }

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
    let status = effective_status_with_entry(proc, cold_entry, cold_cache);
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
        working_set_delta_bytes: proc.working_set_delta_bytes,
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
