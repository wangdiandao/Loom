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

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr;
use std::sync::Arc;
use std::time::Instant;

use loom_pal_api::{PalError, SlowMetadataProber};
use loom_types::{
    DiskDeviceSample, ExeIconRgba, ExeStaticAsset, HardwareStaticInfo, ProcessArch,
    ProcessColdMeta, ProcessKey, UacVirtualization,
};
use parking_lot::RwLock;
use windows_sys::Wdk::System::Threading::{
    NtQueryInformationProcess, ProcessCommandLineInformation,
};
use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, GetLastError, HANDLE, S_FALSE, S_OK,
    STATUS_BUFFER_TOO_SMALL, STATUS_INFO_LENGTH_MISMATCH, STATUS_SUCCESS, UNICODE_STRING,
};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDIBits, GetObjectW, HDC, HGDIOBJ,
};
use windows_sys::Win32::Security::{
    GetLengthSid, GetTokenInformation, LookupAccountSidW, SID_NAME_USE, TOKEN_ELEVATION,
    TOKEN_QUERY, TOKEN_USER, TokenElevation, TokenUser, TokenVirtualizationAllowed,
    TokenVirtualizationEnabled,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_NORMAL, GetDriveTypeW, GetFileVersionInfoSizeW, GetFileVersionInfoW,
    VerQueryValueW,
};
use windows_sys::Win32::Storage::Packaging::Appx::GetPackageFamilyName;
use windows_sys::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
};
use windows_sys::Win32::System::SystemInformation::{
    IMAGE_FILE_MACHINE, IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM64,
    IMAGE_FILE_MACHINE_I386, IMAGE_FILE_MACHINE_UNKNOWN,
};
use windows_sys::Win32::System::Threading::{
    GetProcessInformation, IsWow64Process2, OpenProcessToken,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_STATE, PROCESS_QUERY_LIMITED_INFORMATION, ProcessPowerThrottling,
    QueryFullProcessImageNameW,
};
use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOTE;
use windows_sys::Win32::UI::Shell::{
    SHFILEINFOW, SHGFI_ICON, SHGFI_SMALLICON, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

use crate::circuit_breaker::CircuitBreaker;
use crate::handle::{OwnedHandle, null_wide_ptr, open_verified_process, to_wide_null};
use crate::perf::cpu::probe_hardware_static_info as probe_win_hardware_static_info;
use crate::perf::disk::DiskPerformanceProber;

/// Target downsampled icon width and height in pixels (`20 x 20` RGBA8 = 1,600 bytes).
const ICON_TARGET_DIM: u16 = 20;

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

/// RAII guard for Win32 `HICON` handles calling `DestroyIcon` on drop.
struct IconGuard(HICON);

impl Drop for IconGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is an exclusively owned `HICON`.
            unsafe {
                DestroyIcon(self.0);
            }
        }
    }
}

/// RAII guard for Win32 GDI objects (`HBITMAP`, etc.) calling `DeleteObject` on drop.
struct GdiObjectGuard(HGDIOBJ);

impl Drop for GdiObjectGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is an exclusively owned GDI object handle.
            unsafe {
                DeleteObject(self.0);
            }
        }
    }
}

/// RAII guard for Win32 memory `HDC` calling `DeleteDC` on drop.
struct DcGuard(HDC);

impl Drop for DcGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is an exclusively owned memory device context.
            unsafe {
                DeleteDC(self.0);
            }
        }
    }
}

/// Windows slow-lane bulkhead metadata prober.
///
/// Wraps a `300 ms` [`CircuitBreaker`] (`60 s` cooldown), a per-drive `GetDriveTypeW`
/// cache for network-drive bypass, and a binary-SID -> account-name cache.
#[derive(Debug, Default)]
pub struct WindowsSlowProber {
    circuit_breaker: CircuitBreaker,
    drive_type_cache: RwLock<HashMap<u8, u32>>,
    sid_cache: RwLock<HashMap<Vec<u8>, Arc<str>>>,
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

    /// Resolves a binary Windows SID to a cached `"DOMAIN\\User"` (or `"User"`) [`Arc<str>`].
    fn resolve_sid(&self, sid_ptr: *mut c_void) -> Option<Arc<str>> {
        if sid_ptr.is_null() {
            return None;
        }

        // SAFETY: `sid_ptr` points to a valid Windows SID returned by `GetTokenInformation`.
        let sid_len = unsafe { GetLengthSid(sid_ptr) } as usize;
        if sid_len == 0 || sid_len > 256 {
            return None;
        }

        // SAFETY: `sid_ptr` is readable for `sid_len` bytes.
        let sid_bytes = unsafe { std::slice::from_raw_parts(sid_ptr.cast::<u8>(), sid_len) };

        if let Some(cached) = self.sid_cache.read().get(sid_bytes) {
            return Some(Arc::clone(cached));
        }

        let mut name_len: u32 = 0;
        let mut domain_len: u32 = 0;
        let mut sid_use: SID_NAME_USE = 0;

        // First call determines required buffer lengths.
        // SAFETY: Null output buffers are documented for size discovery in `LookupAccountSidW`.
        unsafe {
            LookupAccountSidW(
                null_wide_ptr(),
                sid_ptr,
                ptr::null_mut(),
                &mut name_len,
                ptr::null_mut(),
                &mut domain_len,
                &mut sid_use,
            );
        }

        if name_len == 0 {
            return None;
        }

        let mut name_buf = vec![0u16; name_len as usize];
        let mut domain_buf = vec![0u16; domain_len.max(1) as usize];

        // SAFETY: Both buffers are sized according to the preceding `LookupAccountSidW` query.
        let ok = unsafe {
            LookupAccountSidW(
                null_wide_ptr(),
                sid_ptr,
                name_buf.as_mut_ptr(),
                &mut name_len,
                domain_buf.as_mut_ptr(),
                &mut domain_len,
                &mut sid_use,
            )
        };
        if ok == 0 {
            return None;
        }

        let name = String::from_utf16_lossy(&name_buf[..(name_len as usize).min(name_buf.len())]);
        let domain =
            String::from_utf16_lossy(&domain_buf[..(domain_len as usize).min(domain_buf.len())]);

        let formatted: Arc<str> = if domain.is_empty() {
            Arc::from(name)
        } else {
            Arc::from(format!("{domain}\\{name}"))
        };

        self.sid_cache
            .write()
            .insert(sid_bytes.to_vec(), Arc::clone(&formatted));
        Some(formatted)
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
            query_token_metadata(handle.as_raw(), self);

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
            let icon = extract_icon_20x20(trimmed, true);
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
        let icon = extract_icon_20x20(trimmed, false);

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
fn extract_drive_letter(path: &str) -> Option<u8> {
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
fn circuit_target_key(path: &str) -> String {
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

/// Reads the full Win32 executable path via `QueryFullProcessImageNameW`.
fn query_process_image_path(handle: HANDLE) -> Option<String> {
    let mut cap = 512usize;
    for _ in 0..4 {
        let mut buf = vec![0u16; cap];
        let mut len = cap as u32;
        // SAFETY: `handle` has `PROCESS_QUERY_LIMITED_INFORMATION` and `buf` is writable for `len`.
        let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len) };
        if ok != 0 {
            return Some(String::from_utf16_lossy(
                &buf[..(len as usize).min(buf.len())],
            ));
        }
        // SAFETY: `GetLastError` reads thread-local error from `QueryFullProcessImageNameW`.
        if unsafe { GetLastError() } == ERROR_INSUFFICIENT_BUFFER && cap < 32_768 {
            cap = (cap * 2).min(32_768);
            continue;
        }
        return None;
    }
    None
}

/// Reads the target process command line purely from kernel memory via
/// `ntdll!NtQueryInformationProcess(ProcessCommandLineInformation)`.
///
/// Strictly avoids cross-process `ReadProcessMemory` on the target PEB.
fn query_process_command_line(handle: HANDLE) -> Option<String> {
    let mut buf: Vec<u64> = vec![0u64; 128]; // 1024 bytes, 8-byte aligned

    for _ in 0..3 {
        let buf_bytes = buf.len() * size_of::<u64>();
        let mut return_len: u32 = 0;

        // SAFETY: `buf` is 8-byte aligned and writable for `buf_bytes`.
        let status = unsafe {
            NtQueryInformationProcess(
                handle,
                ProcessCommandLineInformation,
                buf.as_mut_ptr().cast::<c_void>(),
                buf_bytes as u32,
                &mut return_len,
            )
        };

        if status == STATUS_SUCCESS {
            if buf_bytes < size_of::<UNICODE_STRING>() {
                return None;
            }
            // SAFETY: Buffer is 8-byte aligned and holds at least `UNICODE_STRING`.
            let us = unsafe { ptr::read(buf.as_ptr().cast::<UNICODE_STRING>()) };
            if us.Length == 0 || us.Buffer.is_null() {
                return Some(String::new());
            }
            let base = buf.as_ptr() as usize;
            let end = base.saturating_add(buf_bytes);
            let ptr_addr = us.Buffer as usize;
            let byte_len = usize::from(us.Length);
            if ptr_addr < base || ptr_addr.saturating_add(byte_len) > end {
                return None;
            }
            let code_units = byte_len / size_of::<u16>();
            // SAFETY: Validated `us.Buffer` slice lies strictly within `buf`.
            let wide = unsafe { std::slice::from_raw_parts(us.Buffer, code_units) };
            return Some(String::from_utf16_lossy(wide));
        }

        if (status == STATUS_INFO_LENGTH_MISMATCH || status == STATUS_BUFFER_TOO_SMALL)
            && return_len > 0
            && (return_len as usize) <= 1024 * 1024
        {
            let needed_u64s = (return_len as usize)
                .saturating_add(64)
                .div_ceil(size_of::<u64>());
            buf.resize(needed_u64s, 0);
            continue;
        }

        return None;
    }

    None
}

/// Determines target process CPU architecture via `IsWow64Process2`.
fn query_process_arch(handle: HANDLE) -> ProcessArch {
    let mut process_machine: IMAGE_FILE_MACHINE = IMAGE_FILE_MACHINE_UNKNOWN;
    let mut native_machine: IMAGE_FILE_MACHINE = IMAGE_FILE_MACHINE_UNKNOWN;

    // SAFETY: `handle` has `PROCESS_QUERY_LIMITED_INFORMATION` and out pointers are stack locals.
    let ok = unsafe { IsWow64Process2(handle, &mut process_machine, &mut native_machine) };
    if ok == 0 {
        return ProcessArch::Unknown;
    }

    let effective = if process_machine == IMAGE_FILE_MACHINE_UNKNOWN {
        native_machine
    } else {
        process_machine
    };

    match effective {
        IMAGE_FILE_MACHINE_I386 => ProcessArch::X86,
        IMAGE_FILE_MACHINE_AMD64 => ProcessArch::X64,
        IMAGE_FILE_MACHINE_ARM64 => ProcessArch::Arm64,
        _ => ProcessArch::Unknown,
    }
}

/// Queries whether EcoQoS / Efficiency Mode is active on `handle` via
/// `GetProcessInformation(ProcessPowerThrottling)`.
fn query_efficiency_mode(handle: HANDLE) -> bool {
    let mut state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: 0,
        StateMask: 0,
    };
    // SAFETY: `handle` has `PROCESS_QUERY_LIMITED_INFORMATION` and `state` matches `ProcessPowerThrottling`.
    let ok = unsafe {
        GetProcessInformation(
            handle,
            ProcessPowerThrottling,
            (&mut state as *mut PROCESS_POWER_THROTTLING_STATE).cast::<c_void>(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    };
    ok != 0 && (state.ControlMask & state.StateMask & PROCESS_POWER_THROTTLING_EXECUTION_SPEED) != 0
}

/// Queries the MSIX/UWP Package Family Name for packaged Windows apps via `GetPackageFamilyName`.
///
/// Returns `None` for non-packaged Win32 desktop processes (`APPMODEL_ERROR_NO_PACKAGE`).
fn query_process_package_family_name(handle: HANDLE) -> Option<String> {
    let mut stack_buf = [0u16; 128];
    let mut len = stack_buf.len() as u32;

    // SAFETY: `handle` has `PROCESS_QUERY_LIMITED_INFORMATION`; `stack_buf` is writable for `len` u16s.
    let rc = unsafe { GetPackageFamilyName(handle, &mut len, stack_buf.as_mut_ptr()) };
    if rc == ERROR_SUCCESS {
        let char_len = (len as usize).saturating_sub(1).min(stack_buf.len());
        let name = String::from_utf16_lossy(&stack_buf[..char_len]);
        let trimmed = name.trim();
        return (!trimmed.is_empty()).then(|| trimmed.to_owned());
    }

    if rc == ERROR_INSUFFICIENT_BUFFER && len > 1 && len <= 1024 {
        let mut heap_buf = vec![0u16; len as usize];
        // SAFETY: `heap_buf` is sized to the exact length returned by `GetPackageFamilyName`.
        let rc2 = unsafe { GetPackageFamilyName(handle, &mut len, heap_buf.as_mut_ptr()) };
        if rc2 == ERROR_SUCCESS {
            let char_len = (len as usize).saturating_sub(1).min(heap_buf.len());
            let name = String::from_utf16_lossy(&heap_buf[..char_len]);
            let trimmed = name.trim();
            return (!trimmed.is_empty()).then(|| trimmed.to_owned());
        }
    }

    None
}

/// Queries process token owner account name, UAC virtualization state, and elevation status.
fn query_token_metadata(
    proc_handle: HANDLE,
    prober: &WindowsSlowProber,
) -> (Option<Arc<str>>, UacVirtualization, bool) {
    let mut raw_token: HANDLE = ptr::null_mut();
    // SAFETY: `OpenProcessToken` writes a new token handle into `raw_token` when successful.
    let ok = unsafe { OpenProcessToken(proc_handle, TOKEN_QUERY, &mut raw_token) };
    if ok == 0 {
        return (None, UacVirtualization::Unknown, false);
    }
    let Some(token) = OwnedHandle::from_raw(raw_token) else {
        return (None, UacVirtualization::Unknown, false);
    };

    let uac = query_uac_virtualization(token.as_raw());
    let user = query_token_user(token.as_raw(), prober);
    let is_elevated = query_token_elevation(token.as_raw());
    (user, uac, is_elevated)
}

/// Queries `TokenElevation` from an open `TOKEN_QUERY` handle.
fn query_token_elevation(token: HANDLE) -> bool {
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut ret_len: u32 = 0;
    // SAFETY: `elevation` matches `TokenElevation` output size.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast::<c_void>(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        )
    };
    ok != 0 && elevation.TokenIsElevated != 0
}

/// Queries `TokenVirtualizationAllowed` and `TokenVirtualizationEnabled` from a process token.
fn query_uac_virtualization(token: HANDLE) -> UacVirtualization {
    let mut allowed: u32 = 0;
    let mut ret_len: u32 = 0;
    // SAFETY: Output buffer holds one `u32` as required by `TokenVirtualizationAllowed`.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenVirtualizationAllowed,
            (&mut allowed as *mut u32).cast::<c_void>(),
            size_of::<u32>() as u32,
            &mut ret_len,
        )
    };
    if ok == 0 {
        return UacVirtualization::Unknown;
    }
    if allowed == 0 {
        return UacVirtualization::NotAllowed;
    }

    let mut enabled: u32 = 0;
    // SAFETY: Output buffer holds one `u32` as required by `TokenVirtualizationEnabled`.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenVirtualizationEnabled,
            (&mut enabled as *mut u32).cast::<c_void>(),
            size_of::<u32>() as u32,
            &mut ret_len,
        )
    };
    if ok == 0 {
        UacVirtualization::Unknown
    } else if enabled != 0 {
        UacVirtualization::Enabled
    } else {
        UacVirtualization::Disabled
    }
}

/// Queries `TokenUser` from a process token and resolves its SID via `prober`'s SID cache.
fn query_token_user(token: HANDLE, prober: &WindowsSlowProber) -> Option<Arc<str>> {
    let mut needed: u32 = 0;
    // SAFETY: Size-query call with null buffer.
    unsafe {
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed);
    }
    if needed == 0 || (needed as usize) < size_of::<TOKEN_USER>() || needed > 4096 {
        return None;
    }

    let mut buf: Vec<u64> = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
    let buf_bytes = (buf.len() * size_of::<u64>()) as u32;

    // SAFETY: `buf` is 8-byte aligned and sized to at least `needed` bytes.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast::<c_void>(),
            buf_bytes,
            &mut needed,
        )
    };
    if ok == 0 {
        return None;
    }

    // SAFETY: `TOKEN_USER` is written at offset 0 of `buf`.
    let token_user = unsafe { ptr::read(buf.as_ptr().cast::<TOKEN_USER>()) };
    prober.resolve_sid(token_user.User.Sid)
}

/// Reads local PE version resources (`CompanyName`, `FileDescription`) without performing
/// any network certificate revocation (CRL/OCSP) checks.
fn read_pe_version_info(exe_path: &str) -> (Option<Arc<str>>, Option<Arc<str>>) {
    let wide_path = to_wide_null(exe_path);
    let mut unused_handle: u32 = 0;

    // SAFETY: `wide_path` is null-terminated UTF-16.
    let size = unsafe { GetFileVersionInfoSizeW(wide_path.as_ptr(), &mut unused_handle) };
    if size == 0 || size > 4 * 1024 * 1024 {
        return (None, None);
    }

    let mut data = vec![0u8; size as usize];
    // SAFETY: `data` has capacity `size` bytes matching `GetFileVersionInfoSizeW`.
    let ok = unsafe {
        GetFileVersionInfoW(
            wide_path.as_ptr(),
            0,
            size,
            data.as_mut_ptr().cast::<c_void>(),
        )
    };
    if ok == 0 {
        return (None, None);
    }

    let translations = read_version_translations(&data);

    let mut publisher = None;
    let mut description = None;

    for &(lang, codepage) in &translations {
        if publisher.is_none() {
            publisher = query_version_string(&data, lang, codepage, "CompanyName");
        }
        if description.is_none() {
            description = query_version_string(&data, lang, codepage, "FileDescription");
        }
        if publisher.is_some() && description.is_some() {
            break;
        }
    }

    (publisher, description)
}

/// Extracts `(language, codepage)` pairs from `\VarFileInfo\Translation`, appending
/// standard fallback locales if not already present.
fn read_version_translations(data: &[u8]) -> Vec<(u16, u16)> {
    let mut result = Vec::with_capacity(4);
    let sub_block = to_wide_null(r"\VarFileInfo\Translation");
    let mut ptr_out: *mut c_void = ptr::null_mut();
    let mut len_bytes: u32 = 0;

    // SAFETY: `data` is a valid version resource buffer populated by `GetFileVersionInfoW`.
    let ok = unsafe {
        VerQueryValueW(
            data.as_ptr().cast::<c_void>(),
            sub_block.as_ptr(),
            &mut ptr_out,
            &mut len_bytes,
        )
    };

    if ok != 0 && !ptr_out.is_null() && len_bytes >= 4 {
        let count = (len_bytes as usize) / 4;
        for i in 0..count.min(8) {
            // SAFETY: `VerQueryValueW` guarantees `len_bytes` readable bytes at `ptr_out`.
            let pair = unsafe { ptr::read_unaligned(ptr_out.cast::<[u16; 2]>().add(i)) };
            result.push((pair[0], pair[1]));
        }
    }

    // Standard fallback translations (US English Unicode, US English ANSI, Neutral Unicode).
    for fallback in [(0x0409u16, 0x04B0u16), (0x0409, 0x04E4), (0x0000, 0x04B0)] {
        if !result.contains(&fallback) {
            result.push(fallback);
        }
    }

    result
}

/// Queries a single UTF-16 string from `\StringFileInfo\{lang}{cp}\{field}`.
fn query_version_string(data: &[u8], lang: u16, codepage: u16, field: &str) -> Option<Arc<str>> {
    let query = format!(r"\StringFileInfo\{lang:04x}{codepage:04x}\{field}");
    let wide_query = to_wide_null(&query);
    let mut ptr_out: *mut c_void = ptr::null_mut();
    let mut len_chars: u32 = 0;

    // SAFETY: `data` is a valid version resource buffer and `wide_query` is null-terminated.
    let ok = unsafe {
        VerQueryValueW(
            data.as_ptr().cast::<c_void>(),
            wide_query.as_ptr(),
            &mut ptr_out,
            &mut len_chars,
        )
    };
    if ok == 0 || ptr_out.is_null() || len_chars == 0 {
        return None;
    }

    // SAFETY: `VerQueryValueW` sets `len_chars` to the UTF-16 code unit count (including null).
    let raw_slice =
        unsafe { std::slice::from_raw_parts(ptr_out.cast::<u16>(), len_chars as usize) };
    let trimmed_len = raw_slice
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(raw_slice.len());
    let decoded = String::from_utf16_lossy(&raw_slice[..trimmed_len]);
    let trimmed = decoded.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(Arc::from(trimmed))
    }
}

/// Extracts an executable icon via `SHGetFileInfoW` and resamples it to `20x20` RGBA8.
///
/// When `memory_only` is `true` (for UNC / `DRIVE_REMOTE` paths), passes
/// `SHGFI_USEFILEATTRIBUTES` so Windows resolves the generic extension icon from memory
/// without touching the network share.
fn extract_icon_20x20(exe_path: &str, memory_only: bool) -> Option<ExeIconRgba> {
    let wide_path = if memory_only {
        // Extension-only hint (e.g. ".exe") avoids any path resolution against remote shares.
        let ext = std::path::Path::new(exe_path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
            .unwrap_or_else(|| ".exe".to_owned());
        to_wide_null(&ext)
    } else {
        to_wide_null(exe_path)
    };

    // SAFETY: `SHFILEINFOW` is POD.
    let mut shfi: SHFILEINFOW = unsafe { zeroed() };
    let mut flags = SHGFI_ICON | SHGFI_SMALLICON;
    let file_attrs = if memory_only {
        flags |= SHGFI_USEFILEATTRIBUTES;
        FILE_ATTRIBUTE_NORMAL
    } else {
        0
    };

    // SAFETY: `wide_path` is null-terminated and `shfi` has valid size.
    let res = unsafe {
        SHGetFileInfoW(
            wide_path.as_ptr(),
            file_attrs,
            &mut shfi,
            size_of::<SHFILEINFOW>() as u32,
            flags,
        )
    };
    if res == 0 || shfi.hIcon.is_null() {
        return None;
    }

    let icon_guard = IconGuard(shfi.hIcon);
    hicon_to_rgba_20x20(icon_guard.0)
}

/// Converts a Win32 `HICON` into a `20x20` RGBA8 [`ExeIconRgba`], ensuring every GDI
/// bitmap and device context handle is released via RAII guards.
fn hicon_to_rgba_20x20(hicon: HICON) -> Option<ExeIconRgba> {
    // SAFETY: `ICONINFO` is POD.
    let mut icon_info: ICONINFO = unsafe { zeroed() };
    // SAFETY: `hicon` is a valid icon handle owned by caller's `IconGuard`.
    if unsafe { GetIconInfo(hicon, &mut icon_info) } == 0 {
        return None;
    }

    // Wrap both bitmaps immediately so no early return can leak GDI handles.
    let color_guard = GdiObjectGuard(icon_info.hbmColor.cast::<c_void>());
    let mask_guard = GdiObjectGuard(icon_info.hbmMask.cast::<c_void>());

    if color_guard.0.is_null() {
        return None;
    }

    // SAFETY: `BITMAP` is POD.
    let mut bmp: BITMAP = unsafe { zeroed() };
    // SAFETY: `color_guard.0` is a valid `HBITMAP`.
    let got = unsafe {
        GetObjectW(
            color_guard.0,
            size_of::<BITMAP>() as i32,
            (&mut bmp as *mut BITMAP).cast::<c_void>(),
        )
    };
    if got == 0 || bmp.bmWidth <= 0 || bmp.bmHeight <= 0 || bmp.bmWidth > 512 || bmp.bmHeight > 512
    {
        return None;
    }

    let src_w = bmp.bmWidth as usize;
    let src_h = bmp.bmHeight as usize;

    // SAFETY: Creating a memory DC compatible with the screen.
    let dc = DcGuard(unsafe { CreateCompatibleDC(ptr::null_mut()) });
    if dc.0.is_null() {
        return None;
    }

    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: bmp.bmWidth,
            // Negative height requests a top-down DIB.
            biHeight: -bmp.bmHeight,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..Default::default()
        },
        bmiColors: [Default::default()],
    };

    let mut bgra = vec![0u8; src_w * src_h * 4];
    // SAFETY: `bgra` has exact byte capacity for a `src_w * src_h` 32bpp DIB.
    let scanlines = unsafe {
        GetDIBits(
            dc.0,
            icon_info.hbmColor,
            0,
            src_h as u32,
            bgra.as_mut_ptr().cast::<c_void>(),
            &mut bmi,
            DIB_RGB_COLORS,
        )
    };
    if scanlines == 0 {
        return None;
    }

    // Legacy 24bpp icons leave all alpha bytes as 0; reconstruct alpha from `hbmMask` if so.
    let all_alpha_zero = bgra.as_chunks::<4>().0.iter().all(|px| px[3] == 0);
    if all_alpha_zero && !mask_guard.0.is_null() {
        let mut mask_bgra = vec![0u8; src_w * src_h * 4];
        // SAFETY: `mask_bgra` has capacity for `src_w * src_h` 32bpp DIB pixels.
        let mask_lines = unsafe {
            GetDIBits(
                dc.0,
                icon_info.hbmMask,
                0,
                src_h as u32,
                mask_bgra.as_mut_ptr().cast::<c_void>(),
                &mut bmi,
                DIB_RGB_COLORS,
            )
        };
        if mask_lines != 0 {
            for (px, mpx) in bgra
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(mask_bgra.as_chunks::<4>().0.iter())
            {
                // Mask pixel 0 (black) = opaque foreground, non-zero (white) = transparent.
                px[3] = if mpx[0] == 0 { 255 } else { 0 };
            }
        } else {
            for px in bgra.as_chunks_mut::<4>().0 {
                px[3] = 255;
            }
        }
    }

    // Convert BGRA -> RGBA in place.
    for px in bgra.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }

    let dst_dim = usize::from(ICON_TARGET_DIM);
    let rgba = if src_w == dst_dim && src_h == dst_dim {
        bgra
    } else {
        resample_rgba_bilinear(&bgra, src_w, src_h, dst_dim, dst_dim)
    };

    Some(ExeIconRgba {
        width: ICON_TARGET_DIM,
        height: ICON_TARGET_DIM,
        rgba: Arc::from(rgba),
    })
}

/// Bilinear resampler converting a tightly packed `src_w x src_h` RGBA8 image into
/// `dst_w x dst_h` RGBA8.
fn resample_rgba_bilinear(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
) -> Vec<u8> {
    let mut dst = vec![0u8; dst_w * dst_h * 4];
    let x_scale = src_w as f32 / dst_w as f32;
    let y_scale = src_h as f32 / dst_h as f32;

    for dy in 0..dst_h {
        let sy = ((dy as f32 + 0.5) * y_scale - 0.5).clamp(0.0, (src_h - 1) as f32);
        let y0 = sy.floor() as usize;
        let y1 = (y0 + 1).min(src_h - 1);
        let fy = sy - y0 as f32;

        for dx in 0..dst_w {
            let sx = ((dx as f32 + 0.5) * x_scale - 0.5).clamp(0.0, (src_w - 1) as f32);
            let x0 = sx.floor() as usize;
            let x1 = (x0 + 1).min(src_w - 1);
            let fx = sx - x0 as f32;

            let i00 = (y0 * src_w + x0) * 4;
            let i10 = (y0 * src_w + x1) * 4;
            let i01 = (y1 * src_w + x0) * 4;
            let i11 = (y1 * src_w + x1) * 4;

            let w00 = (1.0 - fx) * (1.0 - fy);
            let w10 = fx * (1.0 - fy);
            let w01 = (1.0 - fx) * fy;
            let w11 = fx * fy;

            let out_idx = (dy * dst_w + dx) * 4;
            for c in 0..4 {
                let val = f32::from(src[i00 + c]) * w00
                    + f32::from(src[i10 + c]) * w10
                    + f32::from(src[i01 + c]) * w01
                    + f32::from(src[i11 + c]) * w11;
                dst[out_idx + c] = val.round().clamp(0.0, 255.0) as u8;
            }
        }
    }

    dst
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_path_detection_handles_unc_extended_and_local_paths() {
        let prober = WindowsSlowProber::new();

        // Standard and extended UNC paths must always be detected as network paths.
        assert!(is_unc_path(r"\\server\share\app.exe"));
        assert!(is_unc_path(r"\\?\UNC\server\share\app.exe"));
        assert!(is_unc_path(r"\\.\UNC\fileserver\tools\tool.exe"));
        assert!(is_unc_path("//server/share/app.exe"));
        assert!(prober.is_network_path(r"\\server\share\app.exe"));
        assert!(prober.is_network_path(r"\\?\UNC\server\share\app.exe"));

        // Local system drive paths must NOT be flagged as UNC or network paths.
        assert!(!is_unc_path(r"C:\Windows\notepad.exe"));
        assert!(!is_unc_path(r"\\?\C:\Windows\notepad.exe"));
        assert!(!prober.is_network_path(r"C:\Windows\notepad.exe"));
        assert!(!prober.is_network_path(r"\\?\C:\Windows\notepad.exe"));

        // Target keys group by drive letter or UNC share.
        assert_eq!(circuit_target_key(r"C:\Windows\notepad.exe"), "C:");
        assert_eq!(circuit_target_key(r"\\?\d:\Apps\demo.exe"), "D:");
        assert_eq!(
            circuit_target_key(r"\\filesrv\share\bin\tool.exe"),
            r"\\filesrv\share"
        );
        assert_eq!(
            circuit_target_key(r"\\?\UNC\filesrv\share\bin\tool.exe"),
            r"\\filesrv\share"
        );
    }
}
