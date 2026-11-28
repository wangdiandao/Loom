//! Headless dual-lane [`EngineCoordinator`] orchestrating fast-lane sampling,
//! slow-lane metadata probing, command execution, and lock-free snapshot publishing.

mod commands;
mod publish;
mod runner;
mod slow_results;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use loom_config::AppSettings;
use loom_pal_api::{
    PalError, PerfSampleScratch, PlatformBackend, ProcessSampleScratch, SystemOperator,
};
use loom_types::{AppSnapshot, MonitorDomain};
use parking_lot::Mutex;

use crate::command::{
    CommandEnvelope, CommandEvent, CommandKind, CommandOutcome, CommandSuccess, EngineCommand,
    EngineCommandError, EventBus,
};
use crate::leaks::LeakDetectorConfig;
use crate::lease::{MonitorLease, MonitorLeaseRegistry};
use crate::projection::{ProjectionEngine, ViewProjectionConfig};
use crate::query::ParsedQuery;
use crate::slow_lane::{CircuitBreaker, ColdMetaCache, SlowLanePool};
use crate::tree::FastBuildHasher;
use crate::wake::SnapshotWakeSignal;

use runner::FastLaneRunner;

/// Retry cooldown before re-probing a process whose metadata probe failed (e.g., protected/denied).
pub(super) const FAILED_PROBE_RETRY_COOLDOWN: Duration = Duration::from_secs(10);

/// Maximum number of slow-lane results drained per fast-lane wakeup batch.
pub(super) const SLOW_RESULT_DRAIN_BATCH: usize = 128;

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
    /// Periodic flush cadence for dirty cumulative `AppHistory` records while leased.
    ///
    /// Defaults to `60 s`.
    pub app_history_flush_interval: Duration,
    /// Minimum cache TTL between on-demand `:PORT` listening socket scans while active.
    ///
    /// Defaults to `2 s`.
    pub port_scan_cache_ttl: Duration,
    /// Timing thresholds for the pure in-memory [`crate::LeakDetector`].
    ///
    /// Defaults to `30 s` check cadence and `10 min` sustain window.
    pub leak_detector: LeakDetectorConfig,
    /// When `true`, `ProcessTableSlice` projection is strictly gated by
    /// `MonitorDomain::Processes` RAII leases (zero projection work while away from the
    /// Processes tab and tray quick-overview).
    ///
    /// Defaults to `false` so headless callers and tests observe Processes snapshots without
    /// acquiring an explicit lease; acquiring `MonitorDomain::Processes` once also activates
    /// lease-based gating automatically.
    pub gate_processes_by_lease: bool,
    /// When `true`, Tick 0 still projects an initial `ProcessTableSlice` frame during startup
    /// even when [`Self::gate_processes_by_lease`] is enabled, keeping first-frame latency
    /// minimal when launching onto the Processes tab.
    ///
    /// Defaults to `true`.
    pub project_processes_on_boot: bool,
    /// Coalescing window applied to consecutive slow-lane cold-metadata arrivals before
    /// re-projecting active process tables, preventing startup projection storms.
    ///
    /// Defaults to `40 ms`. Set to `Duration::ZERO` to publish every slow-lane batch immediately.
    pub cold_meta_coalesce_window: Duration,
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
            app_history_flush_interval: Duration::from_secs(60),
            port_scan_cache_ttl: Duration::from_secs(2),
            leak_detector: LeakDetectorConfig::default(),
            gate_processes_by_lease: false,
            project_processes_on_boot: true,
            cold_meta_coalesce_window: Duration::from_millis(40),
        }
    }
}

/// Headless engine coordinator parameterized statically over a [`PlatformBackend`].
///
/// Owns at most **3** background threads (`1` fast-lane sampling & projection thread
/// plus `1..=2` slow-lane bulkhead worker threads; elevated `ProcessNetwork` ETW adds at
/// most `1` on-demand thread strictly while leased and stops immediately on release).
///
/// Publishes domain-sliced [`AppSnapshot`] instances lock-free via [`ArcSwap`] and
/// fires [`SnapshotWakeSignal`] on every snapshot publication or [`CommandEvent`].
pub struct EngineCoordinator<B: PlatformBackend> {
    backend: Arc<B>,
    settings: AppSettings,
    snapshot: Arc<ArcSwap<AppSnapshot>>,
    wake_signal: SnapshotWakeSignal,
    leases: Arc<MonitorLeaseRegistry>,
    cmd_tx: Sender<CommandEnvelope>,
    events: Arc<EventBus>,
    event_channel_capacity: usize,
    fast_handle: Mutex<Option<JoinHandle<()>>>,
    shutdown_sent: Arc<AtomicBool>,
}

impl<B: PlatformBackend> EngineCoordinator<B> {
    /// Initializes an [`EngineCoordinator`] with production [`EngineConfig`] defaults
    /// (`250 ms` startup calibration delay, `2` slow-lane workers, `300 ms` breaker).
    pub fn new(backend: B, settings: AppSettings) -> Result<Self, PalError> {
        Self::with_config(backend, settings, EngineConfig::default())
    }

    /// Initializes an [`EngineCoordinator`] with explicit [`EngineConfig`] parameters.
    ///
    /// Fast-sampler construction, slow-prober initialization, and slow-lane bulkhead thread
    /// creation all execute on the `loom-fast-sampler` background thread so the caller
    /// returns immediately and can initialize the UI window in parallel with Tick 0.
    pub fn with_config(
        backend: B,
        settings: AppSettings,
        config: EngineConfig,
    ) -> Result<Self, PalError> {
        let backend = Arc::new(backend);

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
        let wake_signal = SnapshotWakeSignal::new();
        let leases = Arc::new(MonitorLeaseRegistry::with_wakeup(wake_tx));
        let events = Arc::new(EventBus::with_wake_signal(wake_signal.clone()));
        let shutdown_sent = Arc::new(AtomicBool::new(false));

        let logical_cpu_count = backend.operator().logical_cpu_count().max(1);
        let view_config = ViewProjectionConfig::from_settings(&settings);
        let refresh_speed = settings.refresh_speed;
        let event_channel_capacity = config.event_channel_capacity;

        let runner_backend = Arc::clone(&backend);
        let runner_snapshot = Arc::clone(&snapshot);
        let runner_wake_signal = wake_signal.clone();
        let runner_leases = Arc::clone(&leases);
        let runner_events = Arc::clone(&events);
        let runner_shutdown = Arc::clone(&shutdown_sent);

        let fast_handle = thread::Builder::new()
            .name("loom-fast-sampler".to_owned())
            .spawn(move || {
                if runner_shutdown.load(Ordering::Acquire) {
                    runner_wake_signal.close();
                    return;
                }
                let fast_sampler = match runner_backend.create_fast_sampler() {
                    Ok(sampler) => sampler,
                    Err(err) => {
                        tracing::error!(%err, "failed to create platform fast sampler");
                        runner_wake_signal.close();
                        return;
                    }
                };
                let slow_prober = match runner_backend.create_slow_prober() {
                    Ok(prober) => Arc::new(prober),
                    Err(err) => {
                        tracing::error!(%err, "failed to create platform slow metadata prober");
                        runner_wake_signal.close();
                        return;
                    }
                };
                let slow_pool = match SlowLanePool::spawn(
                    Arc::clone(&runner_backend),
                    slow_prober,
                    Arc::clone(&runner_leases),
                    Arc::clone(&runner_events),
                    config.slow_workers,
                    config.slow_queue_capacity,
                    config.slow_shutdown_timeout,
                ) {
                    Ok(pool) => pool,
                    Err(err) => {
                        tracing::error!(%err, "failed to spawn slow-lane bulkhead pool");
                        runner_wake_signal.close();
                        return;
                    }
                };
                let leak_cfg = config.leak_detector;
                let mut runner = FastLaneRunner {
                    backend: runner_backend,
                    fast_sampler,
                    slow_pool,
                    snapshot_store: runner_snapshot,
                    wake_signal: runner_wake_signal,
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
                    parsed_query: ParsedQuery::default(),
                    projection: ProjectionEngine::with_platform_and_leak_config(
                        B::PLATFORM,
                        leak_cfg,
                    ),
                    cold_cache: ColdMetaCache::new(),
                    pending_procs: HashSet::with_capacity_and_hasher(256, FastBuildHasher),
                    pending_paths: HashSet::with_capacity_and_hasher(64, FastBuildHasher),
                    pending_domains: [false; MonitorDomain::COUNT],
                    failed_procs: HashMap::with_hasher(FastBuildHasher),
                    process_scratch: ProcessSampleScratch::with_capacity(512, 16 * 1024),
                    perf_scratch: PerfSampleScratch::new(),
                    live_keys_scratch: HashSet::with_capacity_and_hasher(512, FastBuildHasher),
                    probe_candidates_scratch: Vec::with_capacity(512),
                    logical_cpu_count,
                    refresh_speed,
                    window_dormant: false,
                    temp_frozen: false,
                    process_net_enabled: false,
                    pending_history_gap: false,
                    last_sample_instant: None,
                    last_sample_timestamp: None,
                    pending_port_scan: false,
                    last_port_scan_at: None,
                    tick_count: 0,
                    next_tick: None,
                    last_processes_projected_tick: 0,
                    last_details_projected_tick: 0,
                    last_perf_sampled_tick: 0,
                    last_per_core_sampled_tick: 0,
                    pending_cold_publish: false,
                    cold_flush_deadline: None,
                    last_cold_publish_at: None,
                    last_domain_poll_tick: [0; MonitorDomain::COUNT],
                    hardware_info_probed: false,
                    pending_hardware_info: false,
                    pending_disk_perf: false,
                    last_disk_poll_tick: 0,
                    cached_services: Vec::new(),
                    services_polled_once: false,
                    services_projection_dirty: false,
                    cached_startup_bios_ms: None,
                    cached_startup_entries: Vec::new(),
                    startup_polled_once: false,
                    startup_projection_dirty: false,
                    cached_user_sessions: Vec::new(),
                    users_polled_once: false,
                    users_projection_dirty: false,
                    persisted_app_history_since: None,
                    persisted_app_history: Vec::new(),
                    app_history_polled_once: false,
                    app_history_projection_dirty: false,
                    app_history_dirty: false,
                    pending_app_history_save: false,
                    last_app_history_flush: Instant::now(),
                };
                runner.run();
            })
            .map_err(PalError::Io)?;

        Ok(Self {
            backend,
            settings,
            snapshot,
            wake_signal,
            leases,
            cmd_tx,
            events,
            event_channel_capacity,
            fast_handle: Mutex::new(Some(fast_handle)),
            shutdown_sent,
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

    /// Returns a cloneable zero-polling [`SnapshotWakeSignal`] fired whenever a new
    /// [`AppSnapshot`] is published or a [`CommandEvent`] is emitted.
    #[inline]
    pub fn snapshot_wake_signal(&self) -> SnapshotWakeSignal {
        self.wake_signal.clone()
    }

    /// Subscribes a coalescing `bounded(1)` [`Receiver<()>`] that receives a wakeup token
    /// whenever a new [`AppSnapshot`] is published or a [`CommandEvent`] is emitted.
    #[inline]
    pub fn subscribe_snapshot_wake(&self) -> Receiver<()> {
        self.wake_signal.subscribe_channel()
    }

    /// Registers a non-blocking callback invoked whenever a new [`AppSnapshot`] is
    /// published or a [`CommandEvent`] is emitted.
    #[inline]
    pub fn set_snapshot_waker(&self, waker: Arc<dyn Fn() + Send + Sync>) {
        self.wake_signal.add_callback(waker);
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

/// Bitmask of domain slices to re-project and publish in a single atomic snapshot update.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct SliceUpdateMask {
    pub(super) processes: bool,
    pub(super) details: bool,
    pub(super) performance: bool,
    pub(super) include_per_core: bool,
    pub(super) services: bool,
    pub(super) startup: bool,
    pub(super) users: bool,
    pub(super) app_history: bool,
}

impl SliceUpdateMask {
    #[inline]
    pub(super) const fn any(self) -> bool {
        self.processes
            || self.details
            || self.performance
            || self.services
            || self.startup
            || self.users
            || self.app_history
    }
}

#[inline]
pub(super) fn current_unix_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[inline]
pub(super) fn push_ack(
    kind: CommandKind,
    reply: Option<Sender<CommandEvent>>,
    pending_acks: &mut Vec<(CommandKind, Sender<CommandEvent>)>,
) {
    if let Some(reply_tx) = reply {
        pending_acks.push((kind, reply_tx));
    }
}

#[inline]
pub(super) fn send_ack(kind: CommandKind, reply_tx: Sender<CommandEvent>) {
    let _ = reply_tx.try_send(CommandEvent::new(
        kind,
        None,
        CommandOutcome::Ok(CommandSuccess::Ack),
    ));
}
