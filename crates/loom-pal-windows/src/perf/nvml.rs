//! Driverless pure user-mode NVIDIA Management Library (`nvml.dll`) telemetry reader.
//!
//! Strictly observes Loom's zero-driver and GPU D3 sleep-protection guarantees:
//! - Dynamically loads `nvml.dll` exclusively from `%SystemRoot%\System32` (modern DCH
//!   NVIDIA drivers) or the explicit protected `%ProgramFiles%\NVIDIA Corporation\NVSMI\nvml.dll`
//!   path (legacy drivers); never searches the current or executable directory.
//! - Deferred initialization: `nvmlInit_v2` is **never** called during startup or while a
//!   hybrid discrete NVIDIA GPU is sleeping (`D1`–`D3`). It is only initialized after
//!   [`crate::perf::gpu::GpuPowerGate`] allows querying an active `D0` NVIDIA adapter.
//! - Matches DXGI adapters to NVML devices deterministically by PCI Bus/Device/Function
//!   (`0000:BB:DD.F`) and gracefully returns `None` on non-NVIDIA or unsupported hardware.

use std::collections::HashMap;
use std::ffi::{CString, c_char, c_int, c_uint, c_void};
use std::mem::transmute;
use std::path::PathBuf;
use std::ptr;

use windows_sys::Win32::System::LibraryLoader::{
    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};

use crate::handle::{OwnedModule, os_str_to_wide_null};

/// PCI Vendor ID for NVIDIA Corporation (`0x10DE`).
pub(crate) const PCI_VENDOR_NVIDIA: u32 = 0x10DE;

/// `NVML_SUCCESS` (`0`).
const NVML_SUCCESS: c_int = 0;
/// `NVML_TEMPERATURE_GPU` (`0`).
const NVML_TEMPERATURE_GPU: c_uint = 0;
/// `NVML_CLOCK_GRAPHICS` (`0`).
const NVML_CLOCK_GRAPHICS: c_uint = 0;
/// `NVML_CLOCK_MEM` (`2`).
const NVML_CLOCK_MEM: c_uint = 2;

type FarProc = unsafe extern "system" fn() -> isize;
type NvmlDevice = *mut c_void;

type FnNvmlInitV2 = unsafe extern "C" fn() -> c_int;
type FnNvmlShutdown = unsafe extern "C" fn() -> c_int;
type FnNvmlDeviceGetHandleByPciBusIdV2 =
    unsafe extern "C" fn(pci_bus_id: *const c_char, device: *mut NvmlDevice) -> c_int;
type FnNvmlDeviceGetPowerUsage =
    unsafe extern "C" fn(device: NvmlDevice, power_mw: *mut c_uint) -> c_int;
type FnNvmlDeviceGetFanSpeed =
    unsafe extern "C" fn(device: NvmlDevice, speed_pct: *mut c_uint) -> c_int;
type FnNvmlDeviceGetClockInfo =
    unsafe extern "C" fn(device: NvmlDevice, clock_type: c_uint, clock_mhz: *mut c_uint) -> c_int;
type FnNvmlDeviceGetTemperature =
    unsafe extern "C" fn(device: NvmlDevice, sensor_type: c_uint, temp_c: *mut c_uint) -> c_int;

/// Read-only supplementary NVIDIA GPU telemetry sampled via `nvml.dll`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NvmlTelemetrySample {
    /// Core temperature in whole degrees Celsius (fallback when D3DKMT omits temperature).
    pub(crate) temperature_celsius: Option<i16>,
    /// Current board power draw in milliwatts.
    pub(crate) power_draw_mw: Option<u32>,
    /// Primary fan speed percentage (`0..=100`).
    pub(crate) fan_speed_percent: Option<u8>,
    /// Current graphics/core clock in MHz.
    pub(crate) core_clock_mhz: Option<u32>,
    /// Current memory clock in MHz.
    pub(crate) memory_clock_mhz: Option<u32>,
}

/// Lazy, power-gate-respecting NVML session state.
pub(crate) enum NvmlState {
    /// Not yet probed (avoids loading or initializing NVML until an active `D0` NVIDIA GPU is sampled).
    Uninitialized,
    /// Active initialized NVML library and cached PCI BDF -> `nvmlDevice_t` handles.
    Ready(NvmlSession),
    /// NVML is unavailable on this host (non-NVIDIA or driver lacks `nvml.dll`).
    Unavailable,
}

impl NvmlState {
    /// Queries NVML telemetry for the active `D0` NVIDIA GPU at `pci_bdf` (`(bus, device, function)`).
    ///
    /// Callers MUST invoke this only when [`crate::perf::gpu::GpuPowerGate`] returned `Allow`.
    pub(crate) fn query_active_device(&mut self, pci_bdf: (u32, u32, u32)) -> NvmlTelemetrySample {
        if matches!(self, Self::Uninitialized) {
            *self = match NvmlSession::init() {
                Some(session) => Self::Ready(session),
                None => Self::Unavailable,
            };
        }
        match self {
            Self::Ready(session) => session.query_device(pci_bdf),
            Self::Uninitialized | Self::Unavailable => NvmlTelemetrySample::default(),
        }
    }

    /// Clears cached NVML device handles after adapter rediscovery (e.g., eGPU hot-plug).
    pub(crate) fn clear_device_cache(&mut self) {
        if let Self::Ready(session) = self {
            session.devices.clear();
        }
    }
}

/// Active dynamically-loaded `nvml.dll` session calling `nvmlShutdown` + `FreeLibrary` on drop.
///
/// Field drop order matters: `shutdown` runs in `Drop::drop` BEFORE `_module` drops and calls
/// `FreeLibrary`, ensuring `nvmlShutdown` executes while `nvml.dll` is still mapped.
pub(crate) struct NvmlSession {
    _module: OwnedModule,
    shutdown: Option<FnNvmlShutdown>,
    get_handle_by_pci: FnNvmlDeviceGetHandleByPciBusIdV2,
    get_power_usage: Option<FnNvmlDeviceGetPowerUsage>,
    get_fan_speed: Option<FnNvmlDeviceGetFanSpeed>,
    get_clock_info: Option<FnNvmlDeviceGetClockInfo>,
    get_temperature: Option<FnNvmlDeviceGetTemperature>,
    /// Cached `nvmlDevice_t` handles keyed by `(bus, device, function)` (`None` = negative cache).
    devices: HashMap<(u32, u32, u32), Option<NvmlDevice>>,
}

// NVML handles and system HMODULE are safe to move with `GpuPerfSampler` onto the fast sampler thread.
unsafe impl Send for NvmlSession {}

impl NvmlSession {
    fn init() -> Option<Self> {
        let module = load_trusted_nvml_dll()?;

        let init_ptr = module
            .proc_raw(b"nvmlInit_v2\0")
            .or_else(|| module.proc_raw(b"nvmlInit\0"))?;
        let pci_ptr = module
            .proc_raw(b"nvmlDeviceGetHandleByPciBusId_v2\0")
            .or_else(|| module.proc_raw(b"nvmlDeviceGetHandleByPciBusId\0"))?;

        let shutdown_ptr = module.proc_raw(b"nvmlShutdown\0");
        let power_ptr = module.proc_raw(b"nvmlDeviceGetPowerUsage\0");
        let fan_ptr = module.proc_raw(b"nvmlDeviceGetFanSpeed\0");
        let clock_ptr = module.proc_raw(b"nvmlDeviceGetClockInfo\0");
        let temp_ptr = module.proc_raw(b"nvmlDeviceGetTemperature\0");

        // SAFETY: All symbols match the official NVIDIA NVML C ABI (`extern "C"`).
        let init_fn: FnNvmlInitV2 = unsafe { transmute::<FarProc, FnNvmlInitV2>(init_ptr) };
        if unsafe { init_fn() } != NVML_SUCCESS {
            return None;
        }

        unsafe {
            Some(Self {
                _module: module,
                shutdown: shutdown_ptr.map(|p| transmute::<FarProc, FnNvmlShutdown>(p)),
                get_handle_by_pci: transmute::<FarProc, FnNvmlDeviceGetHandleByPciBusIdV2>(pci_ptr),
                get_power_usage: power_ptr
                    .map(|p| transmute::<FarProc, FnNvmlDeviceGetPowerUsage>(p)),
                get_fan_speed: fan_ptr.map(|p| transmute::<FarProc, FnNvmlDeviceGetFanSpeed>(p)),
                get_clock_info: clock_ptr
                    .map(|p| transmute::<FarProc, FnNvmlDeviceGetClockInfo>(p)),
                get_temperature: temp_ptr
                    .map(|p| transmute::<FarProc, FnNvmlDeviceGetTemperature>(p)),
                devices: HashMap::with_capacity(2),
            })
        }
    }

    fn query_device(&mut self, pci_bdf: (u32, u32, u32)) -> NvmlTelemetrySample {
        let device = *self.devices.entry(pci_bdf).or_insert_with(|| {
            let pci_id = format_nvml_pci_bus_id(pci_bdf);
            let cstr = CString::new(pci_id).ok()?;
            let mut handle: NvmlDevice = ptr::null_mut();
            // SAFETY: Valid NUL-terminated ASCII PCI bus ID string passed to NVML.
            let rc = unsafe { (self.get_handle_by_pci)(cstr.as_ptr(), &mut handle) };
            (rc == NVML_SUCCESS && !handle.is_null()).then_some(handle)
        });

        let Some(dev) = device else {
            return NvmlTelemetrySample::default();
        };

        let temperature_celsius = self.get_temperature.and_then(|f| {
            let mut raw: c_uint = 0;
            // SAFETY: `dev` is a valid NVML device handle.
            let rc = unsafe { f(dev, NVML_TEMPERATURE_GPU, &mut raw) };
            (rc == NVML_SUCCESS)
                .then_some(raw)
                .and_then(sanitize_temp_c)
        });

        let power_draw_mw = self.get_power_usage.and_then(|f| {
            let mut raw: c_uint = 0;
            // SAFETY: `dev` is a valid NVML device handle.
            let rc = unsafe { f(dev, &mut raw) };
            (rc == NVML_SUCCESS)
                .then_some(raw)
                .and_then(sanitize_power_mw)
        });

        let fan_speed_percent = self.get_fan_speed.and_then(|f| {
            let mut raw: c_uint = 0;
            // SAFETY: `dev` is a valid NVML device handle.
            let rc = unsafe { f(dev, &mut raw) };
            (rc == NVML_SUCCESS)
                .then_some(raw)
                .and_then(sanitize_fan_percent)
        });

        let core_clock_mhz = self.get_clock_info.and_then(|f| {
            let mut raw: c_uint = 0;
            // SAFETY: `dev` is a valid NVML device handle.
            let rc = unsafe { f(dev, NVML_CLOCK_GRAPHICS, &mut raw) };
            (rc == NVML_SUCCESS)
                .then_some(raw)
                .and_then(sanitize_core_clock_mhz)
        });

        let memory_clock_mhz = self.get_clock_info.and_then(|f| {
            let mut raw: c_uint = 0;
            // SAFETY: `dev` is a valid NVML device handle.
            let rc = unsafe { f(dev, NVML_CLOCK_MEM, &mut raw) };
            (rc == NVML_SUCCESS)
                .then_some(raw)
                .and_then(sanitize_mem_clock_mhz)
        });

        NvmlTelemetrySample {
            temperature_celsius,
            power_draw_mw,
            fan_speed_percent,
            core_clock_mhz,
            memory_clock_mhz,
        }
    }
}

impl Drop for NvmlSession {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown {
            // SAFETY: Paired with `nvmlInit_v2` in `NvmlSession::init`; `_module` is dropped after `Drop::drop`.
            unsafe {
                let _ = shutdown();
            }
        }
    }
}

/// Loads `nvml.dll` strictly from `%SystemRoot%\System32` or the protected NVIDIA Program Files
/// installation folder—never from the executable or current working directory.
fn load_trusted_nvml_dll() -> Option<OwnedModule> {
    if let Ok(module) = OwnedModule::load("nvml.dll") {
        return Some(module);
    }

    // Legacy non-DCH driver fallback in protected `%ProgramFiles%\NVIDIA Corporation\NVSMI\nvml.dll`.
    let pf = std::env::var_os("ProgramFiles")?;
    let legacy = PathBuf::from(pf)
        .join("NVIDIA Corporation")
        .join("NVSMI")
        .join("nvml.dll");
    if !legacy.is_file() {
        return None;
    }
    let wide = os_str_to_wide_null(legacy.as_os_str());
    // SAFETY: Explicitly loads legacy `nvml.dll` from protected `Program Files` directory.
    let m2 = unsafe {
        LoadLibraryExW(
            wide.as_ptr(),
            ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    OwnedModule::from_raw(m2)
}

/// Formats a `(bus, device, function)` tuple into NVML's canonical `"0000:BB:DD.F"` PCI ID.
pub(crate) fn format_nvml_pci_bus_id((bus, device, function): (u32, u32, u32)) -> String {
    format!("0000:{bus:02X}:{device:02X}.{function}")
}

#[inline]
fn sanitize_temp_c(raw: u32) -> Option<i16> {
    (1..=150).contains(&raw).then_some(raw as i16)
}

#[inline]
fn sanitize_power_mw(raw: u32) -> Option<u32> {
    (1..=2_000_000).contains(&raw).then_some(raw)
}

#[inline]
fn sanitize_fan_percent(raw: u32) -> Option<u8> {
    (raw <= 100).then_some(raw as u8)
}

#[inline]
fn sanitize_core_clock_mhz(raw: u32) -> Option<u32> {
    (1..=10_000).contains(&raw).then_some(raw)
}

#[inline]
fn sanitize_mem_clock_mhz(raw: u32) -> Option<u32> {
    (1..=50_000).contains(&raw).then_some(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_nvml_pci_bus_id_and_sanitizes_sensor_ranges() {
        assert_eq!(format_nvml_pci_bus_id((1, 0, 0)), "0000:01:00.0");
        assert_eq!(format_nvml_pci_bus_id((0x2B, 0x0A, 1)), "0000:2B:0A.1");

        assert_eq!(sanitize_temp_c(0), None);
        assert_eq!(sanitize_temp_c(62), Some(62));
        assert_eq!(sanitize_temp_c(200), None);

        assert_eq!(sanitize_power_mw(0), None);
        assert_eq!(sanitize_power_mw(45_300), Some(45_300));
        assert_eq!(sanitize_power_mw(3_000_000), None);

        assert_eq!(sanitize_fan_percent(0), Some(0));
        assert_eq!(sanitize_fan_percent(55), Some(55));
        assert_eq!(sanitize_fan_percent(100), Some(100));
        assert_eq!(sanitize_fan_percent(101), None);

        assert_eq!(sanitize_core_clock_mhz(0), None);
        assert_eq!(sanitize_core_clock_mhz(2145), Some(2145));
        assert_eq!(sanitize_mem_clock_mhz(0), None);
        assert_eq!(sanitize_mem_clock_mhz(10501), Some(10501));
    }
}
