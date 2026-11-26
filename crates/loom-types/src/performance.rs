//! System-wide hardware performance samples and 60-point history slices.

use crate::{HistoryRing, Timestamp100Ns};

/// Single-tick system-wide CPU, memory, disk, network, and GPU summary sample.
///
/// Kept `Copy` so fast-lane samplers can overwrite reusable scratch buffers
/// without heap allocations.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SystemPerfSample {
    /// Sample timestamp in 100-nanosecond ticks.
    pub timestamp: Timestamp100Ns,
    /// Total CPU utilization in permille (`0..=1000`).
    pub cpu_total_permille: u16,
    /// Kernel-mode CPU utilization in permille (`0..=1000`).
    pub cpu_kernel_permille: u16,
    /// Current dynamic CPU frequency in MHz (`0` if unavailable).
    pub cpu_freq_mhz: u32,
    /// Nominal base CPU frequency in MHz (`0` if unavailable).
    pub cpu_base_freq_mhz: u32,
    /// Number of logical processors visible to the OS.
    pub logical_cpu_count: u32,
    /// Number of physical CPU cores (`0` if unavailable).
    pub physical_core_count: u32,
    /// Total running OS processes.
    pub process_count: u32,
    /// Total active OS threads.
    pub thread_count: u32,
    /// Total open OS handles / file descriptors.
    pub handle_count: u32,
    /// System uptime in whole seconds.
    pub uptime_secs: u64,
    /// Total physical RAM installed in bytes.
    pub mem_total_bytes: u64,
    /// Physical RAM currently in use in bytes.
    pub mem_used_bytes: u64,
    /// Physical RAM available for immediate allocation in bytes.
    pub mem_available_bytes: u64,
    /// Physical RAM on the modified page list in bytes.
    pub mem_modified_bytes: u64,
    /// Physical RAM on the standby cache list in bytes.
    pub mem_standby_bytes: u64,
    /// Physical RAM completely free/zeroed in bytes.
    pub mem_free_bytes: u64,
    /// Total virtual memory committed in bytes.
    pub mem_committed_bytes: u64,
    /// Virtual memory commit limit in bytes.
    pub mem_commit_limit_bytes: u64,
    /// Kernel paged pool memory in bytes.
    pub mem_paged_pool_bytes: u64,
    /// Kernel non-paged pool memory in bytes.
    pub mem_nonpaged_pool_bytes: u64,
    /// Aggregate disk read throughput in bytes per second.
    pub disk_read_bytes_per_sec: u64,
    /// Aggregate disk write throughput in bytes per second.
    pub disk_write_bytes_per_sec: u64,
    /// Aggregate network receive throughput in bytes per second.
    pub net_recv_bytes_per_sec: u64,
    /// Aggregate network send throughput in bytes per second.
    pub net_sent_bytes_per_sec: u64,
    /// Aggregate primary GPU utilization in permille (`0..=1000`).
    pub gpu_util_permille: u16,
    /// Dedicated GPU video memory currently in use in bytes.
    pub gpu_dedicated_used_bytes: u64,
    /// Total dedicated GPU video memory capacity in bytes.
    pub gpu_dedicated_total_bytes: u64,
    /// Shared system GPU memory currently in use in bytes.
    pub gpu_shared_used_bytes: u64,
}

impl SystemPerfSample {
    /// Returns memory utilization in permille (`0..=1000`) derived from `mem_used_bytes / mem_total_bytes`.
    #[inline]
    pub fn mem_util_permille(&self) -> u16 {
        if self.mem_total_bytes == 0 {
            return 0;
        }
        let permille = (u128::from(self.mem_used_bytes) * 1000) / u128::from(self.mem_total_bytes);
        permille.min(1000) as u16
    }
}

/// Published performance domain snapshot containing latest counters and 60-point histories.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PerformanceSlice {
    /// Monotonic revision counter for the performance domain.
    pub revision: u64,
    /// Most recent system-wide performance sample.
    pub latest: SystemPerfSample,
    /// 60-point total CPU utilization history (`0..=1000` permille).
    pub cpu_history: HistoryRing<60>,
    /// 60-point kernel CPU utilization history (`0..=1000` permille).
    pub cpu_kernel_history: HistoryRing<60>,
    /// 60-point physical memory utilization history (`0..=1000` permille).
    pub mem_history: HistoryRing<60>,
    /// Latest per-logical-CPU utilization in permille (populated when `PerCoreCpu` lease is held).
    pub per_core_permille: Vec<u16>,
    /// 60-point per-logical-CPU utilization histories (populated when `PerCoreCpu` lease is held).
    pub per_core_history: Vec<HistoryRing<60>>,
}
