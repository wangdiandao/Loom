//! Linux Platform Abstraction Layer (PAL) backend skeleton for Loom.
//!
//! Planned for milestone `0.2.0` (`procfs` / `sysfs` / `netlink` / `drm` / `systemd`).

#![warn(missing_docs)]

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use loom_pal_api::{
    AppHistoryReader, FastSystemSampler, PalError, PerfSampleScratch, PlatformBackend,
    ProcessSampleScratch, ServiceManager, SessionManager, SlowMetadataProber, StartupManager,
    SystemOperator,
};
use loom_types::{
    AppHistoryRecord, CapabilityFlags, CpuAffinityMask, ExeStaticAsset, PlatformProfile,
    ProcessColdMeta, ProcessKey, ProcessPriority, RunTaskRequest, ServiceRecord, StartupEntry,
    Timestamp100Ns, TreeKillReport, UserSessionRecord, WaitChainNode,
};

/// Compile-time placeholder backend for Linux (`0.2.0`).
#[derive(Debug, Default)]
pub struct LinuxBackend {
    service_mgr: LinuxStub,
    startup_mgr: LinuxStub,
    session_mgr: LinuxStub,
    history_reader: LinuxStub,
    operator: LinuxStub,
}

/// Placeholder unit implementing stub PAL sub-traits for Linux.
#[derive(Debug, Default)]
pub struct LinuxStub;

impl PlatformBackend for LinuxBackend {
    type ProcessExt = ();
    type FastSampler = LinuxStub;
    type SlowProber = LinuxStub;
    type ServiceMgr = LinuxStub;
    type StartupMgr = LinuxStub;
    type SessionMgr = LinuxStub;
    type HistoryReader = LinuxStub;
    type Operator = LinuxStub;

    const PLATFORM: PlatformProfile = PlatformProfile::LINUX;

    const CAPABILITIES: CapabilityFlags = CapabilityFlags::empty();

    fn boost_self_priority() {}

    fn initialize() -> Result<Self, PalError> {
        Ok(Self::default())
    }

    fn create_fast_sampler(&self) -> Result<Self::FastSampler, PalError> {
        Ok(LinuxStub)
    }

    fn create_slow_prober(&self) -> Result<Self::SlowProber, PalError> {
        Ok(LinuxStub)
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

impl FastSystemSampler for LinuxStub {
    type Ext = ();

    fn sample_processes(
        &mut self,
        scratch: &mut ProcessSampleScratch<Self::Ext>,
    ) -> Result<Timestamp100Ns, PalError> {
        scratch.clear();
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn sample_performance(
        &mut self,
        scratch: &mut PerfSampleScratch,
    ) -> Result<Timestamp100Ns, PalError> {
        scratch.clear();
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }
}

impl SlowMetadataProber for LinuxStub {
    fn probe_process(&self, _key: ProcessKey) -> Result<ProcessColdMeta, PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn probe_exe_asset(&self, _exe_path: &str) -> Result<ExeStaticAsset, PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }
}

impl ServiceManager for LinuxStub {
    fn list_services(&self) -> Result<Vec<ServiceRecord>, PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn start_service(&self, _name: &str) -> Result<(), PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn stop_service(&self, _name: &str) -> Result<(), PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn restart_service(&self, _name: &str) -> Result<(), PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn open_services_console(&self) -> Result<(), PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }
}

impl StartupManager for LinuxStub {
    fn list_startup_entries(&self) -> Result<(Option<u64>, Vec<StartupEntry>), PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn set_startup_enabled(&self, _id: &str, _enabled: bool) -> Result<(), PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }
}

impl SessionManager for LinuxStub {
    fn list_sessions(&self) -> Result<Vec<UserSessionRecord>, PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn disconnect_session(&self, _session_id: u32) -> Result<(), PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }
}

impl AppHistoryReader for LinuxStub {
    fn read_history(&self) -> Result<(Option<u64>, Vec<AppHistoryRecord>), PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn clear_history(&self) -> Result<(), PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }
}

impl SystemOperator for LinuxStub {
    fn run_new_task(&self, _req: &RunTaskRequest) -> Result<Option<u32>, PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn terminate_process(&self, _key: ProcessKey) -> Result<(), PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn terminate_process_tree(&self, _targets: &[ProcessKey]) -> Result<TreeKillReport, PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn set_efficiency_mode(&self, _key: ProcessKey, _enabled: bool) -> Result<(), PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn set_priority(&self, _key: ProcessKey, _priority: ProcessPriority) -> Result<(), PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn get_affinity(&self, _key: ProcessKey) -> Result<CpuAffinityMask, PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn set_affinity(&self, _key: ProcessKey, _mask: &CpuAffinityMask) -> Result<(), PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn set_uac_virtualization(&self, _key: ProcessKey, _enabled: bool) -> Result<(), PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn analyze_wait_chain(&self, _key: ProcessKey) -> Result<Vec<WaitChainNode>, PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn write_minidump(
        &self,
        _key: ProcessKey,
        _dest: &Path,
        _cancel: &AtomicBool,
    ) -> Result<(), PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn relaunch_self_elevated(&self, _restore_route: &str) -> Result<(), PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn open_file_location(&self, _path: &Path) -> Result<(), PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn show_file_properties(&self, _path: &Path) -> Result<(), PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn search_online(&self, _query: &str) -> Result<(), PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn browse_executable(&self) -> Result<Option<PathBuf>, PalError> {
        Err(PalError::Unsupported("linux backend planned for 0.2.0"))
    }

    fn is_elevated(&self) -> bool {
        false
    }

    fn logical_cpu_count(&self) -> u32 {
        std::thread::available_parallelism()
            .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
            .unwrap_or(1)
    }
}

#[cfg(test)]
mod tests {
    use loom_types::TaskManagerReplacementState;

    use super::*;

    #[test]
    fn linux_backend_advertises_platform_profile_and_noop_window_helpers() {
        assert_eq!(std::mem::size_of::<LinuxBackend>(), 0);
        assert_eq!(LinuxBackend::PLATFORM, PlatformProfile::LINUX);
        assert_eq!(LinuxBackend::CAPABILITIES, CapabilityFlags::empty());
        assert!(!LinuxBackend::is_main_window_minimized());
        assert_eq!(LinuxBackend::system_accent_color(), None);
        LinuxBackend::set_main_window_always_on_top(true);
        LinuxBackend::restore_and_focus_main_window();

        let backend = LinuxBackend::initialize().expect("LinuxBackend::initialize");
        assert!(backend.operator().logical_cpu_count() >= 1);
        assert!(!backend.operator().is_elevated());

        let mut fast = backend.create_fast_sampler().expect("create_fast_sampler");
        let mut proc_scratch = ProcessSampleScratch::new();
        let _ = proc_scratch.push_str("stale");
        assert!(matches!(
            fast.sample_processes(&mut proc_scratch),
            Err(PalError::Unsupported(_))
        ));
        assert!(proc_scratch.string_buf.is_empty());

        let mut perf_scratch = PerfSampleScratch::new();
        perf_scratch.per_core_permille.push(500);
        assert!(matches!(
            fast.sample_performance(&mut perf_scratch),
            Err(PalError::Unsupported(_))
        ));
        assert!(perf_scratch.per_core_permille.is_empty());

        let slow = backend.create_slow_prober().expect("create_slow_prober");
        assert!(slow.probe_disk_performance().unwrap().is_empty());
        assert!(slow.list_listening_ports().unwrap().is_empty());

        let tm = backend
            .operator()
            .task_manager_replacement_status()
            .unwrap();
        assert_eq!(tm.state, TaskManagerReplacementState::Unsupported);
        let admin = backend.operator().admin_startup_status().unwrap();
        assert!(!admin.supported);
    }
}
