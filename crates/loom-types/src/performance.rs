//! System-wide hardware performance samples and 60-point history slices.

use std::sync::Arc;

use crate::{HistoryRing, ThroughputRing, Timestamp100Ns};

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

/// Static CPU hardware identity, topology, virtualization, and cache metadata.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CpuStaticInfo {
    /// Human-readable processor brand string (e.g., `"AMD Ryzen 9 7950X"`).
    pub brand_name: Arc<str>,
    /// Nominal base frequency in MHz (`0` if unavailable).
    pub base_freq_mhz: u32,
    /// Number of physical CPU sockets / packages.
    pub sockets: u32,
    /// Number of physical processor cores.
    pub physical_cores: u32,
    /// Number of logical processors visible to the OS.
    pub logical_processors: u32,
    /// Whether hardware virtualization (VT-x / AMD-V) is enabled in firmware.
    pub virtualization_enabled: Option<bool>,
    /// Total L1 cache size in bytes across all cores.
    pub l1_cache_bytes: u64,
    /// Total L2 cache size in bytes across all cores.
    pub l2_cache_bytes: u64,
    /// Total L3 cache size in bytes across all packages.
    pub l3_cache_bytes: u64,
}

/// Static physical RAM hardware metadata decoded from firmware SMBIOS tables.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MemoryHardwareInfo {
    /// Configured or rated memory transfer speed in MT/s (e.g., `4800`, `6000`).
    pub speed_mt_s: Option<u32>,
    /// Number of populated memory slots.
    pub slots_used: Option<u32>,
    /// Total number of physical memory slots reported by SMBIOS.
    pub slots_total: Option<u32>,
    /// Memory module form factor (e.g., `"DIMM"`, `"SODIMM"`).
    pub form_factor: Option<Arc<str>>,
    /// Memory reserved by firmware/hardware (installed minus OS-usable physical RAM) in bytes.
    pub hardware_reserved_bytes: u64,
}

/// Combined static CPU and memory hardware metadata probed once on demand.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HardwareStaticInfo {
    /// Static processor brand, topology, and cache metadata.
    pub cpu: CpuStaticInfo,
    /// Static SMBIOS physical memory hardware metadata.
    pub memory: MemoryHardwareInfo,
}

/// Physical storage media classification for a disk device.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiskMediaType {
    /// NVMe solid-state storage over PCIe.
    Nvme,
    /// Solid-state drive without rotational seek penalty (SATA/SAS/USB SSD).
    Ssd,
    /// Rotational hard disk drive incurring seek penalty.
    Hdd,
    /// Media type could not be determined.
    #[default]
    Unknown,
}

/// Single-tick telemetry sample for a physical disk device (`Disk 0..N`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DiskDeviceSample {
    /// Physical disk index (e.g., `0` for `\\.\PhysicalDrive0`).
    pub index: u32,
    /// Disk vendor and model identifier.
    pub model: Arc<str>,
    /// Storage media classification (`Nvme`, `Ssd`, `Hdd`, or `Unknown`).
    pub media_type: DiskMediaType,
    /// Space-separated drive letters mapped to this disk (e.g., `"C: D:"`).
    pub drive_letters: Arc<str>,
    /// Total physical disk capacity in bytes.
    pub capacity_bytes: u64,
    /// Active busy time in permille (`0..=1000`).
    pub active_time_permille: u16,
    /// Current read throughput in bytes per second.
    pub read_bytes_per_sec: u64,
    /// Current write throughput in bytes per second.
    pub write_bytes_per_sec: u64,
    /// Average I/O request response latency in microseconds.
    pub avg_response_us: u64,
    /// `true` when this disk's slow-lane circuit breaker is currently open (probing bypassed).
    pub circuit_open: bool,
}

/// Published per-disk telemetry slice including 60-point active-time and throughput histories.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DiskDeviceSlice {
    /// Physical disk index (e.g., `0` for `\\.\PhysicalDrive0`).
    pub index: u32,
    /// Disk vendor and model identifier.
    pub model: Arc<str>,
    /// Storage media classification (`Nvme`, `Ssd`, `Hdd`, or `Unknown`).
    pub media_type: DiskMediaType,
    /// Space-separated drive letters mapped to this disk (e.g., `"C: D:"`).
    pub drive_letters: Arc<str>,
    /// Total physical disk capacity in bytes.
    pub capacity_bytes: u64,
    /// Active busy time in permille (`0..=1000`).
    pub active_time_permille: u16,
    /// Current read throughput in bytes per second.
    pub read_bytes_per_sec: u64,
    /// Current write throughput in bytes per second.
    pub write_bytes_per_sec: u64,
    /// Average I/O request response latency in microseconds.
    pub avg_response_us: u64,
    /// `true` when this disk's slow-lane circuit breaker is currently open (probing bypassed).
    pub circuit_open: bool,
    /// 60-point active time history (`0..=1000` permille).
    pub active_history: HistoryRing<60>,
    /// 60-point read throughput history in bytes per second.
    pub read_history: ThroughputRing<60>,
    /// 60-point write throughput history in bytes per second.
    pub write_history: ThroughputRing<60>,
}

/// Network adapter link medium classification.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NetConnectionType {
    /// Wired Ethernet adapter.
    Ethernet,
    /// Wireless IEEE 802.11 Wi-Fi adapter.
    WiFi,
    /// Other operational network interface type.
    #[default]
    Other,
}

/// Single-tick telemetry sample for an operational network adapter.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NetAdapterSample {
    /// OS locally unique interface identifier (`NET_LUID::Value` on Windows).
    pub luid: u64,
    /// User-friendly adapter alias (e.g., `"Ethernet"`, `"Wi-Fi"`).
    pub name: Arc<str>,
    /// Hardware interface description string.
    pub description: Arc<str>,
    /// Link medium type (`Ethernet`, `WiFi`, or `Other`).
    pub connection_type: NetConnectionType,
    /// Negotiated link speed in bits per second (`0` if unavailable).
    pub link_speed_bps: u64,
    /// Primary IPv4 unicast address string if assigned.
    pub ipv4: Option<Arc<str>>,
    /// Primary IPv6 unicast address string if assigned.
    pub ipv6: Option<Arc<str>>,
    /// Connection-specific DNS suffix if configured.
    pub dns_suffix: Option<Arc<str>>,
    /// Current receive throughput in bytes per second.
    pub recv_bytes_per_sec: u64,
    /// Current transmit throughput in bytes per second.
    pub sent_bytes_per_sec: u64,
}

/// Published per-adapter network telemetry slice including 60-point receive/send histories.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NetAdapterSlice {
    /// OS locally unique interface identifier (`NET_LUID::Value` on Windows).
    pub luid: u64,
    /// User-friendly adapter alias (e.g., `"Ethernet"`, `"Wi-Fi"`).
    pub name: Arc<str>,
    /// Hardware interface description string.
    pub description: Arc<str>,
    /// Link medium type (`Ethernet`, `WiFi`, or `Other`).
    pub connection_type: NetConnectionType,
    /// Negotiated link speed in bits per second (`0` if unavailable).
    pub link_speed_bps: u64,
    /// Primary IPv4 unicast address string if assigned.
    pub ipv4: Option<Arc<str>>,
    /// Primary IPv6 unicast address string if assigned.
    pub ipv6: Option<Arc<str>>,
    /// Connection-specific DNS suffix if configured.
    pub dns_suffix: Option<Arc<str>>,
    /// Current receive throughput in bytes per second.
    pub recv_bytes_per_sec: u64,
    /// Current transmit throughput in bytes per second.
    pub sent_bytes_per_sec: u64,
    /// 60-point receive throughput history in bytes per second.
    pub recv_history: ThroughputRing<60>,
    /// 60-point transmit throughput history in bytes per second.
    pub sent_history: ThroughputRing<60>,
}

/// Single-tick utilization sample for a single GPU execution engine (e.g., `3D`, `Copy`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GpuEngineSample {
    /// Engine display name (e.g., `"3D"`, `"Copy"`, `"Video Decode"`, `"Video Encode"`).
    pub name: Arc<str>,
    /// Current engine utilization in permille (`0..=1000`).
    pub util_permille: u16,
}

/// Published GPU execution engine slice including a 60-point utilization history.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GpuEngineSlice {
    /// Engine display name (e.g., `"3D"`, `"Copy"`, `"Video Decode"`, `"Video Encode"`).
    pub name: Arc<str>,
    /// Current engine utilization in permille (`0..=1000`).
    pub util_permille: u16,
    /// 60-point utilization history (`0..=1000` permille).
    pub history: HistoryRing<60>,
}

/// Single-tick telemetry sample for a GPU adapter (`GPU 0..N`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GpuAdapterSample {
    /// Zero-based adapter display index (`GPU 0`, `GPU 1`, ...).
    pub index: u32,
    /// OS locally unique adapter identifier (`LUID` packed into `u64`).
    pub luid: u64,
    /// Adapter hardware description (e.g., `"NVIDIA GeForce RTX 4080"`).
    pub name: Arc<str>,
    /// Installed graphics driver version string if available.
    pub driver_version: Option<Arc<str>>,
    /// Supported DirectX / WDDM version string if available.
    pub directx_version: Option<Arc<str>>,
    /// PCI bus/device/function location string if available.
    pub pci_location: Option<Arc<str>>,
    /// Core temperature in whole degrees Celsius if exposed by the driver.
    pub temperature_celsius: Option<i16>,
    /// Overall adapter utilization in permille (`0..=1000`), typically the max engine load.
    pub util_permille: u16,
    /// Dedicated video memory currently in use in bytes.
    pub dedicated_used_bytes: u64,
    /// Total dedicated video memory capacity in bytes.
    pub dedicated_total_bytes: u64,
    /// Shared system video memory currently in use in bytes.
    pub shared_used_bytes: u64,
    /// Total shared system video memory capacity in bytes.
    pub shared_total_bytes: u64,
    /// Per-engine utilization samples reported for this GPU adapter.
    pub engines: Vec<GpuEngineSample>,
}

/// Published per-GPU telemetry slice including 60-point utilization, memory, and engine histories.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GpuAdapterSlice {
    /// Zero-based adapter display index (`GPU 0`, `GPU 1`, ...).
    pub index: u32,
    /// OS locally unique adapter identifier (`LUID` packed into `u64`).
    pub luid: u64,
    /// Adapter hardware description (e.g., `"NVIDIA GeForce RTX 4080"`).
    pub name: Arc<str>,
    /// Installed graphics driver version string if available.
    pub driver_version: Option<Arc<str>>,
    /// Supported DirectX / WDDM version string if available.
    pub directx_version: Option<Arc<str>>,
    /// PCI bus/device/function location string if available.
    pub pci_location: Option<Arc<str>>,
    /// Core temperature in whole degrees Celsius if exposed by the driver.
    pub temperature_celsius: Option<i16>,
    /// Overall adapter utilization in permille (`0..=1000`), typically the max engine load.
    pub util_permille: u16,
    /// Dedicated video memory currently in use in bytes.
    pub dedicated_used_bytes: u64,
    /// Total dedicated video memory capacity in bytes.
    pub dedicated_total_bytes: u64,
    /// Shared system video memory currently in use in bytes.
    pub shared_used_bytes: u64,
    /// Total shared system video memory capacity in bytes.
    pub shared_total_bytes: u64,
    /// 60-point overall GPU utilization history (`0..=1000` permille).
    pub util_history: HistoryRing<60>,
    /// 60-point dedicated video memory utilization history (`0..=1000` permille).
    pub dedicated_mem_history: HistoryRing<60>,
    /// 60-point shared video memory utilization history (`0..=1000` permille).
    pub shared_mem_history: HistoryRing<60>,
    /// Per-engine telemetry slices and 60-point histories.
    pub engines: Vec<GpuEngineSlice>,
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
    /// Static CPU brand, topology, virtualization, and cache metadata.
    pub cpu_info: CpuStaticInfo,
    /// Static SMBIOS physical RAM metadata.
    pub memory_info: MemoryHardwareInfo,
    /// Per-physical-disk telemetry slices and 60-point histories (`Disk 0..N`).
    pub disks: Vec<DiskDeviceSlice>,
    /// Per-network-adapter telemetry slices and 60-point histories (Ethernet / Wi-Fi).
    pub networks: Vec<NetAdapterSlice>,
    /// Per-GPU-adapter telemetry slices and 60-point histories (`GPU 0..N`).
    pub gpus: Vec<GpuAdapterSlice>,
}
