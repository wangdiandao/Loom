//! Fast-lane sampling thread event loop, tick scheduling, and slow-lane probe dispatch.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, select};
use loom_pal_api::{
    AppHistoryReader, FastSystemSampler, PerfSampleScratch, PlatformBackend, ProcessSampleScratch,
};
use loom_types::{
    AppHistoryRecord, AppSnapshot, MonitorDomain, ProcessCategory, ProcessKey, RefreshSpeed,
    ServiceRecord, StartupEntry, Timestamp100Ns, UserSessionRecord,
};

use super::{EngineConfig, SliceUpdateMask, current_unix_epoch_secs};
use crate::command::{CommandEnvelope, EventBus};
use crate::lease::MonitorLeaseRegistry;
use crate::projection::{ProjectionEngine, ViewProjectionConfig};
use crate::query::ParsedQuery;
use crate::slow_lane::{
    CircuitBreaker, ColdMetaCache, SlowLanePool, SlowPriorityJob, SlowProbeJob,
};
use crate::tree::FastBuildHasher;
use crate::wake::SnapshotWakeSignal;

pub(super) struct FastLaneRunner<B: PlatformBackend> {
    pub(super) backend: Arc<B>,
    pub(super) fast_sampler: B::FastSampler,
    pub(super) slow_pool: SlowLanePool,
    pub(super) snapshot_store: Arc<ArcSwap<AppSnapshot>>,
    pub(super) wake_signal: SnapshotWakeSignal,
    pub(super) current_snapshot: AppSnapshot,
    pub(super) leases: Arc<MonitorLeaseRegistry>,
    pub(super) events: Arc<EventBus>,
    pub(super) cmd_rx: Receiver<CommandEnvelope>,
    pub(super) wake_rx: Receiver<()>,
    pub(super) config: EngineConfig,
    pub(super) view_config: ViewProjectionConfig,
    pub(super) parsed_query: ParsedQuery,
    pub(super) projection: ProjectionEngine,
    pub(super) cold_cache: ColdMetaCache,
    pub(super) circuit_breaker: CircuitBreaker,
    pub(super) pending_procs: HashSet<ProcessKey, FastBuildHasher>,
    pub(super) pending_paths: HashSet<Arc<str>, FastBuildHasher>,
    pub(super) pending_domains: [bool; MonitorDomain::COUNT],
    pub(super) failed_procs: HashMap<ProcessKey, Instant, FastBuildHasher>,
    pub(super) process_scratch: ProcessSampleScratch<B::ProcessExt>,
    pub(super) perf_scratch: PerfSampleScratch,
    pub(super) live_keys_scratch: HashSet<ProcessKey, FastBuildHasher>,
    /// Reusable prioritized probe candidate scratch `(category_rank, private_ws_bytes, key)`.
    pub(super) probe_candidates_scratch: Vec<(u8, u64, ProcessKey)>,
    pub(super) logical_cpu_count: u32,
    pub(super) refresh_speed: RefreshSpeed,
    pub(super) window_dormant: bool,
    pub(super) temp_frozen: bool,
    pub(super) process_net_enabled: bool,
    pub(super) pending_history_gap: bool,
    pub(super) last_sample_instant: Option<Instant>,
    pub(super) last_sample_timestamp: Option<Timestamp100Ns>,
    pub(super) pending_port_scan: bool,
    pub(super) last_port_scan_at: Option<Instant>,
    pub(super) tick_count: u64,
    pub(super) next_tick: Option<Instant>,
    pub(super) last_processes_projected_tick: u64,
    pub(super) last_details_projected_tick: u64,
    pub(super) last_perf_sampled_tick: u64,
    pub(super) last_per_core_sampled_tick: u64,
    pub(super) pending_cold_publish: bool,
    pub(super) cold_flush_deadline: Option<Instant>,
    pub(super) last_cold_publish_at: Option<Instant>,
    pub(super) last_domain_poll_tick: [u64; MonitorDomain::COUNT],
    pub(super) hardware_info_probed: bool,
    pub(super) pending_hardware_info: bool,
    pub(super) pending_disk_perf: bool,
    pub(super) last_disk_poll_tick: u64,
    pub(super) cached_services: Vec<ServiceRecord>,
    pub(super) services_polled_once: bool,
    pub(super) services_projection_dirty: bool,
    pub(super) cached_startup_bios_ms: Option<u64>,
    pub(super) cached_startup_entries: Vec<StartupEntry>,
    pub(super) startup_polled_once: bool,
    pub(super) startup_projection_dirty: bool,
    pub(super) cached_user_sessions: Vec<UserSessionRecord>,
    pub(super) users_polled_once: bool,
    pub(super) users_projection_dirty: bool,
    pub(super) persisted_app_history_since: Option<u64>,
    pub(super) persisted_app_history: Vec<AppHistoryRecord>,
    pub(super) app_history_polled_once: bool,
    pub(super) app_history_projection_dirty: bool,
    pub(super) app_history_dirty: bool,
    pub(super) pending_app_history_save: bool,
    pub(super) last_app_history_flush: Instant,
}

impl<B: PlatformBackend> FastLaneRunner<B> {
    pub(super) fn run(&mut self) {
        self.sync_process_network_state();

        // Tick 0: publish first interactive frame immediately before entering the event loop.
        // Skip per-process GPU discovery/D3DKMT queries on Tick 0 (GPU utilization deltas are
        // zero on the first sample anyway) and re-enable immediately for Tick 1 (+250 ms).
        self.fast_sampler.set_per_process_gpu_enabled(false);
        self.run_sample_tick();
        self.fast_sampler.set_per_process_gpu_enabled(true);
        self.schedule_after_tick(true);

        loop {
            let tick_rx = self
                .next_tick
                .map_or_else(crossbeam_channel::never, crossbeam_channel::at);
            let cold_flush_rx = self
                .cold_flush_deadline
                .map_or_else(crossbeam_channel::never, crossbeam_channel::at);
            let slow_rx = self.slow_pool.result_rx();

            select! {
                recv(self.cmd_rx) -> msg => {
                    match msg {
                        Ok(envelope) => {
                            if self.handle_command_batch(envelope) {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                recv(self.wake_rx) -> msg => {
                    if msg.is_err() {
                        break;
                    }
                    // Coalesce any buffered wakeups.
                    while self.wake_rx.try_recv().is_ok() {}
                    self.handle_lease_wakeup();
                }
                recv(slow_rx) -> msg => {
                    if let Ok(first) = msg {
                        self.handle_slow_result_batch(first);
                    }
                }
                recv(cold_flush_rx) -> _ => {
                    self.flush_pending_cold_publish();
                }
                recv(tick_rx) -> _ => {
                    if !self.window_dormant && !self.temp_frozen {
                        self.run_sample_tick();
                        self.schedule_after_tick(false);
                    } else {
                        self.next_tick = None;
                    }
                }
            }
        }

        // Stop elevated ETW per-process network session immediately on shutdown.
        if self.process_net_enabled {
            self.fast_sampler.set_per_process_network_enabled(false);
            self.process_net_enabled = false;
        }

        // Flush dirty accumulated AppHistory cleanly once on shutdown.
        self.flush_app_history_sync_on_shutdown();

        self.slow_pool.shutdown();
        self.wake_signal.close();
    }

    #[inline]
    pub(super) fn processes_projection_active(&self) -> bool {
        if self.config.gate_processes_by_lease
            || self.leases.ever_acquired(MonitorDomain::Processes)
        {
            self.leases.is_active(MonitorDomain::Processes)
        } else {
            true
        }
    }

    #[inline]
    pub(super) fn has_active_cold_consumer(&self) -> bool {
        self.processes_projection_active()
            || self.leases.is_active(MonitorDomain::Details)
            || (self.leases.is_active(MonitorDomain::Users)
                && (self.users_polled_once || !self.cached_user_sessions.is_empty()))
    }

    pub(super) fn sync_process_network_state(&mut self) {
        let desired = !self.window_dormant && self.leases.is_active(MonitorDomain::ProcessNetwork);
        if desired != self.process_net_enabled {
            self.fast_sampler.set_per_process_network_enabled(desired);
            self.process_net_enabled = desired;
        }
    }

    pub(super) fn schedule_after_tick(&mut self, use_calibration_delay: bool) {
        if self.window_dormant || self.temp_frozen {
            self.next_tick = None;
            return;
        }
        let delay = if use_calibration_delay {
            match self.refresh_speed {
                RefreshSpeed::Paused => None,
                _ => Some(self.config.startup_calibration_delay),
            }
        } else {
            self.refresh_speed.interval_ms().map(Duration::from_millis)
        };
        self.next_tick = delay.map(|d| Instant::now() + d);
    }

    pub(super) fn run_sample_tick(&mut self) {
        self.sync_process_network_state();
        self.process_scratch.clear();
        let timestamp = match self
            .fast_sampler
            .sample_processes(&mut self.process_scratch)
        {
            Ok(ts) => ts,
            Err(err) => {
                tracing::warn!(%err, "fast-lane process sampling failed");
                self.pending_history_gap = true;
                self.projection.push_performance_gap();
                return;
            }
        };

        let now = Instant::now();
        let gap_threshold_ms = self
            .refresh_speed
            .interval_ms()
            .map_or(5_000u64, |ms| ms.saturating_mul(3).max(5_000));
        let wall_gap = self.last_sample_instant.is_some_and(|prev| {
            now.saturating_duration_since(prev).as_millis() as u64 > gap_threshold_ms
        });
        let sample_ts_gap = self
            .last_sample_timestamp
            .is_some_and(|prev| timestamp.saturating_sub(prev).as_millis() > gap_threshold_ms);
        if self.pending_history_gap || wall_gap || sample_ts_gap {
            self.projection.push_performance_gap();
            self.pending_history_gap = false;
        }
        self.last_sample_instant = Some(now);
        self.last_sample_timestamp = Some(timestamp);

        self.projection.ingest_process_sample(
            timestamp,
            &self.process_scratch,
            self.logical_cpu_count,
        );
        let app_history_tick_changed = self.projection.accumulate_app_history(&self.cold_cache);
        if app_history_tick_changed {
            self.app_history_dirty = true;
        }
        self.tick_count = self.tick_count.saturating_add(1);

        // Maintain live-set caches and enqueue cold metadata probes non-blockingly.
        self.reconcile_live_caches_and_enqueue_probes(now);

        // Update zero-syscall in-memory leak heuristic analyzer on its coarse cadence.
        self.projection.update_leak_detector(now, &self.cold_cache);

        // Refresh on-demand `:PORT` listening socket table ONLY when a port query is active.
        self.maybe_enqueue_listening_ports(now);

        // Periodically persist dirty AppHistory on the slow lane while AppHistory is leased.
        self.maybe_flush_app_history(now);

        // Sample full performance metrics only when Performance or PerCoreCpu lease is active.
        let perf_leased = self.leases.is_active(MonitorDomain::Performance);
        let per_core_leased = self.leases.is_active(MonitorDomain::PerCoreCpu);
        if perf_leased || per_core_leased {
            self.sample_full_performance(per_core_leased);
        } else {
            self.projection.record_performance(None, None);
        }

        // Trigger on-demand slow-domain refreshes and slow-lane performance probes for leased domains.
        self.enqueue_performance_slow_probes();
        self.enqueue_leased_slow_domains();

        // Clear any pending coalesced cold-metadata publish because this sample tick projects fresh data.
        self.pending_cold_publish = false;
        self.cold_flush_deadline = None;

        // Project and publish snapshot, updating Processes/Details/Users/AppHistory ONLY when active.
        let processes_active = self.processes_projection_active()
            || (self.tick_count == 1 && self.config.project_processes_on_boot);
        let details_leased = self.leases.is_active(MonitorDomain::Details);
        let users_active = self.leases.is_active(MonitorDomain::Users)
            && (self.users_polled_once || !self.cached_user_sessions.is_empty());
        let app_history_active = self.leases.is_active(MonitorDomain::AppHistory)
            && self.app_history_polled_once
            && (app_history_tick_changed
                || self.app_history_projection_dirty
                || self.current_snapshot.revisions.app_history == 0);
        self.publish_slices(SliceUpdateMask {
            processes: processes_active,
            details: details_leased,
            performance: true,
            include_per_core: per_core_leased,
            users: users_active,
            app_history: app_history_active,
            ..SliceUpdateMask::default()
        });
    }

    pub(super) fn maybe_enqueue_listening_ports(&mut self, now: Instant) {
        if !self.parsed_query.has_port_predicate() || self.pending_port_scan {
            return;
        }
        let cache_stale = self.last_port_scan_at.is_none_or(|last| {
            now.saturating_duration_since(last) >= self.config.port_scan_cache_ttl
        });
        if cache_stale
            && self
                .slow_pool
                .try_enqueue_priority(SlowPriorityJob::PollListeningPorts)
                .is_ok()
        {
            self.pending_port_scan = true;
        }
    }

    fn prepare_app_history_flush(&mut self) -> (u64, Vec<AppHistoryRecord>) {
        let since = self
            .persisted_app_history_since
            .unwrap_or_else(current_unix_epoch_secs);
        let merged = self
            .projection
            .project_app_history(
                Some(since),
                &self.persisted_app_history,
                &self.view_config,
                0,
            )
            .items;
        self.persisted_app_history_since = Some(since);
        self.persisted_app_history = merged.clone();
        self.projection.clear_app_history();
        self.app_history_dirty = false;
        (since, merged)
    }

    fn maybe_flush_app_history(&mut self, now: Instant) {
        if !self.app_history_dirty
            || self.pending_app_history_save
            || !self.leases.is_active(MonitorDomain::AppHistory)
        {
            return;
        }
        if now.saturating_duration_since(self.last_app_history_flush)
            < self.config.app_history_flush_interval
        {
            return;
        }
        let (since_epoch_secs, records) = self.prepare_app_history_flush();
        self.last_app_history_flush = now;
        match self
            .slow_pool
            .try_enqueue_priority(SlowPriorityJob::SaveAppHistory {
                since_epoch_secs,
                records,
            }) {
            Ok(()) => {
                self.pending_app_history_save = true;
            }
            Err(_) => {
                // Retry on a subsequent tick if slow-lane queue was momentarily saturated.
                self.app_history_dirty = true;
            }
        }
    }

    fn flush_app_history_sync_on_shutdown(&mut self) {
        if !self.app_history_dirty {
            return;
        }
        let (since_epoch_secs, records) = self.prepare_app_history_flush();
        if let Err(err) = self
            .backend
            .history_reader()
            .save_history(since_epoch_secs, &records)
        {
            tracing::warn!(%err, "failed to flush AppHistory on shutdown");
        }
    }

    pub(super) fn sample_full_performance(&mut self, include_per_core: bool) {
        self.perf_scratch.clear();
        match self.fast_sampler.sample_performance(&mut self.perf_scratch) {
            Ok(ts) => {
                let mut sample = self.perf_scratch.sample;
                if sample.timestamp == Timestamp100Ns::default() {
                    sample.timestamp = ts;
                }
                let per_core = if include_per_core {
                    Some(self.perf_scratch.per_core_permille.as_slice())
                } else {
                    None
                };
                self.projection.record_performance(Some(sample), per_core);
                self.projection
                    .record_network_and_gpu(&self.perf_scratch.networks, &self.perf_scratch.gpus);
                self.last_perf_sampled_tick = self.tick_count;
                if include_per_core {
                    self.last_per_core_sampled_tick = self.tick_count;
                }
            }
            Err(err) => {
                tracing::warn!(%err, "fast-lane performance sampling failed");
                self.projection.push_performance_gap();
            }
        }
    }

    pub(super) fn enqueue_performance_slow_probes(&mut self) {
        if !self.leases.is_active(MonitorDomain::Performance) {
            return;
        }
        if !self.hardware_info_probed
            && !self.pending_hardware_info
            && self
                .slow_pool
                .try_enqueue_priority(SlowPriorityJob::ProbeHardwareStaticInfo)
                .is_ok()
        {
            self.pending_hardware_info = true;
        }
        if !self.pending_disk_perf
            && self.last_disk_poll_tick != self.tick_count
            && self
                .slow_pool
                .try_enqueue_priority(SlowPriorityJob::PollDiskPerformance)
                .is_ok()
        {
            self.pending_disk_perf = true;
            self.last_disk_poll_tick = self.tick_count;
        }
    }

    fn reconcile_live_caches_and_enqueue_probes(&mut self, now: Instant) {
        self.live_keys_scratch.clear();
        for key in self.projection.live_keys() {
            self.live_keys_scratch.insert(key);
        }

        self.cold_cache
            .retain_live_processes_with(&self.live_keys_scratch);
        self.pending_procs
            .retain(|k| self.live_keys_scratch.contains(k));
        self.failed_procs
            .retain(|k, retry_at| self.live_keys_scratch.contains(k) && *retry_at > now);
        self.view_config
            .process_expanded
            .retain(|k| self.live_keys_scratch.contains(k));
        self.view_config
            .details_expanded
            .retain(|k| self.live_keys_scratch.contains(k));

        self.probe_candidates_scratch.clear();
        for (key, category, private_ws_bytes) in self.projection.live_probe_candidates() {
            if self.cold_cache.get_process(key).is_some()
                || self.pending_procs.contains(&key)
                || self.failed_procs.contains_key(&key)
            {
                continue;
            }
            let (tier, weight) = if category == ProcessCategory::App {
                (0u8, private_ws_bytes)
            } else {
                (1u8, 0u64)
            };
            self.probe_candidates_scratch.push((tier, weight, key));
        }

        // Prioritize foreground `App` processes (descending by private working set) so visible
        // top rows backfill icons and descriptions first while preserving deterministic relative
        // order across background and system processes.
        self.probe_candidates_scratch
            .sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));

        for &(_, _, key) in &self.probe_candidates_scratch {
            if self
                .slow_pool
                .try_enqueue_probe(SlowProbeJob::ProbeProcess(key))
            {
                self.pending_procs.insert(key);
            } else {
                // Bounded slow queue is full; remaining processes will be probed on later ticks.
                break;
            }
        }
    }

    pub(super) fn enqueue_leased_slow_domains(&mut self) {
        const SLOW_DOMAINS: [MonitorDomain; 4] = [
            MonitorDomain::Services,
            MonitorDomain::Startup,
            MonitorDomain::Users,
            MonitorDomain::AppHistory,
        ];

        for domain in SLOW_DOMAINS {
            let idx = domain.index();
            if domain == MonitorDomain::AppHistory && self.pending_app_history_save {
                continue;
            }
            if self.leases.is_active(domain)
                && !self.pending_domains[idx]
                && self.last_domain_poll_tick[idx] != self.tick_count
                && self
                    .slow_pool
                    .try_enqueue_priority(SlowPriorityJob::PollOnDemandDomain(domain))
                    .is_ok()
            {
                self.pending_domains[idx] = true;
                self.last_domain_poll_tick[idx] = self.tick_count;
            }
        }
    }
}
