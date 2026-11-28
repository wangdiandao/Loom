//! Bounded slow-lane bulkhead worker pool (`1..=2` threads) and job dispatch.

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
    UserSessionRecord,
};
use parking_lot::{Condvar, Mutex};

use crate::command::{CommandEvent, CommandKind, CommandOutcome, CommandSuccess, EventBus};
use crate::lease::MonitorLeaseRegistry;

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
    QueryTaskManagerReplacement,
    SetTaskManagerReplacement(bool),
    OpenNativeTaskManager,
    QueryAdminStartup,
    SetAdminStartup(bool),
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
    DiskPerformanceSkipped,
    ListeningPortsPolled(Result<Vec<ProcessSocketRecord>, PalError>),
    AppHistorySaved(Result<(), PalError>),
    DomainPollSkipped(MonitorDomain),
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

#[inline]
fn shell_event<T>(
    kind: CommandKind,
    target: Option<ProcessKey>,
    result: Result<T, PalError>,
    map_ok: impl FnOnce(T) -> CommandSuccess,
) -> CommandEvent {
    let outcome = match result {
        Ok(val) => CommandOutcome::Ok(map_ok(val)),
        Err(err) => CommandOutcome::err(err),
    };
    CommandEvent::new(kind, target, outcome)
}

fn execute_shell_op<O: SystemOperator>(operator: &O, op: SlowShellOp) -> CommandEvent {
    match op {
        SlowShellOp::RunNewTask(req) => shell_event(
            CommandKind::RunNewTask,
            None,
            operator.run_new_task(&req),
            |pid| CommandSuccess::TaskLaunched { pid },
        ),
        SlowShellOp::BrowseExecutable => shell_event(
            CommandKind::BrowseExecutable,
            None,
            operator.browse_executable(),
            CommandSuccess::FilePicked,
        ),
        SlowShellOp::OpenFileLocation(path) => shell_event(
            CommandKind::OpenFileLocation,
            None,
            operator.open_file_location(&path),
            |()| CommandSuccess::Ack,
        ),
        SlowShellOp::ShowFileProperties(path) => shell_event(
            CommandKind::ShowFileProperties,
            None,
            operator.show_file_properties(&path),
            |()| CommandSuccess::Ack,
        ),
        SlowShellOp::SearchOnline(query) => shell_event(
            CommandKind::SearchOnline,
            None,
            operator.search_online(&query),
            |()| CommandSuccess::Ack,
        ),
        SlowShellOp::RelaunchElevated(route) => shell_event(
            CommandKind::RelaunchElevated,
            None,
            operator.relaunch_self_elevated(&route),
            |()| CommandSuccess::Ack,
        ),
        SlowShellOp::AnalyzeWaitChain(key) => shell_event(
            CommandKind::AnalyzeWaitChain,
            Some(key),
            operator.analyze_wait_chain(key),
            CommandSuccess::WaitChain,
        ),
        SlowShellOp::WriteMiniDump { key, dest, cancel } => {
            let res = operator.write_minidump(key, &dest, &cancel);
            shell_event(CommandKind::WriteMiniDump, Some(key), res, |()| {
                CommandSuccess::MiniDumpWritten(dest)
            })
        }
        SlowShellOp::AttachDebugger(key) => shell_event(
            CommandKind::AttachDebugger,
            Some(key),
            operator.attach_debugger(key),
            |()| CommandSuccess::Ack,
        ),
        SlowShellOp::RestartExplorer => shell_event(
            CommandKind::RestartExplorer,
            None,
            operator.restart_explorer(),
            |()| CommandSuccess::Ack,
        ),
        SlowShellOp::FindFileLocks(path) => shell_event(
            CommandKind::FindFileLocks,
            None,
            operator.find_file_locks(&path),
            CommandSuccess::FileLocks,
        ),
        SlowShellOp::QueryTaskManagerReplacement => shell_event(
            CommandKind::QueryTaskManagerReplacement,
            None,
            operator.task_manager_replacement_status(),
            CommandSuccess::TaskManagerReplacementStatus,
        ),
        SlowShellOp::SetTaskManagerReplacement(enabled) => shell_event(
            CommandKind::SetTaskManagerReplacement,
            None,
            operator.set_task_manager_replacement(enabled),
            CommandSuccess::TaskManagerReplacementStatus,
        ),
        SlowShellOp::OpenNativeTaskManager => shell_event(
            CommandKind::OpenNativeTaskManager,
            None,
            operator.open_native_task_manager(),
            |()| CommandSuccess::Ack,
        ),
        SlowShellOp::QueryAdminStartup => shell_event(
            CommandKind::QueryAdminStartup,
            None,
            operator.admin_startup_status(),
            CommandSuccess::AdminStartupStatus,
        ),
        SlowShellOp::SetAdminStartup(enabled) => shell_event(
            CommandKind::SetAdminStartup,
            None,
            operator.set_admin_startup(enabled),
            CommandSuccess::AdminStartupStatus,
        ),
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
            let event = execute_shell_op(backend.operator(), op);
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
                    | MonitorDomain::ProcessNetwork
                    | MonitorDomain::Processes => None,
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
            // Re-verify lease right before invoking OS managers so a lease dropped while
            // the job sat in the queue incurs zero background polling overhead.
            if !leases.is_active(domain) {
                let _ = result_tx.try_send(SlowResult::DomainPollSkipped(domain));
                return;
            }
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
                | MonitorDomain::ProcessNetwork
                | MonitorDomain::Processes => {
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
            if !leases.is_active(MonitorDomain::Performance) {
                let _ = result_tx.try_send(SlowResult::DiskPerformanceSkipped);
                return;
            }
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
