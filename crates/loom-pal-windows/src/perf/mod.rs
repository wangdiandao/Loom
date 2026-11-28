//! WMI/PDH-free Windows hardware performance monitoring submodules.
//!
//! - [`cpu`]: `CallNtPowerInformation` dynamic/base CPU frequency, `GetLogicalProcessorInformationEx`
//!   topology/cache hierarchy, and `NtQuerySystemInformation(SystemMemoryListInformation)` composition.
//! - [`smbios`]: pure-Rust SMBIOS Type 17 parser (`GetSystemFirmwareTable('RSMB', 0)`).
//! - `disk`: slow-lane physical disk IOCTL prober guarded by per-device `300 ms` circuit breaker.
//! - `net`: `iphlpapi` (`GetIfTable2` + `GetAdaptersAddresses`) adapter throughput & IP telemetry.
//! - `gpu`: dynamic `dxgi.dll` + `gdi32.dll` (`D3DKMTQueryStatistics` / `D3DKMTQueryAdapterInfo`)
//!   GPU VRAM, engine utilization, temperature, and driver metadata.

pub mod cpu;
pub(crate) mod disk;
pub(crate) mod gpu;
pub(crate) mod net;
pub(crate) mod nvml;
pub mod smbios;
