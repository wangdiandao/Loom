//! Capability bitflags and system control / diagnostic payloads.

use std::sync::Arc;

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

use crate::ProcessKey;

bitflags! {
    /// Compile-time capability bitmask advertised by a platform backend.
    ///
    /// Upper layers inspect these flags to hide or disable platform-specific
    /// controls without introducing OS `#[cfg]` branches outside `loom-pal`.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct CapabilityFlags: u32 {
        /// Supports toggling process Efficiency Mode / EcoQoS.
        const ECO_MODE = 1 << 0;
        /// Supports querying and modifying process CPU affinity masks.
        const CPU_AFFINITY = 1 << 1;
        /// Supports changing process scheduling priority class.
        const PROCESS_PRIORITY = 1 << 2;
        /// Supports querying and toggling UAC virtualization.
        const UAC_VIRTUALIZATION = 1 << 3;
        /// Supports thread wait-chain / deadlock inspection.
        const WAIT_CHAIN = 1 << 4;
        /// Supports writing process memory minidump files.
        const MINIDUMP = 1 << 5;
        /// Supports enumerating and controlling system services.
        const SERVICES = 1 << 6;
        /// Supports enumerating and toggling startup applications.
        const STARTUP_APPS = 1 << 7;
        /// Supports enumerating and managing user sessions.
        const USER_SESSIONS = 1 << 8;
        /// Supports reading application resource usage history.
        const APP_HISTORY = 1 << 9;
        /// Supports relaunching Loom elevated via OS privilege prompt.
        const ELEVATION_RELAUNCH = 1 << 10;
        /// Supports toggling host window always-on-top z-order.
        const ALWAYS_ON_TOP = 1 << 11;
        /// Supports attaching the OS-configured JIT debugger to a running process.
        const DEBUGGER_ATTACH = 1 << 12;
        /// Supports registering a system-wide global wakeup hotkey.
        const GLOBAL_HOTKEY = 1 << 13;
        /// Supports suspending and resuming non-critical user-space processes.
        const SUSPEND_RESUME = 1 << 14;
        /// Supports querying processes holding open locks on files or directories.
        const FILE_LOCKS = 1 << 15;
        /// Supports enumerating listening ports and per-process open network sockets.
        const PROCESS_SOCKETS = 1 << 16;
        /// Supports elevated per-process network byte attribution (e.g., real-time ETW).
        const ELEVATED_PROCESS_NET = 1 << 17;
        /// Supports replacing the OS built-in Task Manager (e.g., Windows IFEO `taskmgr.exe`).
        const TASK_MANAGER_REPLACEMENT = 1 << 18;
        /// Supports persistent UAC-free administrator startup (e.g., Windows Task Scheduler).
        const ADMIN_STARTUP = 1 << 19;
    }
}

/// Request payload for launching a new process via the "Run new task" dialog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunTaskRequest {
    /// Command line or executable path to execute.
    pub command: String,
    /// Whether to request administrator / elevated privileges (`runas`).
    pub run_as_admin: bool,
}

/// Outcome for a single process during a bottom-up process tree termination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeKillItem {
    /// Process lifecycle key targeted for termination.
    pub key: ProcessKey,
    /// Image name captured before termination.
    pub name: Arc<str>,
    /// `true` if the process was terminated (or had already exited).
    pub terminated: bool,
    /// Human-readable error detail when `terminated` is `false`.
    pub error: Option<String>,
}

/// Aggregated report returned after terminating a process tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeKillReport {
    /// Root process key requested by the user.
    pub root: ProcessKey,
    /// Individual process outcomes in execution order (leaves to root).
    pub items: Vec<TreeKillItem>,
}

impl TreeKillReport {
    /// Returns the number of processes successfully terminated.
    pub fn succeeded_count(&self) -> usize {
        self.items.iter().filter(|item| item.terminated).count()
    }

    /// Returns the number of processes that failed to terminate.
    pub fn failed_count(&self) -> usize {
        self.items.iter().filter(|item| !item.terminated).count()
    }

    /// Returns `true` if every targeted process in the tree was terminated.
    pub fn all_succeeded(&self) -> bool {
        self.items.iter().all(|item| item.terminated)
    }
}

/// A node in a process/thread wait-chain diagnostic tree (e.g., Win32 WCT).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitChainNode {
    /// Operating-system thread identifier waiting or holding the resource.
    pub thread_id: u32,
    /// Owning process key if the thread belongs to an identifiable process.
    pub process_key: Option<ProcessKey>,
    /// Owning process image name, if resolved.
    pub process_name: Option<Arc<str>>,
    /// Synchronization object type (e.g., `Mutex`, `CriticalSection`, `ALPC`, `Process`).
    pub object_type: Arc<str>,
    /// Optional synchronization object name.
    pub object_name: Option<Arc<str>>,
    /// `true` if this node participates in a detected deadlock cycle.
    pub is_deadlocked: bool,
    /// Downstream wait-chain nodes blocking this node.
    pub children: Vec<WaitChainNode>,
}

/// Transport protocol and IP family for a process network socket endpoint.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub enum SocketProtocol {
    /// IPv4 TCP socket.
    #[default]
    Tcp,
    /// IPv6 TCP socket.
    Tcp6,
    /// IPv4 UDP socket.
    Udp,
    /// IPv6 UDP socket.
    Udp6,
}

/// A single TCP or UDP network socket endpoint owned by a running process.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProcessSocketRecord {
    /// Transport protocol and IP version.
    pub protocol: SocketProtocol,
    /// Formatted local IP address string (e.g., `"0.0.0.0"`, `"::1"`).
    pub local_addr: Arc<str>,
    /// Local port number in host byte order.
    pub local_port: u16,
    /// Formatted remote IP address string, if connected.
    pub remote_addr: Option<Arc<str>>,
    /// Remote port number in host byte order, if connected.
    pub remote_port: Option<u16>,
    /// Connection state label (e.g., `"LISTEN"`, `"ESTABLISHED"`, `"UDP"`).
    pub state: Arc<str>,
    /// Numeric process identifier of the socket owner.
    pub pid: u32,
}

/// Process or service currently holding an open lock on a queried file or directory.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FileLockHolder {
    /// Numeric process identifier of the lock holder.
    pub pid: u32,
    /// Friendly application display name reported by the OS.
    pub app_name: Arc<str>,
    /// Optional service short name when the lock holder is a system service.
    pub service_name: Option<Arc<str>>,
    /// Classification label of the application (e.g., `"MainWindow"`, `"Service"`, `"Critical"`).
    pub app_type: Arc<str>,
    /// Whether the OS indicates the lock holder can be safely restarted automatically.
    pub restartable: bool,
}

/// Lifecycle state of OS-level system Task Manager replacement (e.g., Windows IFEO).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskManagerReplacementState {
    /// Replacement is not enabled and no conflicting entry is present.
    #[default]
    Disabled,
    /// Replacement is active and verified against Loom's registered launcher command.
    Enabled,
    /// Another program's `Debugger`, `UseFilter`, or IFEO subkey is present; Loom refuses to overwrite.
    Conflict,
    /// Loom's recovery journal is active, but the OS entry was externally removed/restored.
    Revoked,
    /// An interrupted enable or restore left an unfinished recovery journal.
    RecoveryNeeded,
    /// Platform does not support system Task Manager replacement.
    Unsupported,
}

/// Read-only status snapshot for system Task Manager replacement.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskManagerReplacementStatus {
    /// Current verified replacement state.
    pub state: TaskManagerReplacementState,
    /// Whether enabling replacement is currently permitted (no conflict and supported).
    pub can_enable: bool,
    /// Whether restoring/cleaning up replacement is currently permitted.
    pub can_restore: bool,
    /// Optional human-readable conflict summary (e.g., third-party Debugger or IFEO subkeys).
    pub conflict_detail: Option<String>,
    /// Optional path to the recovery journal / instructions for troubleshooting.
    pub recovery_path: Option<String>,
}

/// Read-only status snapshot for persistent administrator startup (e.g., Windows Task Scheduler).
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminStartupStatus {
    /// Whether the platform supports persistent administrator startup.
    pub supported: bool,
    /// Whether a persistent administrator startup task/grant is currently registered.
    pub enabled: bool,
    /// Whether the registered task is verified and ready to elevate the current executable.
    pub ready: bool,
    /// Whether the current process is already running elevated.
    pub elevated: bool,
    /// Whether the current user account is eligible for elevation (administrator or split-token admin).
    pub eligible: bool,
    /// Whether the current elevated session can cleanly drop back to standard privileges (split token).
    pub can_drop_elevation: bool,
    /// Optional diagnostic detail when an existing grant is missing, stale, or invalid.
    pub error: Option<String>,
}
