//! Headless dual-lane [`EngineCoordinator`] orchestrating fast-lane sampling,
//! slow-lane metadata probing, command execution, and lock-free snapshot publishing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, TrySendError, select};
use loom_config::AppSettings;
use loom_pal_api::{
    FastSystemSampler, PalError, PerfSampleScratch, PlatformBackend, ProcessSampleScratch,
    SystemOperator,
};
use loom_types::{
    AppHistoryRecord, AppSnapshot, MonitorDomain, ProcessKey, RefreshSpeed, ServicesSlice,
    StartupSlice, UserSessionRecord,
};
use parking_lot::Mutex;

use crate::command::{
    CommandEnvelope, CommandEvent, CommandKind, CommandOutcome, CommandSuccess, EngineCommand,
    EngineCommandError, EventBus,
};
use crate::lease::{MonitorLease, MonitorLeaseRegistry};
use crate::projection::{ProjectionEngine, ViewProjectionConfig};
use crate::slow_lane::{
    CircuitBreaker, ColdMetaCache, MgmtDomainRefresh, SlowLanePool, SlowMgmtOp, SlowPriorityJob,
    SlowProbeJob, SlowResult, SlowShellOp,
};
use crate::tree::collect_bottom_up_kill_order;

/// Retry cooldown before re-probing a process whose metadata probe failed (e.g., protected/denied).
const FAILED_PROBE_RETRY_COOLDOWN: Duration = Duration::from_secs(10);

/// Maximum number of slow-lane results drained per fast-lane wakeup batch.
const SLOW_RESULT_DRAIN_BATCH: usize = 128;

/// Tunable configuration parameters for [`EngineCoordinator`].
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Delay between Tick 0 (immediate first frame) and Tick 1 (CPU/I/O calibration tick).
    ///
    /// Defaults to `250 ms`.
    pub startup_calibration_delay: Duration,
    /// Number of slow-lane bulkhead worker threads (clamped to `1..=2`).
    ///
    /// Defaults to `2`.
    pub slow_workers: usize,
    /// Bounded queue capacity for background slow-lane metadata probes.
    ///
    /// Defaults to `256`.
    pub slow_queue_capacity: usize,
    /// Slow-probe latency threshold beyond which the engine-level [`CircuitBreaker`] trips.
    ///
    /// Defaults to `300 ms`.
    pub slow_probe_timeout: Duration,
    /// Cooldown duration while a tripped slow-lane circuit breaker remains open.
    ///
    /// Defaults to `60 s`.
    pub slow_circuit_cooldown: Duration,
    /// Maximum time to wait for slow-lane workers to exit during shutdown before detaching
    /// workers stuck in hung OS/disk calls.
    ///
    /// Defaults to `100 ms`.
    pub slow_shutdown_timeout: Duration,
    /// Bounded capacity of the incoming [`EngineCommand`] channel.
    ///
    /// Defaults to `128`.
    pub command_queue_capacity: usize,
    /// Bounded capacity per [`CommandEvent`] subscriber channel.
    ///
    /// Defaults to `64`.
    pub event_channel_capacity: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            startup_calibration_delay: Duration::from_millis(250),
            slow_workers: 2,
            slow_queue_capacity: 256,
            slow_probe_timeout: Duration::from_millis(300),
            slow_circuit_cooldown: Duration::from_secs(60),
            slow_shutdown_timeout: Duration::from_millis(100),
            command_queue_capacity: 128,
            event_channel_capacity: 64,
        }
    }
}

/// Headless engine coordinator parameterized statically over a [`PlatformBackend`].
///
/// Owns at most **3** background threads:
/// - `1` dedicated fast-lane sampling and snapshot projection thread (`loom-fast-sampler`).
/// - `1..=2` slow-lane bulkhead worker threads (`loom-slow-*`) for disk/shell/PE probes.
///
/// Publishes domain-sliced [`AppSnapshot`] instances lock-free via [`ArcSwap`].
pub struct EngineCoordinator<B: PlatformBackend> {
    backend: Arc<B>,
    settings: AppSettings,
    snapshot: Arc<ArcSwap<AppSnapshot>>,
    leases: Arc<MonitorLeaseRegistry>,
    cmd_tx: Sender<CommandEnvelope>,
    events: Arc<EventBus>,
    event_channel_capacity: usize,
    fast_handle: Mutex<Option<JoinHandle<()>>>,
    shutdown_sent: AtomicBool,
}

impl<B: PlatformBackend> EngineCoordinator<B> {
    /// Initializes an [`EngineCoordinator`] with production [`EngineConfig`] defaults
    /// (`250 ms` startup calibration delay, `2` slow-lane workers, `300 ms` breaker).
    pub fn new(backend: B, settings: AppSettings) -> Result<Self, PalError> {
        Self::with_config(backend, settings, EngineConfig::default())
    }

    /// Initializes an [`EngineCoordinator`] with explicit [`EngineConfig`] parameters.
    pub fn with_config(
        backend: B,
        settings: AppSettings,
        config: EngineConfig,
    ) -> Result<Self, PalError> {
        let backend = Arc::new(backend);
        let fast_sampler = backend.create_fast_sampler()?;
        let slow_prober = Arc::new(backend.create_slow_prober()?);

        let initial_snapshot = AppSnapshot {
            platform: B::PLATFORM,
            capabilities: B::CAPABILITIES,
            is_elevated: backend.operator().is_elevated(),
            ..AppSnapshot::default()
        };
        let snapshot = Arc::new(ArcSwap::from_pointee(initial_snapshot.clone()));

        let cmd_cap = config.command_queue_capacity.max(16);
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded::<CommandEnvelope>(cmd_cap);
        let (wake_tx, wake_rx) = crossbeam_channel::bounded::<()>(1);
        let leases = Arc::new(MonitorLeaseRegistry::with_wakeup(wake_tx));
        let events = Arc::new(EventBus::new());

        let slow_pool = SlowLanePool::spawn(
            Arc::clone(&backend),
            slow_prober,
            Arc::clone(&leases),
            Arc::clone(&events),
            config.slow_workers,
            config.slow_queue_capacity,
            config.slow_shutdown_timeout,
        )?;

        let logical_cpu_count = backend.operator().logical_cpu_count().max(1);
        let view_config = ViewProjectionConfig {
            process_view_mode: settings.process_view_mode,
            ..ViewProjectionConfig::default()
        };
        let refresh_speed = settings.refresh_speed;
        let event_channel_capacity = config.event_channel_capacity;

        let runner_backend = Arc::clone(&backend);
        let runner_snapshot = Arc::clone(&snapshot);
        let runner_leases = Arc::clone(&leases);
        let runner_events = Arc::clone(&events);

        let fast_handle = thread::Builder::new()
            .name("loom-fast-sampler".to_owned())
            .spawn(move || {
                let mut runner = FastLaneRunner {
                    backend: runner_backend,
                    fast_sampler,
                    slow_pool,
                    snapshot_store: runner_snapshot,
                    current_snapshot: initial_snapshot,
                    leases: runner_leases,
                    events: runner_events,
                    cmd_rx,
                    wake_rx,
                    circuit_breaker: CircuitBreaker::new(
                        config.slow_probe_timeout,
                        config.slow_circuit_cooldown,
                    ),
                    config,
                    view_config,
                    projection: ProjectionEngine::with_platform(B::PLATFORM),
                    cold_cache: ColdMetaCache::new(),
                    pending_procs: HashSet::with_capacity(256),
                    pending_paths: HashSet::with_capacity(64),
                    pending_domains: [false; MonitorDomain::COUNT],
                    failed_procs: HashMap::new(),
                    process_scratch: ProcessSampleScratch::with_capacity(512, 16 * 1024),
                    perf_scratch: PerfSampleScratch::new(),
                    live_keys_scratch: HashSet::with_capacity(512),
                    logical_cpu_count,
                    refresh_speed,
                    window_dormant: false,
                    tick_count: 0,
                    next_tick: None,
                    last_details_projected_tick: 0,
                    last_perf_sampled_tick: 0,
                    last_per_core_sampled_tick: 0,
                    last_domain_poll_tick: [0; MonitorDomain::COUNT],
                    hardware_info_probed: false,
                    pending_hardware_info: false,
                    pending_disk_perf: false,
                    last_disk_poll_tick: 0,
                    cached_user_sessions: Vec::new(),
                    persisted_app_history_since: None,
                    persisted_app_history: Vec::new(),
                    app_history_polled_once: false,
                };
                runner.run();
            })
            .map_err(PalError::Io)?;

        Ok(Self {
            backend,
            settings,
            snapshot,
            leases,
            cmd_tx,
            events,
            event_channel_capacity,
            fast_handle: Mutex::new(Some(fast_handle)),
            shutdown_sent: AtomicBool::new(false),
        })
    }

    /// Loads the latest lock-free immutable [`AppSnapshot`].
    #[inline]
    pub fn snapshot(&self) -> Arc<AppSnapshot> {
        self.snapshot.load_full()
    }

    /// Returns a cloned [`Arc`] handle to the underlying lock-free [`ArcSwap<AppSnapshot>`] store.
    #[inline]
    pub fn snapshot_store(&self) -> Arc<ArcSwap<AppSnapshot>> {
        Arc::clone(&self.snapshot)
    }

    /// Returns a reference to the initial [`AppSettings`].
    #[inline]
    pub fn settings(&self) -> &AppSettings {
        &self.settings
    }

    /// Returns a reference to the statically bound [`PlatformBackend`].
    #[inline]
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Acquires an RAII [`MonitorLease`] keeping `domain` active while held.
    #[inline]
    pub fn acquire_lease(&self, domain: MonitorDomain) -> MonitorLease {
        self.leases.acquire(domain)
    }

    /// Returns a reference to the shared [`MonitorLeaseRegistry`].
    #[inline]
    pub fn leases(&self) -> &Arc<MonitorLeaseRegistry> {
        &self.leases
    }

    /// Submits an [`EngineCommand`] non-blockingly to the coordinator.
    pub fn send_command(&self, command: EngineCommand) -> Result<(), EngineCommandError> {
        self.enqueue_envelope(CommandEnvelope {
            command,
            reply: None,
        })
    }

    /// Submits an [`EngineCommand`] and returns a oneshot [`Receiver<CommandEvent>`]
    /// delivering its completion outcome.
    pub fn send_command_with_reply(
        &self,
        command: EngineCommand,
    ) -> Result<Receiver<CommandEvent>, EngineCommandError> {
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        self.enqueue_envelope(CommandEnvelope {
            command,
            reply: Some(reply_tx),
        })?;
        Ok(reply_rx)
    }

    /// Subscribes to the engine's [`CommandEvent`] feedback stream (including
    /// structured [`PalError::ElevationRequired`] notifications for UAC prompts).
    pub fn subscribe_events(&self) -> Receiver<CommandEvent> {
        self.events.subscribe(self.event_channel_capacity)
    }

    /// Shuts down the fast-lane and slow-lane background threads cleanly.
    ///
    /// Safe to invoke multiple times; subsequent calls are no-ops.
    pub fn shutdown(&self) {
        if !self.shutdown_sent.swap(true, Ordering::AcqRel) {
            // Try non-blocking first; if the bounded queue is full, send blocking briefly.
            let env = CommandEnvelope {
                command: EngineCommand::Shutdown,
                reply: None,
            };
            if let Err(TrySendError::Full(env)) = self.cmd_tx.try_send(env) {
                let _ = self.cmd_tx.send_timeout(env, Duration::from_millis(50));
            }
        }
        if let Some(handle) = self.fast_handle.lock().take() {
            let _ = handle.join();
        }
    }

    fn enqueue_envelope(&self, envelope: CommandEnvelope) -> Result<(), EngineCommandError> {
        match self.cmd_tx.try_send(envelope) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(EngineCommandError::QueueFull),
            Err(TrySendError::Disconnected(_)) => Err(EngineCommandError::Disconnected),
        }
    }
}

impl<B: PlatformBackend> Drop for EngineCoordinator<B> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct FastLaneRunner<B: PlatformBackend> {
    backend: Arc<B>,
    fast_sampler: B::FastSampler,
    slow_pool: SlowLanePool,
    snapshot_store: Arc<ArcSwap<AppSnapshot>>,
    current_snapshot: AppSnapshot,
    leases: Arc<MonitorLeaseRegistry>,
    events: Arc<EventBus>,
    cmd_rx: Receiver<CommandEnvelope>,
    wake_rx: Receiver<()>,
    config: EngineConfig,
    view_config: ViewProjectionConfig,
    projection: ProjectionEngine,
    cold_cache: ColdMetaCache,
    circuit_breaker: CircuitBreaker,
    pending_procs: HashSet<ProcessKey>,
    pending_paths: HashSet<Arc<str>>,
    pending_domains: [bool; MonitorDomain::COUNT],
    failed_procs: HashMap<ProcessKey, Instant>,
    process_scratch: ProcessSampleScratch<B::ProcessExt>,
    perf_scratch: PerfSampleScratch,
    live_keys_scratch: HashSet<ProcessKey>,
    logical_cpu_count: u32,
    refresh_speed: RefreshSpeed,
    window_dormant: bool,
    tick_count: u64,
    next_tick: Option<Instant>,
    last_details_projected_tick: u64,
    last_perf_sampled_tick: u64,
    last_per_core_sampled_tick: u64,
    last_domain_poll_tick: [u64; MonitorDomain::COUNT],
    hardware_info_probed: bool,
    pending_hardware_info: bool,
    pending_disk_perf: bool,
    last_disk_poll_tick: u64,
    cached_user_sessions: Vec<UserSessionRecord>,
    persisted_app_history_since: Option<u64>,
    persisted_app_history: Vec<AppHistoryRecord>,
    app_history_polled_once: bool,
}

impl<B: PlatformBackend> FastLaneRunner<B> {
    fn run(&mut self) {
        // Tick 0: publish first interactive frame immediately before entering the event loop.
        self.run_sample_tick();
        self.schedule_after_tick(true);

        loop {
            let tick_rx = self
                .next_tick
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
                recv(tick_rx) -> _ => {
                    if !self.window_dormant {
                        self.run_sample_tick();
                        self.schedule_after_tick(false);
                    } else {
                        self.next_tick = None;
                    }
                }
            }
        }

        self.slow_pool.shutdown();
    }

    fn schedule_after_tick(&mut self, use_calibration_delay: bool) {
        if self.window_dormant {
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

    fn run_sample_tick(&mut self) {
        self.process_scratch.clear();
        let timestamp = match self
            .fast_sampler
            .sample_processes(&mut self.process_scratch)
        {
            Ok(ts) => ts,
            Err(err) => {
                tracing::warn!(%err, "fast-lane process sampling failed");
                return;
            }
        };

        self.projection.ingest_process_sample(
            timestamp,
            &self.process_scratch,
            self.logical_cpu_count,
        );
        self.projection.accumulate_app_history(&self.cold_cache);
        self.tick_count = self.tick_count.saturating_add(1);

        // Maintain live-set caches and enqueue cold metadata probes non-blockingly.
        self.reconcile_live_caches_and_enqueue_probes();

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

        // Project and publish snapshot, updating Details ONLY when leased.
        let details_leased = self.leases.is_active(MonitorDomain::Details);
        self.publish_slices(true, details_leased, true, per_core_leased);
    }

    fn sample_full_performance(&mut self, include_per_core: bool) {
        self.perf_scratch.clear();
        match self.fast_sampler.sample_performance(&mut self.perf_scratch) {
            Ok(ts) => {
                let mut sample = self.perf_scratch.sample;
                if sample.timestamp == loom_types::Timestamp100Ns::default() {
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
                self.projection.record_performance(None, None);
            }
        }
    }

    fn enqueue_performance_slow_probes(&mut self) {
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

    fn reconcile_live_caches_and_enqueue_probes(&mut self) {
        let now = Instant::now();
        self.live_keys_scratch.clear();
        for key in self.projection.live_keys() {
            self.live_keys_scratch.insert(key);
        }

        self.cold_cache
            .retain_live_processes(&self.live_keys_scratch);
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

        for key in self.projection.live_keys() {
            if self.cold_cache.get_process(key).is_some()
                || self.pending_procs.contains(&key)
                || self.failed_procs.contains_key(&key)
            {
                continue;
            }
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

    fn enqueue_leased_slow_domains(&mut self) {
        const SLOW_DOMAINS: [MonitorDomain; 4] = [
            MonitorDomain::Services,
            MonitorDomain::Startup,
            MonitorDomain::Users,
            MonitorDomain::AppHistory,
        ];

        for domain in SLOW_DOMAINS {
            let idx = domain.index();
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

    fn handle_lease_wakeup(&mut self) {
        if self.window_dormant || self.tick_count == 0 {
            return;
        }

        let details_leased = self.leases.is_active(MonitorDomain::Details);
        let need_details = details_leased
            && (self.current_snapshot.revisions.details == 0
                || self.last_details_projected_tick != self.tick_count);

        let perf_leased = self.leases.is_active(MonitorDomain::Performance);
        let per_core_leased = self.leases.is_active(MonitorDomain::PerCoreCpu);
        let need_perf = ((perf_leased || per_core_leased)
            && self.last_perf_sampled_tick != self.tick_count)
            || (per_core_leased && self.last_per_core_sampled_tick != self.tick_count);

        if need_perf {
            self.sample_full_performance(per_core_leased);
        }

        if need_details || need_perf {
            self.publish_slices(false, need_details, need_perf, per_core_leased);
        }

        self.enqueue_performance_slow_probes();
        self.enqueue_leased_slow_domains();
    }

    fn handle_slow_result_batch(&mut self, first: SlowResult) {
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

        if !self.window_dormant {
            if cold_updated || perf_updated {
                let details_leased = cold_updated && self.leases.is_active(MonitorDomain::Details);
                let per_core_leased =
                    perf_updated && self.leases.is_active(MonitorDomain::PerCoreCpu);
                self.publish_slices(cold_updated, details_leased, perf_updated, per_core_leased);
            } else if snapshot_dirty {
                self.snapshot_store
                    .store(Arc::new(self.current_snapshot.clone()));
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
            SlowResult::ServicesPolled(result) => {
                self.pending_domains[MonitorDomain::Services.index()] = false;
                if let Ok(items) = result
                    && self.leases.is_active(MonitorDomain::Services)
                {
                    self.apply_services_snapshot(items);
                    *snapshot_dirty = true;
                }
            }
            SlowResult::StartupPolled(result) => {
                self.pending_domains[MonitorDomain::Startup.index()] = false;
                if let Ok((last_bios_time_ms, items)) = result
                    && self.leases.is_active(MonitorDomain::Startup)
                {
                    self.apply_startup_snapshot(last_bios_time_ms, items);
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
                {
                    self.apply_app_history_snapshot(since_epoch_secs, items);
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
                    if refresh.is_none() && self.leases.is_active(MonitorDomain::AppHistory) {
                        let since = self.persisted_app_history_since;
                        self.apply_app_history_snapshot(since, Vec::new());
                        *snapshot_dirty = true;
                    }
                }
                if let Some(domain_refresh) = refresh {
                    match domain_refresh {
                        MgmtDomainRefresh::Services(Ok(items))
                            if self.leases.is_active(MonitorDomain::Services) =>
                        {
                            self.apply_services_snapshot(items);
                            *snapshot_dirty = true;
                        }
                        MgmtDomainRefresh::Startup(Ok((bios_ms, items)))
                            if self.leases.is_active(MonitorDomain::Startup) =>
                        {
                            self.apply_startup_snapshot(bios_ms, items);
                            *snapshot_dirty = true;
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
                            self.apply_app_history_snapshot(since, items);
                            *snapshot_dirty = true;
                        }
                        _ => {}
                    }
                }
                pending_events.push((event, reply));
            }
        }
    }

    fn apply_services_snapshot(&mut self, items: Vec<loom_types::ServiceRecord>) {
        let rev = self.current_snapshot.revisions.services.saturating_add(1);
        self.current_snapshot.revisions.services = rev;
        self.current_snapshot.services = Arc::new(ServicesSlice {
            revision: rev,
            timestamp: self.projection.latest_timestamp(),
            items,
        });
    }

    fn apply_startup_snapshot(
        &mut self,
        last_bios_time_ms: Option<u64>,
        items: Vec<loom_types::StartupEntry>,
    ) {
        let rev = self.current_snapshot.revisions.startup.saturating_add(1);
        self.current_snapshot.revisions.startup = rev;
        self.current_snapshot.startup = Arc::new(StartupSlice {
            revision: rev,
            timestamp: self.projection.latest_timestamp(),
            last_bios_time_ms,
            items,
        });
    }

    fn apply_users_snapshot(&mut self, sessions: Vec<UserSessionRecord>) {
        self.cached_user_sessions = sessions;
        let rev = self.current_snapshot.revisions.users.saturating_add(1);
        self.current_snapshot.revisions.users = rev;
        self.current_snapshot.users = Arc::new(self.projection.project_users(
            &self.cached_user_sessions,
            &self.cold_cache,
            rev,
        ));
    }

    fn apply_app_history_snapshot(
        &mut self,
        since_epoch_secs: Option<u64>,
        items: Vec<AppHistoryRecord>,
    ) {
        self.persisted_app_history_since = since_epoch_secs;
        self.persisted_app_history = items;
        self.app_history_polled_once = true;
        let rev = self
            .current_snapshot
            .revisions
            .app_history
            .saturating_add(1);
        self.current_snapshot.revisions.app_history = rev;
        self.current_snapshot.app_history = Arc::new(self.projection.project_app_history(
            self.persisted_app_history_since,
            &self.persisted_app_history,
            rev,
        ));
    }

    /// Processes the received command plus any additional queued commands in one batch,
    /// coalescing repeated view-state changes into a single re-projection.
    ///
    /// Returns `true` if `EngineCommand::Shutdown` was received.
    fn handle_command_batch(&mut self, first: CommandEnvelope) -> bool {
        let mut need_reproject_processes = false;
        let mut need_reproject_details = false;
        let mut need_sample_tick = false;
        let mut pending_acks: Vec<(CommandKind, Sender<CommandEvent>)> = Vec::new();
        let mut pending_events: Vec<(CommandEvent, Option<Sender<CommandEvent>>)> = Vec::new();
        let mut shutdown_requested = false;

        if self.apply_single_command(
            first,
            &mut need_reproject_processes,
            &mut need_reproject_details,
            &mut need_sample_tick,
            &mut pending_acks,
            &mut pending_events,
        ) {
            shutdown_requested = true;
        } else {
            while let Ok(next) = self.cmd_rx.try_recv() {
                if self.apply_single_command(
                    next,
                    &mut need_reproject_processes,
                    &mut need_reproject_details,
                    &mut need_sample_tick,
                    &mut pending_acks,
                    &mut pending_events,
                ) {
                    shutdown_requested = true;
                    break;
                }
            }
        }

        if !shutdown_requested {
            if need_sample_tick {
                self.run_sample_tick();
                self.schedule_after_tick(self.tick_count <= 1);
            } else if !self.window_dormant && (need_reproject_processes || need_reproject_details) {
                let details_dirty =
                    need_reproject_details && self.leases.is_active(MonitorDomain::Details);
                if need_reproject_processes || details_dirty {
                    self.publish_slices(need_reproject_processes, details_dirty, false, false);
                }
            }
        }

        for (event, reply_tx) in pending_events {
            self.events.emit(event, reply_tx);
        }

        for (kind, reply_tx) in pending_acks {
            let _ = reply_tx.try_send(CommandEvent::new(
                kind,
                None,
                CommandOutcome::Ok(CommandSuccess::Ack),
            ));
        }

        shutdown_requested
    }

    fn apply_single_command(
        &mut self,
        envelope: CommandEnvelope,
        need_reproject_processes: &mut bool,
        need_reproject_details: &mut bool,
        need_sample_tick: &mut bool,
        pending_acks: &mut Vec<(CommandKind, Sender<CommandEvent>)>,
        pending_events: &mut Vec<(CommandEvent, Option<Sender<CommandEvent>>)>,
    ) -> bool {
        let kind = envelope.command.kind();
        let reply = envelope.reply;

        match envelope.command {
            EngineCommand::Shutdown => {
                if let Some(reply_tx) = reply {
                    pending_acks.push((kind, reply_tx));
                }
                return true;
            }
            EngineCommand::SetRefreshSpeed(speed) => {
                self.refresh_speed = speed;
                self.schedule_after_tick(false);
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetSearchQuery(query) => {
                if self.view_config.search_query != query {
                    self.view_config.search_query = query;
                    *need_reproject_processes = true;
                    *need_reproject_details = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetProcessViewMode(mode) => {
                if self.view_config.process_view_mode != mode {
                    self.view_config.process_view_mode = mode;
                    *need_reproject_processes = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetProcessSort { column, direction } => {
                if self.view_config.process_sort_column != column
                    || self.view_config.process_sort_direction != direction
                {
                    self.view_config.process_sort_column = column;
                    self.view_config.process_sort_direction = direction;
                    *need_reproject_processes = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::ToggleProcessExpanded(key) => {
                if !self.view_config.process_expanded.remove(&key) {
                    self.view_config.process_expanded.insert(key);
                }
                *need_reproject_processes = true;
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetDetailsViewMode(mode) => {
                if self.view_config.details_view_mode != mode {
                    self.view_config.details_view_mode = mode;
                    *need_reproject_details = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetDetailsSort { column, direction } => {
                if self.view_config.details_sort_column != column
                    || self.view_config.details_sort_direction != direction
                {
                    self.view_config.details_sort_column = column;
                    self.view_config.details_sort_direction = direction;
                    *need_reproject_details = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::ToggleDetailsExpanded(key) => {
                if !self.view_config.details_expanded.remove(&key) {
                    self.view_config.details_expanded.insert(key);
                }
                *need_reproject_details = true;
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetWindowDormant(dormant) => {
                let was_dormant = self.window_dormant;
                self.window_dormant = dormant;
                if dormant {
                    self.next_tick = None;
                    *need_sample_tick = false;
                } else if was_dormant {
                    *need_sample_tick = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::ForceRefresh => {
                *need_sample_tick = true;
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::TerminateProcess(key) => {
                let outcome = match self.backend.operator().terminate_process(key) {
                    Ok(()) => CommandOutcome::Ok(CommandSuccess::Ack),
                    Err(err) => CommandOutcome::err(err),
                };
                self.events
                    .emit(CommandEvent::new(kind, Some(key), outcome), reply);
            }
            EngineCommand::TerminateProcessTree(root_key) => {
                let targets = collect_bottom_up_kill_order(self.projection.latest_tree(), root_key);
                let outcome = match self.backend.operator().terminate_process_tree(&targets) {
                    Ok(report) => CommandOutcome::Ok(CommandSuccess::ProcessTreeTerminated(report)),
                    Err(err) => CommandOutcome::err(err),
                };
                self.events
                    .emit(CommandEvent::new(kind, Some(root_key), outcome), reply);
            }
            EngineCommand::SetEfficiencyMode { key, enabled } => {
                match self.backend.operator().set_efficiency_mode(key, enabled) {
                    Ok(()) => {
                        self.cold_cache.set_efficiency_mode(key, enabled);
                        *need_reproject_processes = true;
                        *need_reproject_details = true;
                        pending_events.push((
                            CommandEvent::new(
                                kind,
                                Some(key),
                                CommandOutcome::Ok(CommandSuccess::Ack),
                            ),
                            reply,
                        ));
                    }
                    Err(err) => {
                        self.events.emit(
                            CommandEvent::new(kind, Some(key), CommandOutcome::err(err)),
                            reply,
                        );
                    }
                }
            }
            EngineCommand::SetPriority { key, priority } => {
                match self.backend.operator().set_priority(key, priority) {
                    Ok(()) => {
                        *need_sample_tick = true;
                        pending_events.push((
                            CommandEvent::new(
                                kind,
                                Some(key),
                                CommandOutcome::Ok(CommandSuccess::Ack),
                            ),
                            reply,
                        ));
                    }
                    Err(err) => {
                        self.events.emit(
                            CommandEvent::new(kind, Some(key), CommandOutcome::err(err)),
                            reply,
                        );
                    }
                }
            }
            EngineCommand::GetAffinity(key) => {
                let outcome = match self.backend.operator().get_affinity(key) {
                    Ok(mask) => CommandOutcome::Ok(CommandSuccess::Affinity(mask)),
                    Err(err) => CommandOutcome::err(err),
                };
                self.events
                    .emit(CommandEvent::new(kind, Some(key), outcome), reply);
            }
            EngineCommand::SetAffinity { key, mask } => {
                let outcome = match self.backend.operator().set_affinity(key, &mask) {
                    Ok(()) => CommandOutcome::Ok(CommandSuccess::Ack),
                    Err(err) => CommandOutcome::err(err),
                };
                self.events
                    .emit(CommandEvent::new(kind, Some(key), outcome), reply);
            }
            EngineCommand::BrowseExecutable => {
                self.dispatch_slow_shell_op(kind, SlowShellOp::BrowseExecutable, reply);
            }
            EngineCommand::RunNewTask(req) => {
                self.dispatch_slow_shell_op(kind, SlowShellOp::RunNewTask(req), reply);
            }
            EngineCommand::OpenFileLocation(path) => {
                self.dispatch_slow_shell_op(kind, SlowShellOp::OpenFileLocation(path), reply);
            }
            EngineCommand::ShowFileProperties(path) => {
                self.dispatch_slow_shell_op(kind, SlowShellOp::ShowFileProperties(path), reply);
            }
            EngineCommand::SearchOnline(query) => {
                self.dispatch_slow_shell_op(kind, SlowShellOp::SearchOnline(query), reply);
            }
            EngineCommand::RelaunchElevated(route) => {
                self.dispatch_slow_shell_op(kind, SlowShellOp::RelaunchElevated(route), reply);
            }
            EngineCommand::StartService(name) => {
                self.dispatch_slow_mgmt_op(kind, SlowMgmtOp::StartService(name), reply);
            }
            EngineCommand::StopService(name) => {
                self.dispatch_slow_mgmt_op(kind, SlowMgmtOp::StopService(name), reply);
            }
            EngineCommand::RestartService(name) => {
                self.dispatch_slow_mgmt_op(kind, SlowMgmtOp::RestartService(name), reply);
            }
            EngineCommand::OpenServicesConsole => {
                self.dispatch_slow_mgmt_op(kind, SlowMgmtOp::OpenServicesConsole, reply);
            }
            EngineCommand::SetStartupEnabled { id, enabled } => {
                self.dispatch_slow_mgmt_op(
                    kind,
                    SlowMgmtOp::SetStartupEnabled { id, enabled },
                    reply,
                );
            }
            EngineCommand::DisconnectSession(session_id) => {
                self.dispatch_slow_mgmt_op(kind, SlowMgmtOp::DisconnectSession(session_id), reply);
            }
            EngineCommand::ClearAppHistory => {
                self.dispatch_slow_mgmt_op(kind, SlowMgmtOp::ClearAppHistory, reply);
            }
        }

        false
    }

    fn dispatch_slow_shell_op(
        &self,
        kind: CommandKind,
        op: SlowShellOp,
        reply: Option<Sender<CommandEvent>>,
    ) {
        if let Err(SlowPriorityJob::ShellOp { reply, .. }) = self
            .slow_pool
            .try_enqueue_priority(SlowPriorityJob::ShellOp { op, reply })
        {
            let event = CommandEvent::new(
                kind,
                None,
                CommandOutcome::err(PalError::CircuitOpen {
                    target: "slow-lane".to_owned(),
                    retry_after_ms: 100,
                }),
            );
            self.events.emit(event, reply);
        }
    }

    fn dispatch_slow_mgmt_op(
        &self,
        kind: CommandKind,
        op: SlowMgmtOp,
        reply: Option<Sender<CommandEvent>>,
    ) {
        if let Err(SlowPriorityJob::MgmtOp { reply, .. }) = self
            .slow_pool
            .try_enqueue_priority(SlowPriorityJob::MgmtOp { op, reply })
        {
            let event = CommandEvent::new(
                kind,
                None,
                CommandOutcome::err(PalError::CircuitOpen {
                    target: "slow-lane".to_owned(),
                    retry_after_ms: 100,
                }),
            );
            self.events.emit(event, reply);
        }
    }

    fn publish_slices(
        &mut self,
        update_processes: bool,
        update_details: bool,
        update_performance: bool,
        include_per_core: bool,
    ) {
        let mut next = self.current_snapshot.clone();

        if update_processes {
            let rev = next.revisions.processes.saturating_add(1);
            next.revisions.processes = rev;
            next.processes = Arc::new(self.projection.project_processes(
                &self.view_config,
                &self.cold_cache,
                rev,
            ));

            if self.leases.is_active(MonitorDomain::Users) && !self.cached_user_sessions.is_empty()
            {
                let users_rev = next.revisions.users.saturating_add(1);
                next.revisions.users = users_rev;
                next.users = Arc::new(self.projection.project_users(
                    &self.cached_user_sessions,
                    &self.cold_cache,
                    users_rev,
                ));
            }

            if self.leases.is_active(MonitorDomain::AppHistory) && self.app_history_polled_once {
                let hist_rev = next.revisions.app_history.saturating_add(1);
                next.revisions.app_history = hist_rev;
                next.app_history = Arc::new(self.projection.project_app_history(
                    self.persisted_app_history_since,
                    &self.persisted_app_history,
                    hist_rev,
                ));
            }
        }

        if update_details {
            let rev = next.revisions.details.saturating_add(1);
            next.revisions.details = rev;
            next.details = Arc::new(self.projection.project_details(
                &self.view_config,
                &self.cold_cache,
                rev,
            ));
            self.last_details_projected_tick = self.tick_count;
        }

        if update_performance {
            let rev = next.revisions.performance.saturating_add(1);
            next.revisions.performance = rev;
            next.performance = Arc::new(self.projection.project_performance(include_per_core, rev));
        }

        self.current_snapshot = next.clone();
        self.snapshot_store.store(Arc::new(next));
    }
}

#[inline]
fn push_ack(
    kind: CommandKind,
    reply: Option<Sender<CommandEvent>>,
    pending_acks: &mut Vec<(CommandKind, Sender<CommandEvent>)>,
) {
    if let Some(reply_tx) = reply {
        pending_acks.push((kind, reply_tx));
    }
}
