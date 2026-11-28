//! Windows Task Manager-aligned `Grouped` process projection, app-identity matching,
//! shell launcher un-nesting, and WebView2 host attribution.

use std::hash::Hasher as _;

use loom_types::{ProcessCategory, ProcessKey, ProcessRowProjection};

use super::deadband_sort::{
    DeadbandContext, compare_process_items, sort_process_indices_slice,
    sort_process_indices_with_aggregates_slice,
};
use super::processes::{
    build_aggregated_process_row_from_parts, build_process_row_from_parts,
    compute_group_aggregate_from_parts, section_header_row,
};
use super::state::{
    ComputedProcess, HashGroupIndex, ProjectionScratch, is_hidden_system_process,
    matches_query_with_ports,
};
use super::{ProjectionEngine, ViewProjectionConfig};
use crate::query::ParsedQuery;
use crate::slow_lane::ColdMetaCache;
use crate::tree::{FastHasher, ProcessTree};
use crate::webview2::{classify_webview2, is_webview2_image};

impl ProjectionEngine {
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
    pub(super) fn project_processes_grouped_windows(
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
            ref dense_frozen_ranks,
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
        let frozen_ranks = use_frozen.then_some(dense_frozen_ranks.as_slice());
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
            let (pkg, exe) = process_identity_slices(cold_cache, proc.key);

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

                    let (pkg, exe) = process_identity_slices(cold_cache, proc.key);

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
                    computed,
                    *leader_idx,
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
                            computed,
                            c_idx,
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
                        computed,
                        idx,
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
            let entry = cold_cache.get_entry(cur_proc.key);
            let host_exe_opt = entry
                .and_then(|e| e.webview2_host_exe.as_deref())
                .or_else(|| {
                    let meta = &entry?.meta;
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
fn is_windows_shell_launcher(image_name: &str) -> bool {
    image_name.eq_ignore_ascii_case("explorer.exe")
}

#[inline]
fn process_identity_slices(
    cold_cache: &ColdMetaCache,
    key: ProcessKey,
) -> (Option<&str>, Option<&str>) {
    let Some(meta) = cold_cache.get_process(key) else {
        return (None, None);
    };
    let pkg = meta
        .app_group_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let exe = meta
        .exe_path
        .as_deref()
        .map(str::trim)
        .map(|p| p.strip_prefix(r"\\?\").unwrap_or(p))
        .filter(|s| !s.is_empty());
    (pkg, exe)
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
    let (pa, ea) = process_identity_slices(cold_cache, a.key);
    let (pb, eb) = process_identity_slices(cold_cache, b.key);
    if let (Some(pa), Some(pb)) = (pa, pb) {
        return pa.eq_ignore_ascii_case(pb);
    }
    if let (Some(ea), Some(eb)) = (ea, eb) {
        return ea.eq_ignore_ascii_case(eb);
    }
    let exe_compat = match (ea, eb) {
        (Some(ea), Some(eb)) => ea.eq_ignore_ascii_case(eb),
        _ => true,
    };
    let pkg_compat = match (pa, pb) {
        (Some(pa), Some(pb)) => pa.eq_ignore_ascii_case(pb),
        _ => true,
    };
    !a.name.is_empty() && a.name.eq_ignore_ascii_case(&b.name) && exe_compat && pkg_compat
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
