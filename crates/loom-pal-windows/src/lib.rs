//! Windows native Platform Abstraction Layer (PAL) backend for Loom.
//!
//! Strictly observes the Loom Windows anti-hang red lines:
//! - No WMI or PDH dependencies.
//! - No cross-process `GetWindowTextW` or blocking `SendMessageW` (uses exclusively
//!   `InternalGetWindowText`, `IsHungAppWindow`, and `DwmGetWindowAttribute`).
//! - No cross-process `ReadProcessMemory` for PEB inspection (command lines use
//!   `NtQueryInformationProcess(ProcessCommandLineInformation)`).
//! - Slow disk/Shell/PE/IOCTL queries isolated behind a `300 ms` [`CircuitBreaker`]
//!   with mandatory UNC / `DRIVE_REMOTE` network drive bypass.
//! - Process handles are never retained across ticks; every handle is scoped to an
//!   RAII guard and validated against [`loom_types::ProcessKey::create_time_100ns`].

#![warn(missing_docs)]

pub mod admin;
pub mod app_history;
pub mod circuit_breaker;
pub mod diagnostics;
pub(crate) mod etw_net;
pub mod fast_sampler;
pub mod filelock;
pub(crate) mod handle;
pub mod hotkey;
pub(crate) mod install_guard;
pub mod instance;
pub mod operator;
pub mod perf;
pub mod ports;
pub mod services;
pub mod slow_prober;
pub mod startup;
pub mod taskmgr;
pub mod users;
pub mod window;

#[cfg(test)]
mod tests;

pub use app_history::WindowsAppHistoryReader;
pub use circuit_breaker::CircuitBreaker;
pub use fast_sampler::WindowsFastSampler;
pub use hotkey::{WindowsGlobalHotkeyController, parse_hotkey_spec};
pub use instance::{ActivationCallback, WindowsSingleInstance};
pub use operator::WindowsSystemOperator;
pub use services::WindowsServiceManager;
pub use slow_prober::WindowsSlowProber;
pub use startup::WindowsStartupManager;
pub use users::WindowsSessionManager;

use loom_pal_api::{PalError, PlatformBackend};
use loom_types::{CapabilityFlags, PlatformProfile};

/// Windows native PAL backend combining dual-lane sampling with system management and process control.
#[derive(Debug, Default)]
pub struct WindowsBackend {
    service_mgr: WindowsServiceManager,
    startup_mgr: WindowsStartupManager,
    session_mgr: WindowsSessionManager,
    history_reader: WindowsAppHistoryReader,
    operator: WindowsSystemOperator,
}

impl PlatformBackend for WindowsBackend {
    type ProcessExt = ();
    type FastSampler = WindowsFastSampler;
    type SlowProber = WindowsSlowProber;
    type ServiceMgr = WindowsServiceManager;
    type StartupMgr = WindowsStartupManager;
    type SessionMgr = WindowsSessionManager;
    type HistoryReader = WindowsAppHistoryReader;
    type Operator = WindowsSystemOperator;

    const PLATFORM: PlatformProfile = PlatformProfile::WINDOWS;

    const CAPABILITIES: CapabilityFlags = CapabilityFlags::ECO_MODE
        .union(CapabilityFlags::CPU_AFFINITY)
        .union(CapabilityFlags::PROCESS_PRIORITY)
        .union(CapabilityFlags::UAC_VIRTUALIZATION)
        .union(CapabilityFlags::WAIT_CHAIN)
        .union(CapabilityFlags::MINIDUMP)
        .union(CapabilityFlags::SERVICES)
        .union(CapabilityFlags::STARTUP_APPS)
        .union(CapabilityFlags::USER_SESSIONS)
        .union(CapabilityFlags::APP_HISTORY)
        .union(CapabilityFlags::ELEVATION_RELAUNCH)
        .union(CapabilityFlags::ALWAYS_ON_TOP)
        .union(CapabilityFlags::DEBUGGER_ATTACH)
        .union(CapabilityFlags::GLOBAL_HOTKEY)
        .union(CapabilityFlags::SUSPEND_RESUME)
        .union(CapabilityFlags::FILE_LOCKS)
        .union(CapabilityFlags::PROCESS_SOCKETS)
        .union(CapabilityFlags::ELEVATED_PROCESS_NET)
        .union(CapabilityFlags::TASK_MANAGER_REPLACEMENT)
        .union(CapabilityFlags::ADMIN_STARTUP);

    fn boost_self_priority() {
        fast_sampler::boost_self_priority();
    }

    fn set_main_window_always_on_top(enabled: bool) {
        window::set_main_window_always_on_top(enabled);
    }

    fn is_main_window_minimized() -> bool {
        window::is_main_window_minimized()
    }

    fn restore_and_focus_main_window() {
        window::restore_and_focus_main_window();
    }

    fn system_accent_color() -> Option<(u8, u8, u8)> {
        window::system_accent_color()
    }

    fn initialize() -> Result<Self, PalError> {
        Ok(Self::default())
    }

    fn create_fast_sampler(&self) -> Result<Self::FastSampler, PalError> {
        Ok(WindowsFastSampler::new())
    }

    fn create_slow_prober(&self) -> Result<Self::SlowProber, PalError> {
        Ok(WindowsSlowProber::new())
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
