//! Bounded slow-lane bulkhead worker pool, 2-level deduplicated cold cache,
//! and latency-based [`CircuitBreaker`] protection.
//!
//! All operations that may touch disks, network shares, PE version resources,
//! shell icons, or on-demand OS managers run exclusively on at most `2`
//! dedicated slow-lane threads. The fast sampling thread only performs
//! non-blocking `try_send` submissions and non-blocking `try_iter` result drains,
//! guaranteeing that a stalled drive or 5-second slow probe never delays fast ticks.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, select_biased};
use loom_pal_api::{
    AppHistoryReader, PalError, PlatformBackend, ServiceManager, SessionManager,
    SlowMetadataProber, StartupManager, SystemOperator,
};
use loom_types::{
    AppHistoryRecord, DiskDeviceSample, ExeStaticAsset, HardwareStaticInfo, MonitorDomain,
    ProcessColdMeta, ProcessKey, ProcessSocketRecord, RunTaskRequest, ServiceRecord, StartupEntry,
    UacVirtualization, UserSessionRecord,
};
use parking_lot::{Condvar, Mutex};

use crate::command::{CommandEvent, CommandKind, CommandOutcome, CommandSuccess, EventBus};
use crate::lease::MonitorLeaseRegistry;

/// Maximum number of path-deduplicated static assets retained in Level-2 cache.
const MAX_ASSET_CACHE_ENTRIES: usize = 4_096;

/// Latency-based circuit breaker isolating slow drives, UNC shares, or targets.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    timeout: Duration,
    cooldown: Duration,
    tripped: HashMap<String, Instant>,
}

impl CircuitBreaker {
    /// Creates a new [`CircuitBreaker`] with the given slow-probe latency threshold
    /// (typically `300 ms`) and cooldown duration (typically `60 s`).
    pub fn new(timeout: Duration, cooldown: Duration) -> Self {
        Self {
            timeout,
            cooldown,
            tripped: HashMap::new(),
        }
    }

    /// Returns the configured latency threshold beyond which probes trip the breaker.
    #[inline]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Returns the configured cooldown duration while a tripped target is bypassed.
    #[inline]
    pub const fn cooldown(&self) -> Duration {
        self.cooldown
    }

    /// Returns `true` if `target` is currently in cooldown and should be bypassed.
    pub fn is_open(&mut self, target: &str, now: Instant) -> bool {
        if let Some(&until) = self.tripped.get(target) {
            if now < until {
                return true;
            }
            self.tripped.remove(target);
        }
        false
    }

    /// Records the observed latency and timeout status of a probe against `target`.
    ///
    /// Returns `true` if the circuit breaker tripped (or was refreshed) for `target`.
    pub fn record_probe(
        &mut self,
        target: &str,
        elapsed: Duration,
        timed_out_error: bool,
        now: Instant,
    ) -> bool {
        if elapsed >= self.timeout || timed_out_error {
            self.tripped.insert(target.to_owned(), now + self.cooldown);
            tracing::warn!(
                target_key = target,
                elapsed_ms = elapsed.as_millis() as u64,
                cooldown_secs = self.cooldown.as_secs(),
                "slow-lane circuit breaker tripped"
            );
            true
        } else {
            false
        }
    }

    /// Derives a coarse volume/share target key from an executable path so one
    /// stalled drive or UNC share trips the breaker for all binaries on that volume.
    pub fn target_key_for_path(path: &str) -> String {
        let trimmed = path.trim();
        let stripped = trimmed
            .strip_prefix(r"\\?\UNC\")
            .or_else(|| trimmed.strip_prefix(r"\\?\"))
            .unwrap_or(trimmed);

        // UNC share: \\server\share\...
        if let Some(rest) = stripped.strip_prefix(r"\\") {
            let mut parts = rest.split(['\\', '/']).filter(|p| !p.is_empty());
            if let (Some(server), Some(share)) = (parts.next(), parts.next()) {
                return format!(
                    r"\\{}\{}",
                    server.to_ascii_lowercase(),
                    share.to_ascii_lowercase()
                );
            }
        }

        // Windows drive letter: C:\...
        let bytes = stripped.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            let drive = (bytes[0] as char).to_ascii_uppercase();
            return format!("{drive}:");
        }

        // Unix / generic fallback: use first two path segments or normalized path.
        if let Some(rest) = stripped.strip_prefix('/') {
            let first = rest.split('/').next().unwrap_or("");
            return format!("/{first}");
        }

        stripped.to_ascii_lowercase()
    }
}

/// Normalizes an executable path into a deduplication key for the Level-2 asset cache.
pub fn normalize_exe_path_key(path: &str) -> Arc<str> {
    let trimmed = path.trim();
    let stripped = trimmed.strip_prefix(r"\\?\").unwrap_or(trimmed);
    let bytes = stripped.as_bytes();
    let is_windows_style = (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || stripped.starts_with(r"\\");

    if is_windows_style {
        let mut normalized = String::with_capacity(stripped.len());
        for ch in stripped.chars() {
            if ch == '/' {
                normalized.push('\\');
            } else {
                normalized.push(ch.to_ascii_lowercase());
            }
        }
        Arc::<str>::from(normalized)
    } else {
        Arc::<str>::from(stripped)
    }
}

/// Two-level deduplicated cold metadata cache owned exclusively by the fast-lane thread.
///
/// - **Level 1**: `ProcessKey -> Arc<ProcessColdMeta>` (evicts exited processes on tick).
/// - **Level 2**: `normalized_path -> Arc<ExeStaticAsset>` (persists across process instances
///   so dozens of instances of the same executable share a single `Arc<ExeStaticAsset>`).
#[derive(Debug, Default)]
pub struct ColdMetaCache {
    proc_meta: HashMap<ProcessKey, Arc<ProcessColdMeta>>,
    exe_assets: HashMap<Arc<str>, Arc<ExeStaticAsset>>,
    eco_overrides: HashMap<ProcessKey, bool>,
    uac_overrides: HashMap<ProcessKey, UacVirtualization>,
}

impl ColdMetaCache {
    /// Creates an empty two-level cold metadata cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Looks up cached per-process cold metadata for `key`.
    #[inline]
    pub fn get_process(&self, key: ProcessKey) -> Option<&Arc<ProcessColdMeta>> {
        if self.proc_meta.is_empty() {
            return None;
        }
        self.proc_meta.get(&key)
    }

    /// Returns whether Efficiency Mode (`EcoQoS`) is known to be enabled for `key`.
    #[inline]
    pub fn efficiency_mode(&self, key: ProcessKey) -> bool {
        if let Some(meta) = self.proc_meta.get(&key) {
            return meta.efficiency_mode;
        }
        self.eco_overrides.get(&key).copied().unwrap_or(false)
    }

    /// Returns whether `key` is known to be running with an elevated token.
    #[inline]
    pub fn is_elevated(&self, key: ProcessKey) -> bool {
        self.proc_meta
            .get(&key)
            .is_some_and(|meta| meta.is_elevated)
    }

    /// Immediately updates the cached Efficiency Mode (`EcoQoS`) flag for `key`.
    ///
    /// If `key` is already in the Level-1 cache, its `ProcessColdMeta` is updated
    /// immediately; otherwise, the state is staged in `eco_overrides` until the slow-lane
    /// probe populates the entry (preventing stale in-flight probes from clobbering the toggle).
    pub fn set_efficiency_mode(&mut self, key: ProcessKey, enabled: bool) {
        if let Some(meta_arc) = self.proc_meta.get_mut(&key) {
            if meta_arc.efficiency_mode != enabled {
                let mut cloned = (**meta_arc).clone();
                cloned.efficiency_mode = enabled;
                *meta_arc = Arc::new(cloned);
            }
            self.eco_overrides.remove(&key);
        } else {
            self.eco_overrides.insert(key, enabled);
        }
    }

    /// Immediately updates the cached UAC virtualization state for `key`.
    ///
    /// If `key` is already in Level 1, its `ProcessColdMeta` is updated immediately;
    /// otherwise, the override is staged until the slow-lane probe populates the entry.
    pub fn set_uac_virtualization(&mut self, key: ProcessKey, enabled: bool) {
        let next = if enabled {
            UacVirtualization::Enabled
        } else {
            UacVirtualization::Disabled
        };
        if let Some(meta_arc) = self.proc_meta.get_mut(&key) {
            if meta_arc.uac_virtualization != next {
                let mut cloned = (**meta_arc).clone();
                cloned.uac_virtualization = next;
                *meta_arc = Arc::new(cloned);
            }
            self.uac_overrides.remove(&key);
        } else {
            self.uac_overrides.insert(key, next);
        }
    }

    /// Looks up a cached path-deduplicated executable asset by normalized key.
    #[inline]
    pub fn get_asset(&self, cache_key: &str) -> Option<&Arc<ExeStaticAsset>> {
        self.exe_assets.get(cache_key)
    }

    /// Returns the number of cached Level-1 per-process entries.
    #[inline]
    pub fn proc_cache_len(&self) -> usize {
        self.proc_meta.len()
    }

    /// Returns the number of cached Level-2 executable asset entries.
    #[inline]
    pub fn asset_cache_len(&self) -> usize {
        self.exe_assets.len()
    }

    /// Inserts resolved [`ProcessColdMeta`] into Level 1 and reconciles Level 2.
    ///
    /// Returns `Some((raw_exe_path, normalized_cache_key))` when the executable's
    /// static asset is not yet present in Level 2 and should be probed.
    pub fn insert_process(&mut self, mut meta: ProcessColdMeta) -> Option<(Arc<str>, Arc<str>)> {
        let mut needs_asset_probe = None;

        if let Some(asset) = meta.asset.take() {
            let key = normalize_exe_path_key(&asset.normalized_path);
            let shared = self.insert_asset_arc(key.clone(), asset);
            if let Some(ref exe_path) = meta.exe_path {
                let exe_key = normalize_exe_path_key(exe_path);
                if exe_key != key {
                    self.insert_asset_arc(exe_key, Arc::clone(&shared));
                }
            }
            meta.asset = Some(shared);
        } else if let Some(ref exe_path) = meta.exe_path {
            let cache_key = normalize_exe_path_key(exe_path);
            if let Some(shared) = self.exe_assets.get(&cache_key) {
                meta.asset = Some(Arc::clone(shared));
            } else {
                needs_asset_probe = Some((Arc::clone(exe_path), cache_key));
            }
        }

        let proc_key = meta.key;
        if let Some(overridden_eco) = self.eco_overrides.remove(&proc_key) {
            meta.efficiency_mode = overridden_eco;
        }
        if let Some(overridden_uac) = self.uac_overrides.remove(&proc_key) {
            meta.uac_virtualization = overridden_uac;
        }
        self.proc_meta.insert(proc_key, Arc::new(meta));
        needs_asset_probe
    }

    /// Inserts a resolved [`ExeStaticAsset`] into Level 2 and backfills any live
    /// Level-1 [`ProcessColdMeta`] entries referencing the same normalized path.
    ///
    /// Returns `true` if any Level-1 process entry was updated.
    pub fn insert_asset(&mut self, cache_key: Arc<str>, asset: ExeStaticAsset) -> bool {
        let asset_arc = Arc::new(asset);
        let alt_key = normalize_exe_path_key(&asset_arc.normalized_path);
        let shared = self.insert_asset_arc(Arc::clone(&cache_key), asset_arc);
        if alt_key != cache_key {
            self.insert_asset_arc(alt_key, Arc::clone(&shared));
        }

        let mut updated_any = false;
        for meta_arc in self.proc_meta.values_mut() {
            let matches_path = meta_arc
                .exe_path
                .as_ref()
                .is_some_and(|p| normalize_exe_path_key(p) == cache_key);
            if matches_path && meta_arc.asset.is_none() {
                let mut cloned = (**meta_arc).clone();
                cloned.asset = Some(Arc::clone(&shared));
                *meta_arc = Arc::new(cloned);
                updated_any = true;
            }
        }
        updated_any
    }

    /// Evicts exited processes from Level 1 while preserving Level-2 executable assets.
    pub fn retain_live_processes(&mut self, live: &HashSet<ProcessKey>) {
        self.proc_meta.retain(|k, _| live.contains(k));
        self.eco_overrides.retain(|k, _| live.contains(k));
        self.uac_overrides.retain(|k, _| live.contains(k));
    }

    fn insert_asset_arc(
        &mut self,
        key: Arc<str>,
        asset: Arc<ExeStaticAsset>,
    ) -> Arc<ExeStaticAsset> {
        if self.exe_assets.len() >= MAX_ASSET_CACHE_ENTRIES && !self.exe_assets.contains_key(&key) {
            // Simple bounded eviction to prevent unbounded memory growth over long uptimes.
            if let Some(evict_key) = self.exe_assets.keys().next().cloned() {
                self.exe_assets.remove(&evict_key);
            }
        }
        Arc::clone(self.exe_assets.entry(key).or_insert(asset))
    }
}

/// Potentially blocking shell/operator jobs executed on the high-priority slow-lane channel.
#[derive(Debug)]
pub(crate) enum SlowShellOp {
    RunNewTask(RunTaskRequest),
    BrowseExecutable,
    OpenFileLocation(PathBuf),
    ShowFileProperties(PathBuf),
    SearchOnline(String),
    RelaunchElevated(String),
    AnalyzeWaitChain(ProcessKey),
    WriteMiniDump {
        key: ProcessKey,
        dest: PathBuf,
        cancel: Arc<AtomicBool>,
    },
    AttachDebugger(ProcessKey),
    RestartExplorer,
    FindFileLocks(PathBuf),
}

/// Blocking system management operations (Services, Startup, Users, App History)
/// executed on the high-priority slow-lane channel.
#[derive(Debug)]
pub(crate) enum SlowMgmtOp {
    StartService(String),
    StopService(String),
    RestartService(String),
    OpenServicesConsole,
    SetStartupEnabled { id: String, enabled: bool },
    DisconnectSession(u32),
    ClearAppHistory,
}

/// Refreshed domain payload fetched immediately after a mutating [`SlowMgmtOp`] succeeds
/// while the affected [`MonitorDomain`] holds an active lease.
#[derive(Debug)]
pub(crate) enum MgmtDomainRefresh {
    Services(Result<Vec<ServiceRecord>, PalError>),
    Startup(Result<(Option<u64>, Vec<StartupEntry>), PalError>),
    Users(Result<Vec<UserSessionRecord>, PalError>),
    AppHistory(Result<(Option<u64>, Vec<AppHistoryRecord>), PalError>),
}

/// High-priority jobs executed ahead of background icon/metadata probes.
#[derive(Debug)]
pub(crate) enum SlowPriorityJob {
    ShellOp {
        op: SlowShellOp,
        reply: Option<Sender<CommandEvent>>,
    },
    MgmtOp {
        op: SlowMgmtOp,
        reply: Option<Sender<CommandEvent>>,
    },
    PollOnDemandDomain(MonitorDomain),
    ProbeHardwareStaticInfo,
    PollDiskPerformance,
    PollListeningPorts,
    SaveAppHistory {
        since_epoch_secs: u64,
        records: Vec<AppHistoryRecord>,
    },
}

/// Low-priority background metadata probe jobs.
#[derive(Debug)]
pub(crate) enum SlowProbeJob {
    ProbeProcess(ProcessKey),
    ProbeExeAsset {
        exe_path: Arc<str>,
        cache_key: Arc<str>,
    },
}

/// Results sent non-blockingly from slow-lane workers back to the fast-lane coordinator.
#[derive(Debug)]
pub(crate) enum SlowResult {
    ProcessMeta {
        key: ProcessKey,
        result: Result<ProcessColdMeta, PalError>,
        elapsed: Duration,
    },
    ExeAsset {
        exe_path: Arc<str>,
        cache_key: Arc<str>,
        result: Result<ExeStaticAsset, PalError>,
        elapsed: Duration,
    },
    HardwareStaticInfo(Result<HardwareStaticInfo, PalError>),
    DiskPerformancePolled(Result<Vec<DiskDeviceSample>, PalError>),
    ListeningPortsPolled(Result<Vec<ProcessSocketRecord>, PalError>),
    AppHistorySaved(Result<(), PalError>),
    ServicesPolled(Result<Vec<ServiceRecord>, PalError>),
    StartupPolled(Result<(Option<u64>, Vec<StartupEntry>), PalError>),
    UsersPolled(Result<Vec<UserSessionRecord>, PalError>),
    AppHistoryPolled(Result<(Option<u64>, Vec<AppHistoryRecord>), PalError>),
    MgmtOpCompleted {
        event: CommandEvent,
        reply: Option<Sender<CommandEvent>>,
        cleared_app_history: bool,
        refresh: Option<MgmtDomainRefresh>,
    },
}

/// Bounded slow-lane bulkhead pool (strictly `1..=2` threads) with deduplicated submission
/// and hang-safe bounded shutdown.
pub(crate) struct SlowLanePool {
    priority_tx: Option<Sender<SlowPriorityJob>>,
    probe_tx: Option<Sender<SlowProbeJob>>,
    result_rx: Receiver<SlowResult>,
    shutdown_flag: Arc<AtomicBool>,
    active_workers: Arc<(Mutex<usize>, Condvar)>,
    handles: Vec<JoinHandle<()>>,
    shutdown_timeout: Duration,
}

impl SlowLanePool {
    /// Spawns `workers.clamp(1, 2)` slow-lane bulkhead worker threads.
    pub(crate) fn spawn<B: PlatformBackend>(
        backend: Arc<B>,
        prober: Arc<B::SlowProber>,
        leases: Arc<MonitorLeaseRegistry>,
        events: Arc<EventBus>,
        workers: usize,
        queue_capacity: usize,
        shutdown_timeout: Duration,
    ) -> Result<Self, PalError> {
        let worker_count = workers.clamp(1, 2);
        let probe_cap = queue_capacity.max(16);
        let (priority_tx, priority_rx) = crossbeam_channel::bounded::<SlowPriorityJob>(64);
        let (probe_tx, probe_rx) = crossbeam_channel::bounded::<SlowProbeJob>(probe_cap);
        let (result_tx, result_rx) = crossbeam_channel::bounded::<SlowResult>(probe_cap * 2);

        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let active_workers = Arc::new((Mutex::new(worker_count), Condvar::new()));
        let mut handles = Vec::with_capacity(worker_count);

        for idx in 0..worker_count {
            let backend = Arc::clone(&backend);
            let prober = Arc::clone(&prober);
            let leases = Arc::clone(&leases);
            let events = Arc::clone(&events);
            let priority_rx = priority_rx.clone();
            let probe_rx = probe_rx.clone();
            let result_tx = result_tx.clone();
            let shutdown_flag = Arc::clone(&shutdown_flag);
            let active_workers = Arc::clone(&active_workers);

            let handle = thread::Builder::new()
                .name(format!("loom-slow-{idx}"))
                .spawn(move || {
                    slow_worker_loop(
                        backend,
                        prober,
                        leases,
                        events,
                        priority_rx,
                        probe_rx,
                        result_tx,
                        shutdown_flag,
                    );
                    let (lock, cvar) = &*active_workers;
                    let mut remaining = lock.lock();
                    *remaining = remaining.saturating_sub(1);
                    if *remaining == 0 {
                        cvar.notify_all();
                    }
                })
                .map_err(PalError::Io)?;
            handles.push(handle);
        }

        Ok(Self {
            priority_tx: Some(priority_tx),
            probe_tx: Some(probe_tx),
            result_rx,
            shutdown_flag,
            active_workers,
            handles,
            shutdown_timeout,
        })
    }

    #[inline]
    pub(crate) fn result_rx(&self) -> &Receiver<SlowResult> {
        &self.result_rx
    }

    /// Non-blockingly attempts to enqueue a background metadata probe.
    #[inline]
    pub(crate) fn try_enqueue_probe(&self, job: SlowProbeJob) -> bool {
        self.probe_tx
            .as_ref()
            .is_some_and(|tx| tx.try_send(job).is_ok())
    }

    /// Non-blockingly attempts to enqueue a high-priority shell or on-demand domain job.
    #[inline]
    pub(crate) fn try_enqueue_priority(&self, job: SlowPriorityJob) -> Result<(), SlowPriorityJob> {
        let Some(ref tx) = self.priority_tx else {
            return Err(job);
        };
        tx.try_send(job).map_err(|err| match err {
            crossbeam_channel::TrySendError::Full(j)
            | crossbeam_channel::TrySendError::Disconnected(j) => j,
        })
    }

    /// Closes job channels and waits up to `shutdown_timeout` for workers to exit.
    ///
    /// If a slow worker is wedged inside a hung disk/OS call beyond the shutdown timeout,
    /// its thread handle is detached so application shutdown never hangs indefinitely.
    pub(crate) fn shutdown(&mut self) {
        self.shutdown_flag.store(true, Ordering::Release);
        self.priority_tx.take();
        self.probe_tx.take();

        let (lock, cvar) = &*self.active_workers;
        let mut remaining = lock.lock();
        if *remaining > 0 {
            let _ = cvar.wait_for(&mut remaining, self.shutdown_timeout);
        }
        let all_finished = *remaining == 0;
        drop(remaining);

        if all_finished {
            for handle in self.handles.drain(..) {
                let _ = handle.join();
            }
        } else {
            let stuck = self.handles.len();
            tracing::warn!(
                stuck_workers = stuck,
                timeout_ms = self.shutdown_timeout.as_millis() as u64,
                "detaching slow-lane worker(s) still blocked in slow probe during shutdown"
            );
            self.handles.clear();
        }
    }
}

impl Drop for SlowLanePool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[allow(clippy::too_many_arguments)]
fn slow_worker_loop<B: PlatformBackend>(
    backend: Arc<B>,
    prober: Arc<B::SlowProber>,
    leases: Arc<MonitorLeaseRegistry>,
    events: Arc<EventBus>,
    priority_rx: Receiver<SlowPriorityJob>,
    probe_rx: Receiver<SlowProbeJob>,
    result_tx: Sender<SlowResult>,
    shutdown_flag: Arc<AtomicBool>,
) {
    loop {
        if shutdown_flag.load(Ordering::Acquire) {
            break;
        }

        select_biased! {
            recv(priority_rx) -> msg => {
                match msg {
                    Ok(job) => execute_priority_job(&*backend, &*prober, &leases, &events, &result_tx, job),
                    Err(_) => break,
                }
            }
            recv(probe_rx) -> msg => {
                match msg {
                    Ok(job) => execute_probe_job(&*prober, &result_tx, job),
                    Err(_) => break,
                }
            }
        }
    }
}

fn execute_priority_job<B: PlatformBackend>(
    backend: &B,
    prober: &B::SlowProber,
    leases: &MonitorLeaseRegistry,
    events: &EventBus,
    result_tx: &Sender<SlowResult>,
    job: SlowPriorityJob,
) {
    match job {
        SlowPriorityJob::ShellOp { op, reply } => {
            let operator = backend.operator();
            let event = match op {
                SlowShellOp::RunNewTask(req) => match operator.run_new_task(&req) {
                    Ok(pid) => CommandEvent::new(
                        CommandKind::RunNewTask,
                        None,
                        CommandOutcome::Ok(CommandSuccess::TaskLaunched { pid }),
                    ),
                    Err(err) => {
                        CommandEvent::new(CommandKind::RunNewTask, None, CommandOutcome::err(err))
                    }
                },
                SlowShellOp::BrowseExecutable => match operator.browse_executable() {
                    Ok(path) => CommandEvent::new(
                        CommandKind::BrowseExecutable,
                        None,
                        CommandOutcome::Ok(CommandSuccess::FilePicked(path)),
                    ),
                    Err(err) => CommandEvent::new(
                        CommandKind::BrowseExecutable,
                        None,
                        CommandOutcome::err(err),
                    ),
                },
                SlowShellOp::OpenFileLocation(path) => match operator.open_file_location(&path) {
                    Ok(()) => CommandEvent::new(
                        CommandKind::OpenFileLocation,
                        None,
                        CommandOutcome::Ok(CommandSuccess::Ack),
                    ),
                    Err(err) => CommandEvent::new(
                        CommandKind::OpenFileLocation,
                        None,
                        CommandOutcome::err(err),
                    ),
                },
                SlowShellOp::ShowFileProperties(path) => match operator.show_file_properties(&path)
                {
                    Ok(()) => CommandEvent::new(
                        CommandKind::ShowFileProperties,
                        None,
                        CommandOutcome::Ok(CommandSuccess::Ack),
                    ),
                    Err(err) => CommandEvent::new(
                        CommandKind::ShowFileProperties,
                        None,
                        CommandOutcome::err(err),
                    ),
                },
                SlowShellOp::SearchOnline(query) => match operator.search_online(&query) {
                    Ok(()) => CommandEvent::new(
                        CommandKind::SearchOnline,
                        None,
                        CommandOutcome::Ok(CommandSuccess::Ack),
                    ),
                    Err(err) => {
                        CommandEvent::new(CommandKind::SearchOnline, None, CommandOutcome::err(err))
                    }
                },
                SlowShellOp::RelaunchElevated(route) => {
                    match operator.relaunch_self_elevated(&route) {
                        Ok(()) => CommandEvent::new(
                            CommandKind::RelaunchElevated,
                            None,
                            CommandOutcome::Ok(CommandSuccess::Ack),
                        ),
                        Err(err) => CommandEvent::new(
                            CommandKind::RelaunchElevated,
                            None,
                            CommandOutcome::err(err),
                        ),
                    }
                }
                SlowShellOp::AnalyzeWaitChain(key) => match operator.analyze_wait_chain(key) {
                    Ok(nodes) => CommandEvent::new(
                        CommandKind::AnalyzeWaitChain,
                        Some(key),
                        CommandOutcome::Ok(CommandSuccess::WaitChain(nodes)),
                    ),
                    Err(err) => CommandEvent::new(
                        CommandKind::AnalyzeWaitChain,
                        Some(key),
                        CommandOutcome::err(err),
                    ),
                },
                SlowShellOp::WriteMiniDump { key, dest, cancel } => {
                    match operator.write_minidump(key, &dest, &cancel) {
                        Ok(()) => CommandEvent::new(
                            CommandKind::WriteMiniDump,
                            Some(key),
                            CommandOutcome::Ok(CommandSuccess::MiniDumpWritten(dest)),
                        ),
                        Err(err) => CommandEvent::new(
                            CommandKind::WriteMiniDump,
                            Some(key),
                            CommandOutcome::err(err),
                        ),
                    }
                }
                SlowShellOp::AttachDebugger(key) => match operator.attach_debugger(key) {
                    Ok(()) => CommandEvent::new(
                        CommandKind::AttachDebugger,
                        Some(key),
                        CommandOutcome::Ok(CommandSuccess::Ack),
                    ),
                    Err(err) => CommandEvent::new(
                        CommandKind::AttachDebugger,
                        Some(key),
                        CommandOutcome::err(err),
                    ),
                },
                SlowShellOp::RestartExplorer => match operator.restart_explorer() {
                    Ok(()) => CommandEvent::new(
                        CommandKind::RestartExplorer,
                        None,
                        CommandOutcome::Ok(CommandSuccess::Ack),
                    ),
                    Err(err) => CommandEvent::new(
                        CommandKind::RestartExplorer,
                        None,
                        CommandOutcome::err(err),
                    ),
                },
                SlowShellOp::FindFileLocks(path) => match operator.find_file_locks(&path) {
                    Ok(holders) => CommandEvent::new(
                        CommandKind::FindFileLocks,
                        None,
                        CommandOutcome::Ok(CommandSuccess::FileLocks(holders)),
                    ),
                    Err(err) => CommandEvent::new(
                        CommandKind::FindFileLocks,
                        None,
                        CommandOutcome::err(err),
                    ),
                },
            };
            events.emit(event, reply);
        }
        SlowPriorityJob::MgmtOp { op, reply } => {
            let (kind, result, affected_domain, cleared_app_history) = match op {
                SlowMgmtOp::StartService(name) => (
                    CommandKind::StartService,
                    backend.service_manager().start_service(&name),
                    Some(MonitorDomain::Services),
                    false,
                ),
                SlowMgmtOp::StopService(name) => (
                    CommandKind::StopService,
                    backend.service_manager().stop_service(&name),
                    Some(MonitorDomain::Services),
                    false,
                ),
                SlowMgmtOp::RestartService(name) => (
                    CommandKind::RestartService,
                    backend.service_manager().restart_service(&name),
                    Some(MonitorDomain::Services),
                    false,
                ),
                SlowMgmtOp::OpenServicesConsole => (
                    CommandKind::OpenServicesConsole,
                    backend.service_manager().open_services_console(),
                    None,
                    false,
                ),
                SlowMgmtOp::SetStartupEnabled { id, enabled } => (
                    CommandKind::SetStartupEnabled,
                    backend.startup_manager().set_startup_enabled(&id, enabled),
                    Some(MonitorDomain::Startup),
                    false,
                ),
                SlowMgmtOp::DisconnectSession(session_id) => (
                    CommandKind::DisconnectSession,
                    backend.session_manager().disconnect_session(session_id),
                    Some(MonitorDomain::Users),
                    false,
                ),
                SlowMgmtOp::ClearAppHistory => (
                    CommandKind::ClearAppHistory,
                    backend.history_reader().clear_history(),
                    Some(MonitorDomain::AppHistory),
                    true,
                ),
            };

            let (outcome, succeeded) = match result {
                Ok(()) => (CommandOutcome::Ok(CommandSuccess::Ack), true),
                Err(err) => (CommandOutcome::err(err), false),
            };
            let event = CommandEvent::new(kind, None, outcome);

            // Immediately refresh the affected domain on success ONLY while that domain is leased.
            let refresh = if succeeded
                && let Some(domain) = affected_domain
                && leases.is_active(domain)
            {
                match domain {
                    MonitorDomain::Services => Some(MgmtDomainRefresh::Services(
                        backend.service_manager().list_services(),
                    )),
                    MonitorDomain::Startup => Some(MgmtDomainRefresh::Startup(
                        backend.startup_manager().list_startup_entries(),
                    )),
                    MonitorDomain::Users => Some(MgmtDomainRefresh::Users(
                        backend.session_manager().list_sessions(),
                    )),
                    MonitorDomain::AppHistory => Some(MgmtDomainRefresh::AppHistory(
                        backend.history_reader().read_history(),
                    )),
                    MonitorDomain::Details
                    | MonitorDomain::Performance
                    | MonitorDomain::PerCoreCpu
                    | MonitorDomain::ProcessNetwork => None,
                }
            } else {
                None
            };

            let msg = SlowResult::MgmtOpCompleted {
                event,
                reply,
                cleared_app_history: succeeded && cleared_app_history,
                refresh,
            };
            if let Err(err) = result_tx.send_timeout(msg, Duration::from_millis(200))
                && let SlowResult::MgmtOpCompleted { event, reply, .. } = err.into_inner()
            {
                events.emit(event, reply);
            }
        }
        SlowPriorityJob::PollOnDemandDomain(domain) => {
            let msg = match domain {
                MonitorDomain::Services => {
                    SlowResult::ServicesPolled(backend.service_manager().list_services())
                }
                MonitorDomain::Startup => {
                    SlowResult::StartupPolled(backend.startup_manager().list_startup_entries())
                }
                MonitorDomain::Users => {
                    SlowResult::UsersPolled(backend.session_manager().list_sessions())
                }
                MonitorDomain::AppHistory => {
                    SlowResult::AppHistoryPolled(backend.history_reader().read_history())
                }
                MonitorDomain::Details
                | MonitorDomain::Performance
                | MonitorDomain::PerCoreCpu
                | MonitorDomain::ProcessNetwork => {
                    return;
                }
            };
            let _ = result_tx.try_send(msg);
        }
        SlowPriorityJob::ProbeHardwareStaticInfo => {
            let res = prober.probe_hardware_static_info();
            let _ = result_tx.try_send(SlowResult::HardwareStaticInfo(res));
        }
        SlowPriorityJob::PollDiskPerformance => {
            let res = prober.probe_disk_performance();
            let _ = result_tx.try_send(SlowResult::DiskPerformancePolled(res));
        }
        SlowPriorityJob::PollListeningPorts => {
            let res = prober.list_listening_ports();
            let _ = result_tx.try_send(SlowResult::ListeningPortsPolled(res));
        }
        SlowPriorityJob::SaveAppHistory {
            since_epoch_secs,
            records,
        } => {
            let res = backend
                .history_reader()
                .save_history(since_epoch_secs, &records);
            let _ = result_tx.try_send(SlowResult::AppHistorySaved(res));
        }
    }
}

fn execute_probe_job<P: SlowMetadataProber>(
    prober: &P,
    result_tx: &Sender<SlowResult>,
    job: SlowProbeJob,
) {
    match job {
        SlowProbeJob::ProbeProcess(key) => {
            let started = Instant::now();
            let result = prober.probe_process(key);
            let elapsed = started.elapsed();
            let _ = result_tx.try_send(SlowResult::ProcessMeta {
                key,
                result,
                elapsed,
            });
        }
        SlowProbeJob::ProbeExeAsset {
            exe_path,
            cache_key,
        } => {
            let started = Instant::now();
            let result = prober.probe_exe_asset(&exe_path);
            let elapsed = started.elapsed();
            let _ = result_tx.try_send(SlowResult::ExeAsset {
                exe_path,
                cache_key,
                result,
                elapsed,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use loom_types::{ProcessArch, UacVirtualization};

    use super::*;

    #[test]
    fn circuit_breaker_trips_and_expires_after_cooldown() {
        let mut cb = CircuitBreaker::new(Duration::from_millis(300), Duration::from_secs(60));
        let t0 = Instant::now();

        assert!(!cb.is_open("D:", t0));
        assert!(!cb.record_probe("D:", Duration::from_millis(50), false, t0));
        assert!(!cb.is_open("D:", t0));

        assert!(cb.record_probe("D:", Duration::from_millis(350), false, t0));
        assert!(cb.is_open("D:", t0 + Duration::from_secs(10)));
        assert!(!cb.is_open("D:", t0 + Duration::from_secs(61)));
    }

    #[test]
    fn target_key_extracts_drive_and_unc_share() {
        assert_eq!(
            CircuitBreaker::target_key_for_path(r"d:\Games\App\foo.exe"),
            "D:"
        );
        assert_eq!(
            CircuitBreaker::target_key_for_path(r"\\?\C:\Windows\explorer.exe"),
            "C:"
        );
        assert_eq!(
            CircuitBreaker::target_key_for_path(r"\\FileServer\Public\tool.exe"),
            r"\\fileserver\public"
        );
    }

    #[test]
    fn cold_meta_cache_deduplicates_assets_across_process_instances() {
        let mut cache = ColdMetaCache::new();
        let p1 = ProcessKey::new(10, 100);
        let p2 = ProcessKey::new(20, 200);

        let meta1 = ProcessColdMeta {
            key: p1,
            exe_path: Some(Arc::from(r"C:\Program Files\Chrome\chrome.exe")),
            command_line: None,
            user_name: Some(Arc::from("User")),
            arch: ProcessArch::X64,
            uac_virtualization: UacVirtualization::NotAllowed,
            efficiency_mode: false,
            is_elevated: false,
            app_group_id: None,
            asset: None,
        };
        let needs = cache.insert_process(meta1);
        let (_, cache_key) = needs.expect("first process should require asset probe");

        let asset = ExeStaticAsset {
            normalized_path: Arc::from(r"C:\Program Files\Chrome\chrome.exe"),
            publisher: Some(Arc::from("Google LLC")),
            file_description: Some(Arc::from("Google Chrome")),
            icon: None,
            is_network_path: false,
        };
        assert!(cache.insert_asset(cache_key, asset));

        // Second instance with different casing shares the exact same Arc<ExeStaticAsset>
        let meta2 = ProcessColdMeta {
            key: p2,
            exe_path: Some(Arc::from(r"c:/program files/chrome/chrome.exe")),
            command_line: None,
            user_name: Some(Arc::from("User")),
            arch: ProcessArch::X64,
            uac_virtualization: UacVirtualization::NotAllowed,
            efficiency_mode: true,
            is_elevated: true,
            app_group_id: None,
            asset: None,
        };
        assert!(
            cache.insert_process(meta2).is_none(),
            "second instance should hit Level-2 asset cache immediately"
        );

        let a1 = cache.get_process(p1).unwrap().asset.as_ref().unwrap();
        let a2 = cache.get_process(p2).unwrap().asset.as_ref().unwrap();
        assert!(Arc::ptr_eq(a1, a2));
        assert!(cache.efficiency_mode(p2));
        assert!(cache.is_elevated(p2));

        // Immediate EcoQoS toggle updates cached state both for cached and pre-probe processes.
        cache.set_efficiency_mode(p2, false);
        assert!(!cache.efficiency_mode(p2));
        let p3 = ProcessKey::new(30, 300);
        cache.set_efficiency_mode(p3, true);
        assert!(cache.efficiency_mode(p3));

        // Evicting p1 removes Level-1 entry but preserves Level-2 asset
        let live: HashSet<ProcessKey> = [p2].into_iter().collect();
        cache.retain_live_processes(&live);
        assert_eq!(cache.proc_cache_len(), 1);
        assert_eq!(cache.asset_cache_len(), 1);
        assert!(!cache.efficiency_mode(p3));
    }
}
