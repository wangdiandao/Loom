//! Process identity, hot sampling payloads, cold metadata, and CPU affinity masks.

use std::fmt;
use std::sync::Arc;

use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// Composite process lifecycle identifier resistant to OS PID reuse.
///
/// Because operating systems recycle exited PIDs aggressively, Loom pairs
/// every PID with its creation timestamp (`create_time_100ns`) when tracking
/// selections, context-menu targets, parent-child relationships, and caches.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct ProcessKey {
    /// Operating-system numeric process identifier.
    pub pid: u32,
    /// Process creation timestamp in 100-nanosecond ticks.
    pub create_time_100ns: u64,
}

impl ProcessKey {
    /// Creates a new composite [`ProcessKey`].
    #[inline]
    pub const fn new(pid: u32, create_time_100ns: u64) -> Self {
        Self {
            pid,
            create_time_100ns,
        }
    }

    /// Returns `true` if `self` can validly be the parent of `child`
    /// (distinct PID and created no later than `child`).
    #[inline]
    pub const fn is_valid_parent_of(self, child: Self) -> bool {
        self.pid != child.pid && self.create_time_100ns <= child.create_time_100ns
    }
}

impl fmt::Display for ProcessKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pid={} (created={})", self.pid, self.create_time_100ns)
    }
}

/// Top-level grouping category for the Processes view.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub enum ProcessCategory {
    /// Interactive desktop application with a visible top-level window.
    App,
    /// Non-interactive user or third-party background process.
    #[default]
    Background,
    /// Core operating-system process (e.g., Windows system services, Linux kernel/daemons, macOS system daemons).
    #[serde(alias = "Windows")]
    System,
}

bitflags! {
    /// Compact bitflags representing dynamic process status and attributes.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct ProcessStatusFlags: u16 {
        /// All threads of the process are currently suspended.
        const SUSPENDED = 1 << 0;
        /// The primary UI window is not pumping messages (`IsHungAppWindow`).
        const NOT_RESPONDING = 1 << 1;
        /// Process is running in EcoQoS / Efficiency Mode.
        const EFFICIENCY_MODE = 1 << 2;
        /// Process token is elevated (Administrator / root).
        const ELEVATED = 1 << 3;
        /// Process is OS-protected (e.g., PPL / Protected Process).
        const PROTECTED = 1 << 4;
        /// Process is marked critical to OS stability.
        const CRITICAL = 1 << 5;
        /// System Idle pseudo-process (PID 0 on Windows).
        const SYSTEM_IDLE = 1 << 6;
        /// OS kernel / non-user-modifiable core process (e.g., Windows System/Idle, Linux `kthreadd`, macOS `kernel_task`).
        const KERNEL = 1 << 7;
    }
}

/// CPU instruction set architecture of a running process.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProcessArch {
    /// Architecture could not be determined or access was denied.
    #[default]
    Unknown,
    /// 32-bit x86 (or WOW64 on x64/ARM64 Windows).
    X86,
    /// 64-bit x86-64 / AMD64.
    X64,
    /// 64-bit ARM64 / AArch64.
    Arm64,
}

/// UAC file/registry virtualization state of a process token.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UacVirtualization {
    /// Virtualization is not allowed for this process (e.g., 64-bit or elevated).
    NotAllowed,
    /// Virtualization is actively enabled.
    Enabled,
    /// Virtualization is supported but disabled.
    Disabled,
    /// Virtualization status is unavailable or unsupported.
    #[default]
    Unknown,
}

/// Scheduling priority class of a process, ordered from lowest to highest.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub enum ProcessPriority {
    /// Lowest priority class; runs only when the system is idle.
    Idle,
    /// Lower than normal priority.
    BelowNormal,
    /// Standard default process priority.
    #[default]
    Normal,
    /// Higher than normal priority.
    AboveNormal,
    /// High priority for time-sensitive workloads.
    High,
    /// Highest possible priority class; may starve OS threads if misused.
    Realtime,
}

/// Multi-group CPU affinity bitmask supporting systems with more than 64 logical CPUs.
///
/// Bits are organized in 64-CPU groups (`groups[cpu_idx / 64]`, bit `cpu_idx % 64`).
/// Up to 128 logical CPUs are stored inline without heap allocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuAffinityMask {
    /// 64-bit bitmasks, one per processor group.
    pub groups: SmallVec<[u64; 2]>,
    /// Total logical CPU count represented by this mask.
    pub logical_cpu_count: u32,
}

impl CpuAffinityMask {
    /// Creates an affinity mask with no CPUs enabled for `logical_cpu_count` CPUs.
    pub fn empty(logical_cpu_count: u32) -> Self {
        let group_count = (logical_cpu_count as usize).div_ceil(64);
        let mut groups = SmallVec::with_capacity(group_count);
        groups.resize(group_count, 0);
        Self {
            groups,
            logical_cpu_count,
        }
    }

    /// Creates an affinity mask with all `logical_cpu_count` CPUs enabled.
    pub fn all(logical_cpu_count: u32) -> Self {
        if logical_cpu_count == 0 {
            return Self {
                groups: SmallVec::new(),
                logical_cpu_count: 0,
            };
        }
        let full_groups = (logical_cpu_count / 64) as usize;
        let rem_bits = logical_cpu_count % 64;
        let group_count = full_groups + usize::from(rem_bits > 0);
        let mut groups = SmallVec::with_capacity(group_count);
        groups.resize(full_groups, u64::MAX);
        if rem_bits > 0 {
            groups.push((1u64 << rem_bits) - 1);
        }
        Self {
            groups,
            logical_cpu_count,
        }
    }

    /// Returns `true` if logical CPU `cpu_idx` is enabled in this mask.
    #[inline]
    pub fn is_enabled(&self, cpu_idx: u32) -> bool {
        if cpu_idx >= self.logical_cpu_count {
            return false;
        }
        let group_idx = (cpu_idx / 64) as usize;
        let bit_idx = cpu_idx % 64;
        self.groups
            .get(group_idx)
            .is_some_and(|&mask| (mask & (1u64 << bit_idx)) != 0)
    }

    /// Enables or disables logical CPU `cpu_idx`.
    ///
    /// Returns `false` if `cpu_idx >= self.logical_cpu_count`.
    #[inline]
    pub fn set(&mut self, cpu_idx: u32, enabled: bool) -> bool {
        if cpu_idx >= self.logical_cpu_count {
            return false;
        }
        let group_idx = (cpu_idx / 64) as usize;
        let bit_idx = cpu_idx % 64;
        if self.groups.len() <= group_idx {
            self.groups.resize(group_idx + 1, 0);
        }
        let bit = 1u64 << bit_idx;
        if enabled {
            self.groups[group_idx] |= bit;
        } else {
            self.groups[group_idx] &= !bit;
        }
        true
    }

    /// Returns the total number of enabled logical CPUs across all groups.
    #[inline]
    pub fn enabled_count(&self) -> u32 {
        self.groups.iter().map(|mask| mask.count_ones()).sum()
    }

    /// Returns `true` if no logical CPUs are enabled.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.groups.iter().all(|&mask| mask == 0)
    }

    /// Returns the number of 64-CPU processor groups in this mask.
    #[inline]
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }
}

/// High-frequency, `Copy` per-process metrics written into reusable scratch buffers.
///
/// String fields (`name`, `window_title`) are stored as `(offset, len)` slices
/// referencing the companion string arena in `ProcessSampleScratch` to eliminate
/// per-process heap allocations on the fast sampling path.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ProcessHotSample<Ext = ()> {
    /// Composite process lifecycle identifier.
    pub key: ProcessKey,
    /// Parent process identifier reported by the kernel.
    pub parent_pid: u32,
    /// OS login session identifier.
    pub session_id: u32,
    /// Active OS thread count.
    pub thread_count: u32,
    /// Open kernel handle / file descriptor count.
    pub handle_count: u32,
    /// Cumulative kernel-mode CPU time in 100-nanosecond ticks.
    pub kernel_time_100ns: u64,
    /// Cumulative user-mode CPU time in 100-nanosecond ticks.
    pub user_time_100ns: u64,
    /// Private working set memory in bytes.
    pub private_working_set_bytes: u64,
    /// Total resident working set memory in bytes.
    pub working_set_bytes: u64,
    /// Private committed virtual memory in bytes.
    pub commit_bytes: u64,
    /// Cumulative disk/storage bytes read.
    pub disk_read_bytes: u64,
    /// Cumulative disk/storage bytes written.
    pub disk_write_bytes: u64,
    /// Cumulative network bytes received.
    pub net_recv_bytes: u64,
    /// Cumulative network bytes sent.
    pub net_sent_bytes: u64,
    /// Dedicated GPU video memory currently used in bytes.
    pub gpu_dedicated_bytes: u64,
    /// Shared system GPU memory currently used in bytes.
    pub gpu_shared_bytes: u64,
    /// Instantaneous GPU utilization in permille (`0..=1000`).
    pub gpu_util_permille: u16,
    /// Current scheduling priority class.
    pub priority: ProcessPriority,
    /// Grouping category for the Processes view.
    pub category: ProcessCategory,
    /// Dynamic process status flags.
    pub status: ProcessStatusFlags,
    /// Byte offset of the image/process name in the scratch string arena.
    pub name_offset: u32,
    /// Byte length of the image/process name in the scratch string arena.
    pub name_len: u16,
    /// Byte offset of the primary window title in the scratch string arena.
    pub window_title_offset: u32,
    /// Byte length of the primary window title in the scratch string arena.
    pub window_title_len: u16,
    /// Platform-specific zero-overhead extension payload (defaults to `()`).
    pub ext: Ext,
}

/// Downsampled RGBA8 executable icon cached across processes sharing an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExeIconRgba {
    /// Icon width in pixels (typically `20`).
    pub width: u16,
    /// Icon height in pixels (typically `20`).
    pub height: u16,
    /// Tightly packed RGBA8 pixel buffer (`width * height * 4` bytes).
    pub rgba: Arc<[u8]>,
}

/// Path-deduplicated static metadata for an executable binary.
///
/// Probed asynchronously on the slow-lane bulkhead pool and shared via `Arc`
/// across all process instances launched from the same normalized path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExeStaticAsset {
    /// Normalized canonical executable path used as the cache key.
    pub normalized_path: Arc<str>,
    /// Publisher / company name from version resources or signature metadata.
    pub publisher: Option<Arc<str>>,
    /// Human-friendly file description from binary resources.
    pub file_description: Option<Arc<str>>,
    /// Downsampled RGBA icon, if available.
    pub icon: Option<ExeIconRgba>,
    /// `true` if the binary resides on a UNC or network-mapped drive and bypassed deep I/O.
    pub is_network_path: bool,
}

/// Infrequently changing per-process metadata resolved asynchronously on the slow lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessColdMeta {
    /// Process lifecycle key this metadata belongs to.
    pub key: ProcessKey,
    /// Full executable image path, if accessible.
    pub exe_path: Option<Arc<str>>,
    /// Process command line, if accessible.
    pub command_line: Option<Arc<str>>,
    /// Owning user account name (e.g., `DOMAIN\User` or `SYSTEM`).
    pub user_name: Option<Arc<str>>,
    /// Process CPU architecture.
    pub arch: ProcessArch,
    /// UAC virtualization state.
    pub uac_virtualization: UacVirtualization,
    /// Whether the process is running in Efficiency Mode (`EcoQoS`).
    pub efficiency_mode: bool,
    /// Whether the process token is elevated (Administrator / root).
    pub is_elevated: bool,
    /// Optional platform application package/group identity (e.g., Windows MSIX/UWP
    /// Package Family Name), used to group multi-process packaged apps together.
    pub app_group_id: Option<Arc<str>>,
    /// Shared path-level static executable asset.
    pub asset: Option<Arc<ExeStaticAsset>>,
}
