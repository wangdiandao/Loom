//! Windows process control, shell integration, elevation, and diagnostics operator.
//!
//! Every process-mutating method verifies [`ProcessKey::create_time_100ns`] before
//! acting, closes target handles immediately via an RAII handle guard, and maps Win32
//! `ERROR_ACCESS_DENIED` to [`PalError::ElevationRequired`].

mod affinity;
mod process;
mod shell;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use loom_pal_api::{PalError, SystemOperator};
use loom_types::{
    CpuAffinityMask, FileLockHolder, ProcessKey, ProcessPriority, RunTaskRequest, TreeKillReport,
    WaitChainNode,
};

use crate::fast_sampler::query_logical_cpu_count;

/// Windows implementation of [`SystemOperator`].
#[derive(Debug, Default)]
pub struct WindowsSystemOperator;

impl WindowsSystemOperator {
    /// Creates a new Windows system operator.
    pub fn new() -> Self {
        Self
    }
}

impl SystemOperator for WindowsSystemOperator {
    fn run_new_task(&self, req: &RunTaskRequest) -> Result<Option<u32>, PalError> {
        shell::run_new_task(self.is_elevated(), req)
    }

    fn terminate_process(&self, key: ProcessKey) -> Result<(), PalError> {
        process::terminate_process(key)
    }

    fn terminate_process_tree(&self, targets: &[ProcessKey]) -> Result<TreeKillReport, PalError> {
        process::terminate_process_tree(targets)
    }

    fn set_efficiency_mode(&self, key: ProcessKey, enabled: bool) -> Result<(), PalError> {
        process::set_efficiency_mode(key, enabled)
    }

    fn set_priority(&self, key: ProcessKey, priority: ProcessPriority) -> Result<(), PalError> {
        process::set_priority(key, priority)
    }

    fn get_affinity(&self, key: ProcessKey) -> Result<CpuAffinityMask, PalError> {
        affinity::get_affinity(key, self.logical_cpu_count())
    }

    fn set_affinity(&self, key: ProcessKey, mask: &CpuAffinityMask) -> Result<(), PalError> {
        affinity::set_affinity(key, mask, self.logical_cpu_count())
    }

    fn set_uac_virtualization(&self, key: ProcessKey, enabled: bool) -> Result<(), PalError> {
        process::set_uac_virtualization(key, enabled)
    }

    fn analyze_wait_chain(&self, key: ProcessKey) -> Result<Vec<WaitChainNode>, PalError> {
        crate::diagnostics::analyze_wait_chain(key)
    }

    fn write_minidump(
        &self,
        key: ProcessKey,
        dest: &Path,
        cancel: &AtomicBool,
    ) -> Result<(), PalError> {
        crate::diagnostics::write_minidump(key, dest, cancel)
    }

    fn attach_debugger(&self, key: ProcessKey) -> Result<(), PalError> {
        shell::attach_debugger(key)
    }

    fn suspend_process(&self, key: ProcessKey) -> Result<(), PalError> {
        process::suspend_or_resume_process(key, true)
    }

    fn resume_process(&self, key: ProcessKey) -> Result<(), PalError> {
        process::suspend_or_resume_process(key, false)
    }

    fn restart_explorer(&self) -> Result<(), PalError> {
        shell::restart_explorer(self.is_elevated())
    }

    fn find_file_locks(&self, path: &Path) -> Result<Vec<FileLockHolder>, PalError> {
        crate::filelock::find_file_locks(path)
    }

    fn task_manager_replacement_status(
        &self,
    ) -> Result<loom_types::TaskManagerReplacementStatus, PalError> {
        crate::taskmgr::status()
    }

    fn set_task_manager_replacement(
        &self,
        enabled: bool,
    ) -> Result<loom_types::TaskManagerReplacementStatus, PalError> {
        crate::taskmgr::set_replacement(enabled)
    }

    fn open_native_task_manager(&self) -> Result<(), PalError> {
        crate::taskmgr::open_native()
    }

    fn admin_startup_status(&self) -> Result<loom_types::AdminStartupStatus, PalError> {
        crate::admin::status()
    }

    fn set_admin_startup(&self, enabled: bool) -> Result<loom_types::AdminStartupStatus, PalError> {
        crate::admin::set_enabled(enabled)
    }

    fn relaunch_self_elevated(&self, restore_route: &str) -> Result<(), PalError> {
        shell::relaunch_self_elevated(restore_route)
    }

    fn open_file_location(&self, path: &Path) -> Result<(), PalError> {
        shell::open_file_location(path)
    }

    fn show_file_properties(&self, path: &Path) -> Result<(), PalError> {
        shell::show_file_properties(path)
    }

    fn search_online(&self, query: &str) -> Result<(), PalError> {
        shell::search_online(query)
    }

    fn browse_executable(&self) -> Result<Option<PathBuf>, PalError> {
        shell::browse_executable()
    }

    fn is_elevated(&self) -> bool {
        shell::is_elevated()
    }

    fn logical_cpu_count(&self) -> u32 {
        query_logical_cpu_count()
    }
}
