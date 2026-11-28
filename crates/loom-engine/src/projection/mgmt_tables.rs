//! Offloaded Schwartzian filtering, sorting, and flattening for on-demand management
//! domains (`Services`, `Startup`, `Users`, and `AppHistory`).

use std::collections::HashMap;
use std::sync::Arc;

use loom_types::{
    AppHistoryRecord, AppHistorySlice, AppHistorySortColumn, ProcessCategory, ProcessStatusFlags,
    ServiceRecord, ServiceState, ServicesSlice, ServicesSortColumn, SortDirection, StartupEntry,
    StartupImpact, StartupSlice, StartupSortColumn, UserSessionProcess, UserSessionRecord,
    UserSessionState, UsersRowProjection, UsersSlice, UsersSortColumn,
};

use super::state::{
    FoldedKeyArena, LiveAppHistoryEntry, SchwartzEntry, cmp_str_ascii_ci,
    effective_status_with_entry, resolve_display_name_with_entry,
};
use super::{ProjectionEngine, ViewProjectionConfig};
use crate::slow_lane::ColdMetaCache;
use crate::tree::FastBuildHasher;

/// Maximum number of distinct application keys retained in the live App History accumulator.
const MAX_LIVE_APP_HISTORY_ENTRIES: usize = 2_048;

impl ProjectionEngine {
    /// Accumulates per-tick CPU and network deltas from non-system processes into the
    /// in-memory App History map (keyed by app group ID or lowercase image name).
    ///
    /// Returns `true` if any non-zero usage delta was accumulated on this tick.
    pub fn accumulate_app_history(&mut self, cold_cache: &ColdMetaCache) -> bool {
        let mut accumulated_any = false;
        let Self {
            computed,
            app_history_live,
            scratch,
            ..
        } = self;

        for proc in computed.iter() {
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

            let cold_entry = cold_cache.get_entry(proc.key);
            let group_id = cold_entry.and_then(|e| e.meta.app_group_id.as_ref());

            // Zero-allocation lookup path for existing entries (even when `proc.name` has uppercase ASCII).
            let lookup_key: &str = if let Some(gid) = group_id {
                gid.as_ref()
            } else if proc.name.bytes().all(|b| !b.is_ascii_uppercase()) {
                proc.name.as_ref()
            } else {
                scratch.folded_arena.fold_ascii_lowercase_tmp(&proc.name)
            };

            if let Some(entry) = app_history_live.get_mut(lookup_key) {
                entry.cpu_time_100ns = entry.cpu_time_100ns.saturating_add(proc.delta_cpu_100ns);
                entry.network_bytes = entry.network_bytes.saturating_add(proc.delta_net_bytes);
                accumulated_any = true;
                // Upgrade raw image name once PE description / window title resolves.
                if entry.name.as_ref() == proc.name.as_ref() {
                    let friendly_name = resolve_display_name_with_entry(proc, cold_entry);
                    if friendly_name.as_ref() != proc.name.as_ref() {
                        entry.name = friendly_name;
                    }
                }
            } else if app_history_live.len() < MAX_LIVE_APP_HISTORY_ENTRIES {
                let app_id: Arc<str> = if let Some(gid) = group_id {
                    Arc::clone(gid)
                } else if proc.name.bytes().all(|b| !b.is_ascii_uppercase()) {
                    Arc::clone(&proc.name)
                } else {
                    Arc::from(lookup_key)
                };
                let friendly_name = resolve_display_name_with_entry(proc, cold_entry);
                app_history_live.insert(
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
        let mut merged: HashMap<Arc<str>, AppHistoryRecord, FastBuildHasher> =
            HashMap::with_capacity_and_hasher(
                persisted.len().saturating_add(self.app_history_live.len()),
                FastBuildHasher,
            );

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

                let cold_entry = cold_cache.get_entry(proc.key);
                processes.push(UserSessionProcess {
                    key: proc.key,
                    name: Arc::clone(&proc.name),
                    display_name: resolve_display_name_with_entry(proc, cold_entry),
                    status: effective_status_with_entry(proc, cold_entry, cold_cache),
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
