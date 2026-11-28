//! Slow-lane result draining, cold-metadata coalescing, and on-demand domain updates.

use std::sync::Arc;
use std::time::Instant;

use crossbeam_channel::Sender;
use loom_pal_api::{PalError, PlatformBackend};
use loom_types::{AppHistoryRecord, MonitorDomain, ServiceRecord, StartupEntry, UserSessionRecord};

use super::runner::FastLaneRunner;
use super::{FAILED_PROBE_RETRY_COOLDOWN, SLOW_RESULT_DRAIN_BATCH, SliceUpdateMask};
use crate::command::CommandEvent;
use crate::slow_lane::{CircuitBreaker, MgmtDomainRefresh, SlowProbeJob, SlowResult};

impl<B: PlatformBackend> FastLaneRunner<B> {
    pub(super) fn handle_slow_result_batch(&mut self, first: SlowResult) {
        let mut cold_updated = false;
        let mut perf_updated = false;
        let mut snapshot_dirty = false;
        let mut pending_events: Vec<(CommandEvent, Option<Sender<CommandEvent>>)> = Vec::new();

        self.apply_slow_result(
            first,
            &mut cold_updated,
            &mut perf_updated,
            &mut snapshot_dirty,
            &mut pending_events,
        );

        for _ in 1..SLOW_RESULT_DRAIN_BATCH {
            match self.slow_pool.result_rx().try_recv() {
                Ok(res) => self.apply_slow_result(
                    res,
                    &mut cold_updated,
                    &mut perf_updated,
                    &mut snapshot_dirty,
                    &mut pending_events,
                ),
                Err(_) => break,
            }
        }

        if !self.window_dormant && !self.temp_frozen {
            let cold_has_consumer = cold_updated && self.has_active_cold_consumer();
            if cold_has_consumer || perf_updated {
                let now = Instant::now();
                let coalesce_window = self.config.cold_meta_coalesce_window;
                let can_defer_cold = cold_has_consumer
                    && !perf_updated
                    && !snapshot_dirty
                    && !coalesce_window.is_zero()
                    && self
                        .last_cold_publish_at
                        .is_some_and(|last| now.saturating_duration_since(last) < coalesce_window);

                if can_defer_cold {
                    self.pending_cold_publish = true;
                    let target = self
                        .last_cold_publish_at
                        .map_or(now + coalesce_window, |last| last + coalesce_window);
                    self.cold_flush_deadline.get_or_insert(target);
                } else {
                    let include_cold = cold_has_consumer || self.pending_cold_publish;
                    if include_cold {
                        self.pending_cold_publish = false;
                        self.cold_flush_deadline = None;
                        self.last_cold_publish_at = Some(now);
                    }
                    let processes_active = include_cold && self.processes_projection_active();
                    let details_leased =
                        include_cold && self.leases.is_active(MonitorDomain::Details);
                    let users_active = include_cold
                        && self.leases.is_active(MonitorDomain::Users)
                        && (self.users_polled_once || !self.cached_user_sessions.is_empty());
                    let per_core_leased =
                        perf_updated && self.leases.is_active(MonitorDomain::PerCoreCpu);
                    let mask = SliceUpdateMask {
                        processes: processes_active,
                        details: details_leased,
                        performance: perf_updated,
                        include_per_core: per_core_leased,
                        users: users_active,
                        ..SliceUpdateMask::default()
                    };
                    if mask.any() {
                        self.publish_slices(mask);
                    } else if snapshot_dirty {
                        self.snapshot_store
                            .store(Arc::new(self.current_snapshot.clone()));
                        self.wake_signal.notify();
                    }
                }
            } else if snapshot_dirty {
                self.snapshot_store
                    .store(Arc::new(self.current_snapshot.clone()));
                self.wake_signal.notify();
            }
        }

        for (event, reply) in pending_events {
            self.events.emit(event, reply);
        }
    }

    fn apply_slow_result(
        &mut self,
        res: SlowResult,
        cold_updated: &mut bool,
        perf_updated: &mut bool,
        snapshot_dirty: &mut bool,
        pending_events: &mut Vec<(CommandEvent, Option<Sender<CommandEvent>>)>,
    ) {
        let now = Instant::now();
        match res {
            SlowResult::ProcessMeta {
                key,
                result,
                elapsed,
            } => {
                self.pending_procs.remove(&key);
                match result {
                    Ok(meta) => {
                        if let Some(ref exe_path) = meta.exe_path {
                            let target_key = CircuitBreaker::target_key_for_path(exe_path);
                            self.circuit_breaker
                                .record_probe(&target_key, elapsed, false, now);
                        }
                        if let Some((exe_path, cache_key)) = self.cold_cache.insert_process(meta) {
                            let target_key = CircuitBreaker::target_key_for_path(&exe_path);
                            if !self.pending_paths.contains(&cache_key)
                                && !self.circuit_breaker.is_open(&target_key, now)
                                && self
                                    .slow_pool
                                    .try_enqueue_probe(SlowProbeJob::ProbeExeAsset {
                                        exe_path,
                                        cache_key: Arc::clone(&cache_key),
                                    })
                            {
                                self.pending_paths.insert(cache_key);
                            }
                        }
                        *cold_updated = true;
                    }
                    Err(err) => {
                        let timed_out =
                            matches!(err, PalError::TimedOut(_) | PalError::CircuitOpen { .. });
                        if timed_out || elapsed >= self.circuit_breaker.timeout() {
                            let proc_target = format!("pid:{}", key.pid);
                            self.circuit_breaker.record_probe(
                                &proc_target,
                                elapsed,
                                timed_out,
                                now,
                            );
                        }
                        self.failed_procs
                            .insert(key, now + FAILED_PROBE_RETRY_COOLDOWN);
                    }
                }
            }
            SlowResult::ExeAsset {
                exe_path,
                cache_key,
                result,
                elapsed,
            } => {
                self.pending_paths.remove(&cache_key);
                let target_key = CircuitBreaker::target_key_for_path(&exe_path);
                match result {
                    Ok(asset) => {
                        self.circuit_breaker
                            .record_probe(&target_key, elapsed, false, now);
                        if self.cold_cache.insert_asset(cache_key, asset) {
                            *cold_updated = true;
                        }
                    }
                    Err(err) => {
                        let timed_out =
                            matches!(err, PalError::TimedOut(_) | PalError::CircuitOpen { .. });
                        self.circuit_breaker
                            .record_probe(&target_key, elapsed, timed_out, now);
                    }
                }
            }
            SlowResult::HardwareStaticInfo(result) => {
                self.pending_hardware_info = false;
                match result {
                    Ok(info) => {
                        self.hardware_info_probed = true;
                        self.projection.set_hardware_static_info(info);
                        *perf_updated = true;
                    }
                    Err(err) => {
                        tracing::warn!(%err, "slow-lane hardware static info probe failed");
                    }
                }
            }
            SlowResult::DiskPerformancePolled(result) => {
                self.pending_disk_perf = false;
                match result {
                    Ok(disks) => {
                        if self.leases.is_active(MonitorDomain::Performance) {
                            self.projection.record_disk_performance(&disks);
                            *perf_updated = true;
                        }
                    }
                    Err(err) => {
                        tracing::warn!(%err, "slow-lane disk performance poll failed");
                    }
                }
            }
            SlowResult::DiskPerformanceSkipped => {
                self.pending_disk_perf = false;
            }
            SlowResult::ListeningPortsPolled(result) => {
                self.pending_port_scan = false;
                self.last_port_scan_at = Some(now);
                match result {
                    Ok(sockets) => {
                        if self.parsed_query.has_port_predicate() {
                            self.projection.set_listening_ports(&sockets);
                            *cold_updated = true;
                        }
                    }
                    Err(err) => {
                        tracing::warn!(%err, "slow-lane listening port scan failed");
                    }
                }
            }
            SlowResult::AppHistorySaved(result) => {
                self.pending_app_history_save = false;
                if let Err(err) = result {
                    tracing::warn!(%err, "slow-lane AppHistory periodic save failed");
                }
            }
            SlowResult::DomainPollSkipped(domain) => {
                self.pending_domains[domain.index()] = false;
            }
            SlowResult::ServicesPolled(result) => {
                self.pending_domains[MonitorDomain::Services.index()] = false;
                if let Ok(items) = result
                    && self.leases.is_active(MonitorDomain::Services)
                    && self.apply_services_snapshot(items)
                {
                    *snapshot_dirty = true;
                }
            }
            SlowResult::StartupPolled(result) => {
                self.pending_domains[MonitorDomain::Startup.index()] = false;
                if let Ok((last_bios_time_ms, items)) = result
                    && self.leases.is_active(MonitorDomain::Startup)
                    && self.apply_startup_snapshot(last_bios_time_ms, items)
                {
                    *snapshot_dirty = true;
                }
            }
            SlowResult::UsersPolled(result) => {
                self.pending_domains[MonitorDomain::Users.index()] = false;
                if let Ok(sessions) = result
                    && self.leases.is_active(MonitorDomain::Users)
                {
                    self.apply_users_snapshot(sessions);
                    *snapshot_dirty = true;
                }
            }
            SlowResult::AppHistoryPolled(result) => {
                self.pending_domains[MonitorDomain::AppHistory.index()] = false;
                if let Ok((since_epoch_secs, items)) = result
                    && self.leases.is_active(MonitorDomain::AppHistory)
                    && self.apply_app_history_snapshot(since_epoch_secs, items)
                {
                    *snapshot_dirty = true;
                }
            }
            SlowResult::MgmtOpCompleted {
                event,
                reply,
                cleared_app_history,
                refresh,
            } => {
                if cleared_app_history {
                    self.projection.clear_app_history();
                    self.persisted_app_history.clear();
                    self.app_history_dirty = false;
                    self.app_history_projection_dirty = true;
                    if refresh.is_none() && self.leases.is_active(MonitorDomain::AppHistory) {
                        let since = self.persisted_app_history_since;
                        if self.apply_app_history_snapshot(since, Vec::new()) {
                            *snapshot_dirty = true;
                        }
                    }
                }
                if let Some(domain_refresh) = refresh {
                    match domain_refresh {
                        MgmtDomainRefresh::Services(Ok(items))
                            if self.leases.is_active(MonitorDomain::Services) =>
                        {
                            self.services_projection_dirty = true;
                            if self.apply_services_snapshot(items) {
                                *snapshot_dirty = true;
                            }
                        }
                        MgmtDomainRefresh::Startup(Ok((bios_ms, items)))
                            if self.leases.is_active(MonitorDomain::Startup) =>
                        {
                            self.startup_projection_dirty = true;
                            if self.apply_startup_snapshot(bios_ms, items) {
                                *snapshot_dirty = true;
                            }
                        }
                        MgmtDomainRefresh::Users(Ok(sessions))
                            if self.leases.is_active(MonitorDomain::Users) =>
                        {
                            self.apply_users_snapshot(sessions);
                            *snapshot_dirty = true;
                        }
                        MgmtDomainRefresh::AppHistory(Ok((since, items)))
                            if self.leases.is_active(MonitorDomain::AppHistory) =>
                        {
                            self.app_history_projection_dirty = true;
                            if self.apply_app_history_snapshot(since, items) {
                                *snapshot_dirty = true;
                            }
                        }
                        _ => {}
                    }
                }
                pending_events.push((event, reply));
            }
        }
    }

    /// Updates cached services and re-projects `ServicesSlice` only when the underlying data
    /// or view projection parameters changed (or on initial publication).
    fn apply_services_snapshot(&mut self, items: Vec<ServiceRecord>) -> bool {
        let unchanged = self.services_polled_once
            && !self.services_projection_dirty
            && self.current_snapshot.revisions.services > 0
            && self.cached_services == items;
        if unchanged {
            return false;
        }
        self.cached_services = items;
        self.services_polled_once = true;
        self.services_projection_dirty = false;
        let rev = self.current_snapshot.revisions.services.saturating_add(1);
        self.current_snapshot.revisions.services = rev;
        self.current_snapshot.services = Arc::new(self.projection.project_services(
            &self.cached_services,
            &self.view_config,
            rev,
        ));
        true
    }

    /// Updates cached startup entries and re-projects `StartupSlice` only when the underlying data
    /// or view projection parameters changed (or on initial publication).
    fn apply_startup_snapshot(
        &mut self,
        last_bios_time_ms: Option<u64>,
        items: Vec<StartupEntry>,
    ) -> bool {
        let unchanged = self.startup_polled_once
            && !self.startup_projection_dirty
            && self.current_snapshot.revisions.startup > 0
            && self.cached_startup_bios_ms == last_bios_time_ms
            && self.cached_startup_entries == items;
        if unchanged {
            return false;
        }
        self.cached_startup_bios_ms = last_bios_time_ms;
        self.cached_startup_entries = items;
        self.startup_polled_once = true;
        self.startup_projection_dirty = false;
        let rev = self.current_snapshot.revisions.startup.saturating_add(1);
        self.current_snapshot.revisions.startup = rev;
        self.current_snapshot.startup = Arc::new(self.projection.project_startup(
            self.cached_startup_bios_ms,
            &self.cached_startup_entries,
            &self.view_config,
            rev,
        ));
        true
    }

    fn apply_users_snapshot(&mut self, sessions: Vec<UserSessionRecord>) {
        self.cached_user_sessions = sessions;
        self.users_polled_once = true;
        self.users_projection_dirty = false;
        let rev = self.current_snapshot.revisions.users.saturating_add(1);
        self.current_snapshot.revisions.users = rev;
        self.current_snapshot.users = Arc::new(self.projection.project_users(
            &self.cached_user_sessions,
            &self.view_config,
            &self.cold_cache,
            rev,
        ));
    }

    /// Updates persisted app history and re-projects `AppHistorySlice` only when persisted data,
    /// live counters, or view projection parameters changed (or on initial publication).
    fn apply_app_history_snapshot(
        &mut self,
        since_epoch_secs: Option<u64>,
        items: Vec<AppHistoryRecord>,
    ) -> bool {
        let unchanged = self.app_history_polled_once
            && !self.app_history_projection_dirty
            && !self.app_history_dirty
            && self.current_snapshot.revisions.app_history > 0
            && self.persisted_app_history_since == since_epoch_secs
            && self.persisted_app_history == items;
        if unchanged {
            return false;
        }
        self.persisted_app_history_since = since_epoch_secs;
        self.persisted_app_history = items;
        self.app_history_polled_once = true;
        self.app_history_projection_dirty = false;
        let rev = self
            .current_snapshot
            .revisions
            .app_history
            .saturating_add(1);
        self.current_snapshot.revisions.app_history = rev;
        self.current_snapshot.app_history = Arc::new(self.projection.project_app_history(
            self.persisted_app_history_since,
            &self.persisted_app_history,
            &self.view_config,
            rev,
        ));
        true
    }
}
