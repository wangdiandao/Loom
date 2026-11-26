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
