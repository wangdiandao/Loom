//! Engine commands, structured outcomes, and event feedback channels.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crossbeam_channel::Sender;
use loom_pal_api::PalError;
use loom_types::{
    CpuAffinityMask, DetailsSortColumn, DetailsViewMode, ProcessKey, ProcessPriority,
    ProcessSortColumn, ProcessViewMode, RefreshSpeed, RunTaskRequest, SortDirection,
    TreeKillReport, WaitChainNode,
};
use parking_lot::Mutex;
use thiserror::Error;

/// Commands dispatched from UI slices or headless callers into [`crate::EngineCoordinator`].
#[derive(Debug, Clone)]
pub enum EngineCommand {
    /// Updates the fast-lane periodic sampling cadence.
    SetRefreshSpeed(RefreshSpeed),
    /// Updates the global case-insensitive search query applied to process/details projections.
    SetSearchQuery(String),
    /// Switches the Processes view between grouped sections and parent-child tree mode.
    SetProcessViewMode(ProcessViewMode),
    /// Updates the sort column and direction of the Processes view.
    SetProcessSort {
        /// Column to sort by.
        column: ProcessSortColumn,
        /// Ascending or descending direction.
        direction: SortDirection,
    },
    /// Toggles the expanded/collapsed state of a row in the Processes view.
    ToggleProcessExpanded(ProcessKey),
    /// Switches the Details view between flat table and parent-child tree mode.
    SetDetailsViewMode(DetailsViewMode),
    /// Updates the sort column and direction of the Details view.
    SetDetailsSort {
        /// Column to sort by.
        column: DetailsSortColumn,
        /// Ascending or descending direction.
        direction: SortDirection,
    },
    /// Toggles the expanded/collapsed state of a row in the Details tree view.
    ToggleDetailsExpanded(ProcessKey),
    /// Enters or exits window dormancy (minimized / closed to tray) to suspend fast-lane ticks.
    SetWindowDormant(bool),
    /// Triggers an immediate sampling and projection pass even when paused.
    ForceRefresh,
    /// Launches a new process via [`loom_pal_api::SystemOperator::run_new_task`].
    RunNewTask(RunTaskRequest),
    /// Terminates a single process verified by [`ProcessKey`].
    TerminateProcess(ProcessKey),
    /// Computes bottom-up kill order from the latest verified tree and terminates the process tree.
    TerminateProcessTree(ProcessKey),
    /// Enables or disables Efficiency Mode (`EcoQoS`) for a process.
    SetEfficiencyMode {
        /// Target process lifecycle key.
        key: ProcessKey,
        /// Desired Efficiency Mode state.
        enabled: bool,
    },
    /// Changes the scheduling priority class of a process.
    SetPriority {
        /// Target process lifecycle key.
        key: ProcessKey,
        /// Target priority class.
        priority: ProcessPriority,
    },
    /// Queries the current multi-group CPU affinity mask of a process.
    GetAffinity(ProcessKey),
    /// Applies a new multi-group CPU affinity mask to a process.
    SetAffinity {
        /// Target process lifecycle key.
        key: ProcessKey,
        /// Target CPU affinity mask.
        mask: CpuAffinityMask,
    },
    /// Enables or disables UAC virtualization for a process.
    SetUacVirtualization {
        /// Target process lifecycle key.
        key: ProcessKey,
        /// Desired UAC virtualization state.
        enabled: bool,
    },
    /// Inspects the cross-process wait chain / deadlock graph for a process on the slow lane.
    AnalyzeWaitChain(ProcessKey),
    /// Streams a cancellable process memory minidump to `dest` on the slow lane.
    WriteMiniDump {
        /// Target process lifecycle key.
        key: ProcessKey,
        /// Destination `.DMP` file path.
        dest: PathBuf,
        /// Shared cancellation flag polled during dump streaming.
        cancel: Arc<AtomicBool>,
    },
    /// Launches the OS-configured JIT debugger attached to a process on the slow lane.
    AttachDebugger(ProcessKey),
    /// Opens the native OS executable file picker on the slow lane.
    BrowseExecutable,
    /// Opens the OS file explorer and selects `PathBuf`.
    OpenFileLocation(PathBuf),
    /// Opens the native OS properties dialog for `PathBuf`.
    ShowFileProperties(PathBuf),
    /// Opens the user's browser searching online for the supplied query.
    SearchOnline(String),
    /// Relaunches Loom elevated and restores the given route identifier.
    RelaunchElevated(String),
    /// Starts an OS system service identified by its internal name.
    StartService(String),
    /// Stops an OS system service identified by its internal name.
    StopService(String),
    /// Restarts an OS system service identified by its internal name.
    RestartService(String),
    /// Launches the native OS services management console (e.g., `services.msc`).
    OpenServicesConsole,
    /// Enables or disables a registered startup application entry by its stable ID.
    SetStartupEnabled {
        /// Stable startup entry identifier from [`loom_types::StartupEntry::id`].
        id: String,
        /// Desired enabled state.
        enabled: bool,
    },
    /// Disconnects the logged-in OS user session identified by `session_id`.
    DisconnectSession(u32),
    /// Clears persisted and live in-memory cumulative application usage history.
    ClearAppHistory,
    /// Requests a clean shutdown of the engine coordinator threads.
    Shutdown,
}

/// Tag identifying which [`EngineCommand`] produced a [`CommandEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandKind {
    /// [`EngineCommand::SetRefreshSpeed`]
    SetRefreshSpeed,
    /// [`EngineCommand::SetSearchQuery`]
    SetSearchQuery,
    /// [`EngineCommand::SetProcessViewMode`]
    SetProcessViewMode,
    /// [`EngineCommand::SetProcessSort`]
    SetProcessSort,
    /// [`EngineCommand::ToggleProcessExpanded`]
    ToggleProcessExpanded,
    /// [`EngineCommand::SetDetailsViewMode`]
    SetDetailsViewMode,
    /// [`EngineCommand::SetDetailsSort`]
    SetDetailsSort,
    /// [`EngineCommand::ToggleDetailsExpanded`]
    ToggleDetailsExpanded,
    /// [`EngineCommand::SetWindowDormant`]
    SetWindowDormant,
    /// [`EngineCommand::ForceRefresh`]
    ForceRefresh,
    /// [`EngineCommand::RunNewTask`]
    RunNewTask,
    /// [`EngineCommand::TerminateProcess`]
    TerminateProcess,
    /// [`EngineCommand::TerminateProcessTree`]
    TerminateProcessTree,
    /// [`EngineCommand::SetEfficiencyMode`]
    SetEfficiencyMode,
    /// [`EngineCommand::SetPriority`]
    SetPriority,
    /// [`EngineCommand::GetAffinity`]
    GetAffinity,
    /// [`EngineCommand::SetAffinity`]
    SetAffinity,
    /// [`EngineCommand::SetUacVirtualization`]
    SetUacVirtualization,
    /// [`EngineCommand::AnalyzeWaitChain`]
    AnalyzeWaitChain,
    /// [`EngineCommand::WriteMiniDump`]
    WriteMiniDump,
    /// [`EngineCommand::AttachDebugger`]
    AttachDebugger,
    /// [`EngineCommand::BrowseExecutable`]
    BrowseExecutable,
    /// [`EngineCommand::OpenFileLocation`]
    OpenFileLocation,
    /// [`EngineCommand::ShowFileProperties`]
    ShowFileProperties,
    /// [`EngineCommand::SearchOnline`]
    SearchOnline,
    /// [`EngineCommand::RelaunchElevated`]
    RelaunchElevated,
    /// [`EngineCommand::StartService`]
    StartService,
    /// [`EngineCommand::StopService`]
    StopService,
    /// [`EngineCommand::RestartService`]
    RestartService,
    /// [`EngineCommand::OpenServicesConsole`]
    OpenServicesConsole,
    /// [`EngineCommand::SetStartupEnabled`]
    SetStartupEnabled,
    /// [`EngineCommand::DisconnectSession`]
    DisconnectSession,
    /// [`EngineCommand::ClearAppHistory`]
    ClearAppHistory,
    /// [`EngineCommand::Shutdown`]
    Shutdown,
}

impl EngineCommand {
    /// Returns the lightweight [`CommandKind`] tag for this command.
    #[inline]
    pub const fn kind(&self) -> CommandKind {
        match self {
            Self::SetRefreshSpeed(_) => CommandKind::SetRefreshSpeed,
            Self::SetSearchQuery(_) => CommandKind::SetSearchQuery,
            Self::SetProcessViewMode(_) => CommandKind::SetProcessViewMode,
            Self::SetProcessSort { .. } => CommandKind::SetProcessSort,
            Self::ToggleProcessExpanded(_) => CommandKind::ToggleProcessExpanded,
            Self::SetDetailsViewMode(_) => CommandKind::SetDetailsViewMode,
            Self::SetDetailsSort { .. } => CommandKind::SetDetailsSort,
            Self::ToggleDetailsExpanded(_) => CommandKind::ToggleDetailsExpanded,
            Self::SetWindowDormant(_) => CommandKind::SetWindowDormant,
            Self::ForceRefresh => CommandKind::ForceRefresh,
            Self::RunNewTask(_) => CommandKind::RunNewTask,
            Self::TerminateProcess(_) => CommandKind::TerminateProcess,
            Self::TerminateProcessTree(_) => CommandKind::TerminateProcessTree,
            Self::SetEfficiencyMode { .. } => CommandKind::SetEfficiencyMode,
            Self::SetPriority { .. } => CommandKind::SetPriority,
            Self::GetAffinity(_) => CommandKind::GetAffinity,
            Self::SetAffinity { .. } => CommandKind::SetAffinity,
            Self::SetUacVirtualization { .. } => CommandKind::SetUacVirtualization,
            Self::AnalyzeWaitChain(_) => CommandKind::AnalyzeWaitChain,
            Self::WriteMiniDump { .. } => CommandKind::WriteMiniDump,
            Self::AttachDebugger(_) => CommandKind::AttachDebugger,
            Self::BrowseExecutable => CommandKind::BrowseExecutable,
            Self::OpenFileLocation(_) => CommandKind::OpenFileLocation,
            Self::ShowFileProperties(_) => CommandKind::ShowFileProperties,
            Self::SearchOnline(_) => CommandKind::SearchOnline,
            Self::RelaunchElevated(_) => CommandKind::RelaunchElevated,
            Self::StartService(_) => CommandKind::StartService,
            Self::StopService(_) => CommandKind::StopService,
            Self::RestartService(_) => CommandKind::RestartService,
            Self::OpenServicesConsole => CommandKind::OpenServicesConsole,
            Self::SetStartupEnabled { .. } => CommandKind::SetStartupEnabled,
            Self::DisconnectSession(_) => CommandKind::DisconnectSession,
            Self::ClearAppHistory => CommandKind::ClearAppHistory,
            Self::Shutdown => CommandKind::Shutdown,
        }
    }
}

/// Successful payload returned by an executed [`EngineCommand`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandSuccess {
    /// Command completed without an additional return value.
    Ack,
    /// [`EngineCommand::RunNewTask`] launched a new process (with optional PID).
    TaskLaunched {
        /// Spawned OS process identifier, when reported by the backend.
        pid: Option<u32>,
    },
    /// [`EngineCommand::TerminateProcessTree`] completed with per-process outcomes.
    ProcessTreeTerminated(TreeKillReport),
    /// [`EngineCommand::GetAffinity`] returned the target process's CPU affinity mask.
    Affinity(CpuAffinityMask),
    /// [`EngineCommand::AnalyzeWaitChain`] returned the inspected wait-chain tree roots.
    WaitChain(Vec<WaitChainNode>),
    /// [`EngineCommand::WriteMiniDump`] completed and wrote the dump file at the returned path.
    MiniDumpWritten(PathBuf),
    /// [`EngineCommand::BrowseExecutable`] completed with an optional selected file path.
    FilePicked(Option<PathBuf>),
}

/// Structured outcome of executing an [`EngineCommand`].
#[derive(Debug, Clone)]
pub enum CommandOutcome {
    /// Command succeeded with payload.
    Ok(CommandSuccess),
    /// Command failed with a structured [`PalError`] (wrapped in `Arc` for fanout).
    Err(Arc<PalError>),
}

impl CommandOutcome {
    /// Wraps a [`PalError`] into [`Self::Err`].
    #[inline]
    pub fn err(error: PalError) -> Self {
        Self::Err(Arc::new(error))
    }

    /// Returns `true` if the command succeeded.
    #[inline]
    pub const fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }

    /// Returns `true` if the command failed with [`PalError::ElevationRequired`].
    #[inline]
    pub fn is_elevation_required(&self) -> bool {
        matches!(self, Self::Err(err) if matches!(err.as_ref(), PalError::ElevationRequired))
    }

    /// Returns the success payload if [`Self::Ok`].
    #[inline]
    pub fn as_ok(&self) -> Option<&CommandSuccess> {
        match self {
            Self::Ok(ok) => Some(ok),
            Self::Err(_) => None,
        }
    }

    /// Returns a reference to the underlying [`PalError`] if [`Self::Err`].
    #[inline]
    pub fn as_error(&self) -> Option<&PalError> {
        match self {
            Self::Ok(_) => None,
            Self::Err(err) => Some(err.as_ref()),
        }
    }
}

/// Event emitted when an [`EngineCommand`] completes on the fast or slow lane.
#[derive(Debug, Clone)]
pub struct CommandEvent {
    /// Kind of command that completed.
    pub kind: CommandKind,
    /// Optional process target associated with this command.
    pub target: Option<ProcessKey>,
    /// Structured result (`Ok` or [`PalError`]).
    pub outcome: CommandOutcome,
}

impl CommandEvent {
    /// Creates a new [`CommandEvent`].
    #[inline]
    pub fn new(kind: CommandKind, target: Option<ProcessKey>, outcome: CommandOutcome) -> Self {
        Self {
            kind,
            target,
            outcome,
        }
    }

    /// Returns `true` if the command succeeded.
    #[inline]
    pub const fn is_ok(&self) -> bool {
        self.outcome.is_ok()
    }

    /// Returns `true` if the command failed with [`PalError::ElevationRequired`].
    #[inline]
    pub fn is_elevation_required(&self) -> bool {
        self.outcome.is_elevation_required()
    }

    /// Returns a reference to the underlying [`PalError`], if any.
    #[inline]
    pub fn error(&self) -> Option<&PalError> {
        self.outcome.as_error()
    }
}

/// Errors returned when submitting an [`EngineCommand`] to [`crate::EngineCoordinator`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum EngineCommandError {
    /// The bounded command queue is currently full.
    #[error("engine command queue is full")]
    QueueFull,
    /// The engine coordinator has already shut down and disconnected its command channel.
    #[error("engine coordinator has shut down")]
    Disconnected,
}

/// Internal command envelope pairing an [`EngineCommand`] with an optional oneshot reply channel.
#[derive(Debug)]
pub(crate) struct CommandEnvelope {
    pub command: EngineCommand,
    pub reply: Option<Sender<CommandEvent>>,
}

/// Multi-subscriber event fanout bus for [`CommandEvent`] notifications.
#[derive(Debug, Default)]
pub(crate) struct EventBus {
    subscribers: Mutex<Vec<Sender<CommandEvent>>>,
}

impl EventBus {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn subscribe(&self, capacity: usize) -> crossbeam_channel::Receiver<CommandEvent> {
        let (tx, rx) = crossbeam_channel::bounded(capacity.max(16));
        self.subscribers.lock().push(tx);
        rx
    }

    pub(crate) fn emit(&self, event: CommandEvent, reply: Option<Sender<CommandEvent>>) {
        if let Some(reply_tx) = reply {
            let _ = reply_tx.try_send(event.clone());
        }
        let mut subs = self.subscribers.lock();
        subs.retain(|tx| match tx.try_send(event.clone()) {
            Ok(()) | Err(crossbeam_channel::TrySendError::Full(_)) => true,
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => false,
        });
    }
}
