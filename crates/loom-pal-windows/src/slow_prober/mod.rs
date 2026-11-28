//! Windows slow-lane bulkhead metadata prober with `300 ms` [`CircuitBreaker`]
//! and mandatory UNC / network-drive bypass.
//!
//! Strictly observes Loom's Windows anti-hang red lines:
//! - Command lines are read via `ntdll!NtQueryInformationProcess(ProcessCommandLineInformation)`
//!   — NEVER cross-process `ReadProcessMemory` on target PEB.
//! - Target handles are opened with minimal `PROCESS_QUERY_LIMITED_INFORMATION`, validated
//!   against [`ProcessKey::create_time_100ns`], and immediately closed via an RAII handle guard.
//! - UNC (`\\server\share`, `\\?\UNC\...`) and `DRIVE_REMOTE` paths skip disk PE reads and
//!   fall back to memory-only `SHGetFileInfoW(..., SHGFI_USEFILEATTRIBUTES)`.
//! - Local PE version info reads only local resources (`CompanyName` / `FileDescription`)
//!   with zero online CRL/OCSP network traffic, and GDI icon bitmaps are released via RAII
//!   guards to prevent handle leaks.

mod icon;
mod pe_version;
mod process_meta;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::ptr;
use std::sync::Arc;
use std::time::Instant;

use loom_pal_api::{PalError, SlowMetadataProber};
use loom_types::{
    DiskDeviceSample, ExeStaticAsset, HardwareStaticInfo, ProcessColdMeta, ProcessKey,
    ProcessSocketRecord,
};
use parking_lot::RwLock;
use windows_sys::Win32::Foundation::{S_FALSE, S_OK};
use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
use windows_sys::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
};
use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;
use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOTE;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON};

use crate::circuit_breaker::CircuitBreaker;
use crate::handle::open_verified_process;
use crate::perf::cpu::probe_hardware_static_info as probe_win_hardware_static_info;
use crate::perf::disk::DiskPerformanceProber;

pub(crate) use self::icon::{extract_icon_rgba, resample_rgba_premultiplied};
pub(crate) use self::pe_version::{read_pe_version_info, read_pe_version_info_full};
use self::process_meta::{
    query_efficiency_mode, query_process_arch, query_process_command_line,
    query_process_image_path, query_process_package_family_name, query_token_metadata,
};

/// Minimum and maximum clamped physical pixel dimensions for extracted small app icons.
const MIN_ICON_DIM: u16 = 16;
const MAX_ICON_DIM: u16 = 64;

/// Returns the system DPI-matched physical pixel dimension for a `16px` logical UI icon
/// (e.g. `16` at 100% DPI, `20` at 125%, `24` at 150%, `32` at 200%).
#[inline]
pub(crate) fn target_icon_dim() -> u16 {
    // SAFETY: `GetSystemMetrics` is a pure read-only Win32 query.
    let raw = unsafe { GetSystemMetrics(SM_CXSMICON) };
    if raw <= 0 {
        MIN_ICON_DIM
    } else {
        (raw as u16).clamp(MIN_ICON_DIM, MAX_ICON_DIM)
    }
}

/// Thread-local RAII COM STA initializer so Shell APIs work reliably on slow-lane worker threads.
struct ComThreadGuard {
    should_uninit: bool,
}

impl Drop for ComThreadGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            // SAFETY: Paired with a successful `CoInitializeEx` on this exact thread.
            unsafe {
                CoUninitialize();
            }
        }
    }
}

thread_local! {
    static COM_STA_GUARD: ComThreadGuard = {
        // SAFETY: Initializes COM apartment-threaded mode on the current worker thread if needed.
        let hr = unsafe {
            CoInitializeEx(
                ptr::null(),
                (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
            )
        };
        ComThreadGuard {
            should_uninit: hr == S_OK || hr == S_FALSE,
        }
    };
}

/// Ensures COM STA is initialized on the calling thread for Shell icon extraction.
#[inline]
pub(crate) fn ensure_com_initialized() {
    COM_STA_GUARD.with(|_| {});
}

/// Windows slow-lane bulkhead metadata prober.
///
/// Wraps a `300 ms` [`CircuitBreaker`] (`60 s` cooldown), a per-drive `GetDriveTypeW`
/// cache for network-drive bypass, and a binary-SID -> account-name cache.
#[derive(Debug, Default)]
pub struct WindowsSlowProber {
    circuit_breaker: CircuitBreaker,
    drive_type_cache: RwLock<HashMap<u8, u32>>,
    pub(super) sid_cache: RwLock<HashMap<Vec<u8>, Arc<str>>>,
    disk_prober: DiskPerformanceProber,
}

impl WindowsSlowProber {
    /// Creates a new Windows slow-lane prober with default `300 ms` / `60 s` circuit breaker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a prober with an explicitly configured [`CircuitBreaker`].
    pub fn with_circuit_breaker(circuit_breaker: CircuitBreaker) -> Self {
        Self {
            circuit_breaker,
            drive_type_cache: RwLock::new(HashMap::new()),
            sid_cache: RwLock::new(HashMap::new()),
            disk_prober: DiskPerformanceProber::new(),
        }
    }

    /// Returns a reference to the internal [`CircuitBreaker`].
    #[inline]
    pub fn circuit_breaker(&self) -> &CircuitBreaker {
        &self.circuit_breaker
    }

    /// Returns `true` if `path` resides on a UNC share (`\\server\share`, `\\?\UNC\...`)
    /// or a network-mapped drive (`GetDriveTypeW == DRIVE_REMOTE`).
    ///
    /// Drive-type lookups are cached per uppercase ASCII drive letter to avoid repeated
    /// Win32 calls.
    pub fn is_network_path(&self, path: &str) -> bool {
        if is_unc_path(path) {
            return true;
        }
        let Some(drive) = extract_drive_letter(path) else {
            return false;
        };

        if let Some(&cached) = self.drive_type_cache.read().get(&drive) {
            return cached == DRIVE_REMOTE;
        }

        let root_wide: [u16; 4] = [u16::from(drive), u16::from(b':'), u16::from(b'\\'), 0];
        // SAFETY: `root_wide` is a valid null-terminated UTF-16 drive root string (`X:\`).
        let drive_type = unsafe { GetDriveTypeW(root_wide.as_ptr()) };
        self.drive_type_cache.write().insert(drive, drive_type);
        drive_type == DRIVE_REMOTE
    }
}

impl SlowMetadataProber for WindowsSlowProber {
    fn probe_process(&self, key: ProcessKey) -> Result<ProcessColdMeta, PalError> {
        let handle = open_verified_process(key, PROCESS_QUERY_LIMITED_INFORMATION)?;

        let exe_path = query_process_image_path(handle.as_raw()).map(Arc::<str>::from);
        let command_line = query_process_command_line(handle.as_raw()).map(Arc::<str>::from);
        let arch = query_process_arch(handle.as_raw());
        let efficiency_mode = query_efficiency_mode(handle.as_raw());
        let app_group_id = query_process_package_family_name(handle.as_raw()).map(Arc::<str>::from);
        let (user_name, uac_virtualization, is_elevated) =
            query_token_metadata(handle.as_raw(), &self.sid_cache);

        Ok(ProcessColdMeta {
            key,
            exe_path,
            command_line,
            user_name,
            arch,
            uac_virtualization,
            efficiency_mode,
            is_elevated,
            app_group_id,
            asset: None,
        })
    }

    fn probe_exe_asset(&self, exe_path: &str) -> Result<ExeStaticAsset, PalError> {
        let trimmed = exe_path.trim();
        let normalized_path: Arc<str> = Arc::from(trimmed);
        let target = circuit_target_key(trimmed);

        self.circuit_breaker.check(&target)?;

        if self.is_network_path(trimmed) {
            ensure_com_initialized();
            let icon = extract_icon_rgba(trimmed, true);
            return Ok(ExeStaticAsset {
                normalized_path,
                publisher: None,
                file_description: None,
                icon,
                is_network_path: true,
            });
        }

        let start = Instant::now();
        ensure_com_initialized();

        let (publisher, file_description) = read_pe_version_info(trimmed);
        let icon = extract_icon_rgba(trimmed, false);

        let elapsed = start.elapsed();
        if self
            .circuit_breaker
            .record_failure_or_timeout(&target, elapsed)
        {
            tracing::warn!(
                target = %target,
                path = %trimmed,
                elapsed_ms = elapsed.as_millis() as u64,
                "slow-lane exe probe exceeded 300ms budget; circuit breaker tripped"
            );
        } else {
            self.circuit_breaker.record_success(&target);
        }

        Ok(ExeStaticAsset {
            normalized_path,
            publisher,
            file_description,
            icon,
            is_network_path: false,
        })
    }

    fn probe_hardware_static_info(&self) -> Result<HardwareStaticInfo, PalError> {
        Ok(probe_win_hardware_static_info())
    }

    fn probe_disk_performance(&self) -> Result<Vec<DiskDeviceSample>, PalError> {
        Ok(self.disk_prober.probe(&self.circuit_breaker))
    }

    fn list_listening_ports(&self) -> Result<Vec<ProcessSocketRecord>, PalError> {
        crate::ports::list_listening_ports()
    }
}

/// Returns `true` if `path` uses UNC syntax (`\\server\share`, `//server/share`,
/// `\\?\UNC\server\share`, or `\\.\UNC\server\share`).
pub fn is_unc_path(path: &str) -> bool {
    let trimmed = path.trim();
    if trimmed.len() < 2 {
        return false;
    }

    if let Some(rest) = trimmed
        .strip_prefix(r"\\?\")
        .or_else(|| trimmed.strip_prefix(r"\\.\"))
    {
        return rest.len() >= 4 && rest[..4].eq_ignore_ascii_case(r"UNC\") && rest.len() > 4;
    }

    trimmed.starts_with(r"\\") || trimmed.starts_with("//")
}

/// Extracts an uppercase ASCII drive letter (`b'A'..=b'Z'`) from local or extended paths
/// (e.g., `C:\Windows\notepad.exe` or `\\?\C:\Windows\notepad.exe`).
pub(crate) fn extract_drive_letter(path: &str) -> Option<u8> {
    let trimmed = path.trim();
    let stripped = trimmed
        .strip_prefix(r"\\?\")
        .or_else(|| trimmed.strip_prefix(r"\\.\"))
        .unwrap_or(trimmed);

    let bytes = stripped.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        Some(bytes[0].to_ascii_uppercase())
    } else {
        None
    }
}

/// Derives a device/share-scoped circuit breaker target identifier for `path`.
pub(super) fn circuit_target_key(path: &str) -> String {
    if is_unc_path(path) {
        let stripped = path
            .trim()
            .strip_prefix(r"\\?\UNC\")
            .or_else(|| path.trim().strip_prefix(r"\\?\unc\"))
            .or_else(|| path.trim().strip_prefix(r"\\.\UNC\"))
            .or_else(|| path.trim().strip_prefix(r"\\"))
            .or_else(|| path.trim().strip_prefix("//"))
            .unwrap_or(path);
        let mut parts = stripped.split(['\\', '/']).filter(|p| !p.is_empty());
        match (parts.next(), parts.next()) {
            (Some(server), Some(share)) => format!(r"\\{server}\{share}"),
            (Some(server), None) => format!(r"\\{server}"),
            _ => "unc".to_owned(),
        }
    } else if let Some(drive) = extract_drive_letter(path) {
        format!("{}:", drive as char)
    } else {
        "local".to_owned()
    }
}
