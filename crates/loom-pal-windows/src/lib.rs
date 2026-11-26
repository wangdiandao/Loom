//! Windows native Platform Abstraction Layer (PAL) backend for Loom.
//!
//! Strictly observes the Loom Windows anti-hang red lines:
//! - No WMI or PDH dependencies.
//! - No cross-process `GetWindowTextW` or blocking `SendMessageW`.
//! - No cross-process `ReadProcessMemory` for PEB inspection.
//! - Slow disk/Shell/PE/IOCTL queries isolated behind a bounded bulkhead with a `300 ms` circuit breaker.

#![warn(missing_docs)]

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use loom_pal_api::{
    AppHistoryReader, FastSystemSampler, PalError, PerfSampleScratch, PlatformBackend,
    ProcessSampleScratch, ServiceManager, SessionManager, SlowMetadataProber, StartupManager,
    SystemOperator,
};
use loom_types::{
    AppHistoryRecord, CapabilityFlags, CpuAffinityMask, ExeStaticAsset, ProcessColdMeta,
    ProcessKey, ProcessPriority, RunTaskRequest, ServiceRecord, StartupEntry, Timestamp100Ns,
    TreeKillReport, UserSessionRecord, WaitChainNode,
};

/// Windows PAL backend skeleton (to be populated with native `ntdll` / `D3DKMT` / `IOCTL` modules).
#[derive(Debug, Default)]
pub struct WindowsBackend {
    service_mgr: WindowsServiceManager,
    startup_mgr: WindowsStartupManager,
    session_mgr: WindowsSessionManager,
    history_reader: WindowsAppHistoryReader,
    operator: WindowsSystemOperator,
}

/// Windows fast-lane pure-memory kernel sampler stub.
#[derive(Debug, Default)]
pub struct WindowsFastSampler;

/// Windows slow-lane bulkhead metadata prober stub.
#[derive(Debug, Default)]
pub struct WindowsSlowProber;

/// Windows SCM service manager stub.
#[derive(Debug, Default)]
pub struct WindowsServiceManager;

/// Windows startup application manager stub.
#[derive(Debug, Default)]
pub struct WindowsStartupManager;

/// Windows WTS user session manager stub.
#[derive(Debug, Default)]
pub struct WindowsSessionManager;

/// Windows application usage history reader stub.
#[derive(Debug, Default)]
pub struct WindowsAppHistoryReader;

/// Windows process control and diagnostics operator stub.
#[derive(Debug, Default)]
pub struct WindowsSystemOperator;

impl PlatformBackend for WindowsBackend {
    type ProcessExt = ();
    type FastSampler = WindowsFastSampler;
    type SlowProber = WindowsSlowProber;
    type ServiceMgr = WindowsServiceManager;
    type StartupMgr = WindowsStartupManager;
    type SessionMgr = WindowsSessionManager;
    type HistoryReader = WindowsAppHistoryReader;
    type Operator = WindowsSystemOperator;

    const CAPABILITIES: CapabilityFlags = CapabilityFlags::all();

    fn boost_self_priority() {}

    fn initialize() -> Result<Self, PalError> {
        Ok(Self::default())
    }

    fn create_fast_sampler(&self) -> Result<Self::FastSampler, PalError> {
        Ok(WindowsFastSampler)
    }

    fn create_slow_prober(&self) -> Result<Self::SlowProber, PalError> {
        Ok(WindowsSlowProber)
    }

    fn service_manager(&self) -> &Self::ServiceMgr {
        &self.service_mgr
    }

    fn startup_manager(&self) -> &Self::StartupMgr {
        &self.startup_mgr
    }

    fn session_manager(&self) -> &Self::SessionMgr {
        &self.session_mgr
    }

    fn history_reader(&self) -> &Self::HistoryReader {
        &self.history_reader
    }

    fn operator(&self) -> &Self::Operator {
        &self.operator
    }
}

impl FastSystemSampler for WindowsFastSampler {
    type Ext = ();

    fn sample_processes(
        &mut self,
        scratch: &mut ProcessSampleScratch<Self::Ext>,
    ) -> Result<Timestamp100Ns, PalError> {
        scratch.clear();
        Err(PalError::Unsupported(
            "windows fast process sampler not yet implemented",
        ))
    }

    fn sample_performance(
        &mut self,
        scratch: &mut PerfSampleScratch,
    ) -> Result<Timestamp100Ns, PalError> {
        scratch.clear();
        Err(PalError::Unsupported(
            "windows fast performance sampler not yet implemented",
        ))
    }
}

impl SlowMetadataProber for WindowsSlowProber {
    fn probe_process(&self, _key: ProcessKey) -> Result<ProcessColdMeta, PalError> {
        Err(PalError::Unsupported(
            "windows slow process prober not yet implemented",
        ))
    }

    fn probe_exe_asset(&self, _exe_path: &str) -> Result<ExeStaticAsset, PalError> {
        Err(PalError::Unsupported(
            "windows exe asset prober not yet implemented",
        ))
    }
}

impl ServiceManager for WindowsServiceManager {
    fn list_services(&self) -> Result<Vec<ServiceRecord>, PalError> {
        Err(PalError::Unsupported(
            "windows service enumeration not yet implemented",
        ))
    }

    fn start_service(&self, _name: &str) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "windows service control not yet implemented",
        ))
    }

    fn stop_service(&self, _name: &str) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "windows service control not yet implemented",
        ))
    }

    fn restart_service(&self, _name: &str) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "windows service control not yet implemented",
        ))
    }

    fn open_services_console(&self) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "windows services console not yet implemented",
        ))
    }
}

impl StartupManager for WindowsStartupManager {
    fn list_startup_entries(&self) -> Result<(Option<u64>, Vec<StartupEntry>), PalError> {
        Err(PalError::Unsupported(
            "windows startup enumeration not yet implemented",
        ))
    }

    fn set_startup_enabled(&self, _id: &str, _enabled: bool) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "windows startup toggle not yet implemented",
        ))
    }
}

impl SessionManager for WindowsSessionManager {
    fn list_sessions(&self) -> Result<Vec<UserSessionRecord>, PalError> {
        Err(PalError::Unsupported(
            "windows session enumeration not yet implemented",
        ))
    }

    fn disconnect_session(&self, _session_id: u32) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "windows session disconnect not yet implemented",
        ))
    }
}

impl AppHistoryReader for WindowsAppHistoryReader {
    fn read_history(&self) -> Result<(Option<u64>, Vec<AppHistoryRecord>), PalError> {
        Err(PalError::Unsupported(
            "windows app history reader not yet implemented",
        ))
    }

    fn clear_history(&self) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "windows app history clear not yet implemented",
        ))
    }
}

impl SystemOperator for WindowsSystemOperator {
    fn run_new_task(&self, _req: &RunTaskRequest) -> Result<Option<u32>, PalError> {
        Err(PalError::Unsupported("run_new_task not yet implemented"))
    }

    fn terminate_process(&self, _key: ProcessKey) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "terminate_process not yet implemented",
        ))
    }

    fn terminate_process_tree(&self, _targets: &[ProcessKey]) -> Result<TreeKillReport, PalError> {
        Err(PalError::Unsupported(
            "terminate_process_tree not yet implemented",
        ))
    }

    fn set_efficiency_mode(&self, _key: ProcessKey, _enabled: bool) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "set_efficiency_mode not yet implemented",
        ))
    }

    fn set_priority(&self, _key: ProcessKey, _priority: ProcessPriority) -> Result<(), PalError> {
        Err(PalError::Unsupported("set_priority not yet implemented"))
    }

    fn get_affinity(&self, _key: ProcessKey) -> Result<CpuAffinityMask, PalError> {
        Err(PalError::Unsupported("get_affinity not yet implemented"))
    }

    fn set_affinity(&self, _key: ProcessKey, _mask: &CpuAffinityMask) -> Result<(), PalError> {
        Err(PalError::Unsupported("set_affinity not yet implemented"))
    }

    fn set_uac_virtualization(&self, _key: ProcessKey, _enabled: bool) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "set_uac_virtualization not yet implemented",
        ))
    }

    fn analyze_wait_chain(&self, _key: ProcessKey) -> Result<Vec<WaitChainNode>, PalError> {
        Err(PalError::Unsupported(
            "analyze_wait_chain not yet implemented",
        ))
    }

    fn write_minidump(
        &self,
        _key: ProcessKey,
        _dest: &Path,
        _cancel: &AtomicBool,
    ) -> Result<(), PalError> {
        Err(PalError::Unsupported("write_minidump not yet implemented"))
    }

    fn relaunch_self_elevated(&self, _restore_route: &str) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "relaunch_self_elevated not yet implemented",
        ))
    }

    fn open_file_location(&self, _path: &Path) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "open_file_location not yet implemented",
        ))
    }

    fn show_file_properties(&self, _path: &Path) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "show_file_properties not yet implemented",
        ))
    }

    fn search_online(&self, _query: &str) -> Result<(), PalError> {
        Err(PalError::Unsupported("search_online not yet implemented"))
    }

    fn browse_executable(&self) -> Result<Option<PathBuf>, PalError> {
        Err(PalError::Unsupported(
            "browse_executable not yet implemented",
        ))
    }

    fn is_elevated(&self) -> bool {
        false
    }

    fn logical_cpu_count(&self) -> u32 {
        std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1)
    }
}
