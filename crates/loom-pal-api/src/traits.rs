//! Zero-cost static-dispatch Platform Abstraction Layer (PAL) trait contracts.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use loom_types::{
    AdminStartupStatus, AppHistoryRecord, CapabilityFlags, CpuAffinityMask, DiskDeviceSample,
    ExeStaticAsset, FileLockHolder, HardwareStaticInfo, PlatformProfile, ProcessColdMeta,
    ProcessKey, ProcessPriority, ProcessSocketRecord, RunTaskRequest, ServiceRecord, StartupEntry,
    TaskManagerReplacementState, TaskManagerReplacementStatus, Timestamp100Ns, TreeKillReport,
    UserSessionRecord, WaitChainNode,
};

use crate::{PalError, PerfSampleScratch, ProcessSampleScratch};

/// Root compile-time monomorphized contract implemented by each OS backend.
///
/// The engine (`EngineCoordinator<B: PlatformBackend>`) binds to a concrete backend
/// statically via `loom-pal`'s `#[cfg(target_os)]` type alias, avoiding dynamic
/// dispatch (`dyn Trait`) and boxed futures on hot paths.
pub trait PlatformBackend: Send + Sync + 'static {
    /// Platform-specific compact per-process extension payload (`()` for zero overhead).
    type ProcessExt: Copy + Default + Send + Sync + 'static;

    /// Fast-lane pure-memory kernel sampler running on the dedicated sampling thread.
    type FastSampler: FastSystemSampler<Ext = Self::ProcessExt>;

    /// Slow-lane bulkhead metadata prober for disk/Shell/PE/IOCTL queries.
    type SlowProber: SlowMetadataProber;

    /// On-demand system service controller.
    type ServiceMgr: ServiceManager;

    /// On-demand startup applications manager.
    type StartupMgr: StartupManager;

    /// On-demand user sessions manager.
    type SessionMgr: SessionManager;

    /// On-demand application resource history reader.
    type HistoryReader: AppHistoryReader;

    /// System process control, diagnostics, and elevation operator.
    type Operator: SystemOperator;

    /// Static platform identity profile advertised by this backend.
    const PLATFORM: PlatformProfile;

    /// Compile-time capability bitmask advertised by this backend.
    const CAPABILITIES: CapabilityFlags;

    /// Boosts the current Loom process priority early during startup so the UI
    /// remains responsive under 100% CPU contention.
    fn boost_self_priority();

    /// Toggles native always-on-top z-order for Loom's primary application window.
    fn set_main_window_always_on_top(_enabled: bool) {}

    /// Returns `true` when Loom's primary native window is currently minimized.
    fn is_main_window_minimized() -> bool {
        false
    }

    /// Restores Loom's primary native window (if minimized) and brings it to foreground focus.
    fn restore_and_focus_main_window() {}

    /// Reads the operating system's active accent color as `(r, g, b)`, if available.
    fn system_accent_color() -> Option<(u8, u8, u8)> {
        None
    }

    /// Initializes the platform backend and any lightweight static hardware caches.
    fn initialize() -> Result<Self, PalError>
    where
        Self: Sized;

    /// Creates a fast-lane sampler instance owned by the dedicated fast sampling thread.
    fn create_fast_sampler(&self) -> Result<Self::FastSampler, PalError>;

    /// Creates a slow-lane metadata prober instance for the slow-lane bulkhead pool.
    fn create_slow_prober(&self) -> Result<Self::SlowProber, PalError>;

    /// Returns a shared reference to the service manager.
    fn service_manager(&self) -> &Self::ServiceMgr;

    /// Returns a shared reference to the startup applications manager.
    fn startup_manager(&self) -> &Self::StartupMgr;

    /// Returns a shared reference to the user session manager.
    fn session_manager(&self) -> &Self::SessionMgr;

    /// Returns a shared reference to the application history reader.
    fn history_reader(&self) -> &Self::HistoryReader;

    /// Returns a shared reference to the system control operator.
    fn operator(&self) -> &Self::Operator;
}

/// Fast-lane kernel sampling contract (sub-millisecond, pure-memory calls only).
///
/// Implementations must never perform blocking disk I/O, network requests, WMI/PDH
/// calls, or cross-process synchronous window messaging.
pub trait FastSystemSampler: Send {
    /// Platform-specific compact extension payload stored inline in [`loom_types::ProcessHotSample`].
    type Ext: Copy + Default + Send + Sync + 'static;

    /// Captures a complete process snapshot into `scratch` and returns the sample timestamp.
    fn sample_processes(
        &mut self,
        scratch: &mut ProcessSampleScratch<Self::Ext>,
    ) -> Result<Timestamp100Ns, PalError>;

    /// Captures system-wide hardware performance counters into `scratch` and returns the timestamp.
    fn sample_performance(
        &mut self,
        scratch: &mut PerfSampleScratch,
    ) -> Result<Timestamp100Ns, PalError>;

    /// Enables or disables optional elevated per-process network byte attribution.
    fn set_per_process_network_enabled(&mut self, _enabled: bool) {}
}

/// Slow-lane bulkhead metadata prober for operations that may touch disk or Shell APIs.
///
/// Executed exclusively on the bounded slow-lane worker pool with timeout / circuit-breaker
/// protection so unresponsive drives or remote paths never stall the fast lane or UI.
pub trait SlowMetadataProber: Send + Sync {
    /// Resolves infrequently changing metadata (exe path, command line, user, arch, UAC)
    /// for a live process validated by `key`.
    fn probe_process(&self, key: ProcessKey) -> Result<ProcessColdMeta, PalError>;

    /// Extracts path-level static binary metadata (publisher, description, DPI-matched RGBA icon)
    /// for `exe_path`, bypassing deep I/O for UNC / network-mapped drives.
    fn probe_exe_asset(&self, exe_path: &str) -> Result<ExeStaticAsset, PalError>;

    /// Probes static CPU brand/topology/cache metadata and SMBIOS physical memory hardware info.
    ///
    /// Executed once on demand on the slow-lane bulkhead pool when `MonitorDomain::Performance`
    /// is leased.
    fn probe_hardware_static_info(&self) -> Result<HardwareStaticInfo, PalError> {
        Ok(HardwareStaticInfo::default())
    }

    /// Probes physical disk devices (`0..N`) for active time, throughput, average response
    /// latency, and media metadata behind per-device circuit-breaker isolation.
    fn probe_disk_performance(&self) -> Result<Vec<DiskDeviceSample>, PalError> {
        Ok(Vec::new())
    }

    /// Lists currently listening TCP and UDP socket endpoints across all processes.
    fn list_listening_ports(&self) -> Result<Vec<ProcessSocketRecord>, PalError> {
        Ok(Vec::new())
    }
}

/// On-demand system service enumeration and lifecycle control contract.
pub trait ServiceManager: Send + Sync {
    /// Enumerates all system services visible to the current user.
    fn list_services(&self) -> Result<Vec<ServiceRecord>, PalError>;

    /// Starts the service identified by `name`.
    fn start_service(&self, name: &str) -> Result<(), PalError>;

    /// Stops the service identified by `name`.
    fn stop_service(&self, name: &str) -> Result<(), PalError>;

    /// Restarts the service identified by `name`.
    fn restart_service(&self, name: &str) -> Result<(), PalError>;

    /// Launches the native OS services management console (e.g., `services.msc`).
    fn open_services_console(&self) -> Result<(), PalError>;
}

/// On-demand startup applications enumeration and toggle contract.
pub trait StartupManager: Send + Sync {
    /// Returns `(last_bios_time_ms, startup_entries)`.
    fn list_startup_entries(&self) -> Result<(Option<u64>, Vec<StartupEntry>), PalError>;

    /// Enables or disables the startup entry identified by `id`.
    fn set_startup_enabled(&self, id: &str, enabled: bool) -> Result<(), PalError>;
}

/// On-demand OS user session enumeration and management contract.
pub trait SessionManager: Send + Sync {
    /// Lists active and disconnected user sessions.
    fn list_sessions(&self) -> Result<Vec<UserSessionRecord>, PalError>;

    /// Disconnects the user session identified by `session_id`.
    fn disconnect_session(&self, session_id: u32) -> Result<(), PalError>;
}

/// On-demand cumulative application resource history reader contract.
pub trait AppHistoryReader: Send + Sync {
    /// Returns `(since_epoch_secs, records)`.
    fn read_history(&self) -> Result<(Option<u64>, Vec<AppHistoryRecord>), PalError>;

    /// Clears accumulated application usage history.
    fn clear_history(&self) -> Result<(), PalError>;

    /// Atomically persists `(since_epoch_secs, records)` to the backing history store.
    fn save_history(
        &self,
        _since_epoch_secs: u64,
        _records: &[AppHistoryRecord],
    ) -> Result<(), PalError> {
        Ok(())
    }
}

/// Process control, diagnostics, shell integration, and elevation contract.
///
/// Operations denied due to insufficient privilege must return
/// [`PalError::ElevationRequired`] so the UI can offer an on-demand UAC elevation prompt.
pub trait SystemOperator: Send + Sync {
    /// Launches a new task/process, returning the spawned PID when available.
    fn run_new_task(&self, req: &RunTaskRequest) -> Result<Option<u32>, PalError>;

    /// Terminates a single process after verifying its [`ProcessKey`] identity.
    fn terminate_process(&self, key: ProcessKey) -> Result<(), PalError>;

    /// Terminates an ordered slice of process keys (typically bottom-up leaves-to-root).
    fn terminate_process_tree(&self, targets: &[ProcessKey]) -> Result<TreeKillReport, PalError>;

    /// Enables or disables Efficiency Mode (`EcoQoS`) for `key`.
    fn set_efficiency_mode(&self, key: ProcessKey, enabled: bool) -> Result<(), PalError>;

    /// Sets the scheduling priority class of `key`.
    fn set_priority(&self, key: ProcessKey, priority: ProcessPriority) -> Result<(), PalError>;

    /// Queries the current CPU affinity mask of `key`.
    fn get_affinity(&self, key: ProcessKey) -> Result<CpuAffinityMask, PalError>;

    /// Applies a new CPU affinity mask to `key`.
    fn set_affinity(&self, key: ProcessKey, mask: &CpuAffinityMask) -> Result<(), PalError>;

    /// Enables or disables UAC virtualization for `key`.
    fn set_uac_virtualization(&self, key: ProcessKey, enabled: bool) -> Result<(), PalError>;

    /// Inspects the cross-process wait chain / deadlock graph for `key`.
    fn analyze_wait_chain(&self, key: ProcessKey) -> Result<Vec<WaitChainNode>, PalError>;

    /// Streams a process memory minidump to `dest`, checking `cancel` periodically.
    fn write_minidump(
        &self,
        key: ProcessKey,
        dest: &Path,
        cancel: &AtomicBool,
    ) -> Result<(), PalError>;

    /// Launches the OS-configured JIT debugger attached to `key`.
    fn attach_debugger(&self, _key: ProcessKey) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "attach debugger is not supported on this platform",
        ))
    }

    /// Suspends all threads of a non-critical process verified by `key`.
    fn suspend_process(&self, _key: ProcessKey) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "suspend process is not supported on this platform",
        ))
    }

    /// Resumes all threads of a previously suspended process verified by `key`.
    fn resume_process(&self, _key: ProcessKey) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "resume process is not supported on this platform",
        ))
    }

    /// Safely restarts the desktop shell / file explorer (e.g., `explorer.exe`).
    fn restart_explorer(&self) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "restart explorer is not supported on this platform",
        ))
    }

    /// Queries processes currently holding open file or directory locks on `path`.
    fn find_file_locks(&self, _path: &Path) -> Result<Vec<FileLockHolder>, PalError> {
        Err(PalError::Unsupported(
            "file lock finder is not supported on this platform",
        ))
    }

    /// Queries the current read-only status of OS-level system Task Manager replacement.
    fn task_manager_replacement_status(&self) -> Result<TaskManagerReplacementStatus, PalError> {
        Ok(TaskManagerReplacementStatus {
            state: TaskManagerReplacementState::Unsupported,
            can_enable: false,
            can_restore: false,
            conflict_detail: None,
            recovery_path: None,
        })
    }

    /// Enables or restores OS-level system Task Manager replacement (e.g., Windows IFEO).
    fn set_task_manager_replacement(
        &self,
        _enabled: bool,
    ) -> Result<TaskManagerReplacementStatus, PalError> {
        Err(PalError::Unsupported(
            "system task manager replacement is not supported on this platform",
        ))
    }

    /// Launches the OS built-in Task Manager directly, bypassing any active replacement entry.
    fn open_native_task_manager(&self) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "opening native task manager is not supported on this platform",
        ))
    }

    /// Queries the current read-only status of persistent administrator startup.
    fn admin_startup_status(&self) -> Result<AdminStartupStatus, PalError> {
        Ok(AdminStartupStatus::default())
    }

    /// Enables or disables persistent administrator startup (e.g., Windows Task Scheduler).
    fn set_admin_startup(&self, _enabled: bool) -> Result<AdminStartupStatus, PalError> {
        Err(PalError::Unsupported(
            "persistent administrator startup is not supported on this platform",
        ))
    }

    /// Relaunches Loom with elevated privileges and instructs the new instance
    /// to restore `restore_route`.
    fn relaunch_self_elevated(&self, restore_route: &str) -> Result<(), PalError>;

    /// Opens the OS file explorer and selects `path`.
    fn open_file_location(&self, path: &Path) -> Result<(), PalError>;

    /// Opens the native OS properties dialog for `path`.
    fn show_file_properties(&self, path: &Path) -> Result<(), PalError>;

    /// Opens the user's default browser searching online for `query`.
    fn search_online(&self, query: &str) -> Result<(), PalError>;

    /// Opens a native executable file picker dialog for the "Run new task" flow.
    fn browse_executable(&self) -> Result<Option<PathBuf>, PalError>;

    /// Returns `true` if the current Loom process is running with elevated privileges.
    fn is_elevated(&self) -> bool;

    /// Returns the number of logical CPUs on the host machine.
    fn logical_cpu_count(&self) -> u32;
}
