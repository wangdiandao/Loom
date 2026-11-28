//! Windows startup applications enumeration and `StartupApproved` / UWP toggle manager.
//!
//! Aggregates:
//! 1. Registry `Run` keys (`HKCU`, `HKLM` 64-bit, `HKLM\WOW6432Node`) cross-referenced
//!    with `Explorer\StartupApproved\Run` and `Run32`, resolving human-readable PE
//!    names (`FileDescription` / `ProductName`), `CompanyName` publishers, and DPI-matched
//!    native RGBA icons.
//! 2. Per-user and common Startup folders cross-referenced with `StartupApproved\StartupFolder`.
//! 3. UWP / MSIX `StartupTask` entries under `AppModel\SystemAppData`, resolving localized
//!    task/app display names and publishers from `AppxManifest.xml` + `SHLoadIndirectString`
//!    and loading DPI-matched RGBA package icons (`Square44x44Logo` / `Logo`).
//!
//! Strictly bypasses UNC and `DRIVE_REMOTE` paths before reading PE version resources,
//! manifests, or icons, and maps Win32 `ERROR_ACCESS_DENIED` to [`PalError::ElevationRequired`].

mod folder;
mod registry;
#[cfg(test)]
mod tests;
mod uwp;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use loom_pal_api::{PalError, StartupManager};
use loom_types::{ExeIconRgba, StartupEntry};
use parking_lot::RwLock;
use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_WOW64_64KEY,
};
use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOTE;

use crate::slow_prober::{
    ensure_com_initialized, extract_drive_letter, extract_icon_rgba, is_unc_path,
    read_pe_version_info_full,
};

use registry::{
    APPROVED_FOLDER_SUBKEY, APPROVED_RUN_SUBKEY, APPROVED_RUN32_SUBKEY, RUN_SUBKEY, RUN32_SUBKEY,
    UWP_STARTUP_ROOT, read_last_bios_time_ms, write_reg_dword, write_startup_approved,
};
use uwp::UwpPackageMeta;

/// Parsed target location encoded in [`StartupEntry::id`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum StartupIdTarget<'a> {
    HkcuRun(&'a str),
    HklmRun(&'a str),
    HklmRun32(&'a str),
    UserFolder(&'a str),
    CommonFolder(&'a str),
    Uwp { package: &'a str, task: &'a str },
}

/// Cached local PE metadata and icon for a Win32 startup target file.
#[derive(Debug, Default, Clone)]
struct FileStartupMeta {
    publisher: Option<Arc<str>>,
    file_description: Option<Arc<str>>,
    product_name: Option<Arc<str>>,
    icon: Option<ExeIconRgba>,
}

/// Windows implementation of [`StartupManager`].
#[derive(Debug, Default)]
pub struct WindowsStartupManager {
    file_meta_cache: RwLock<HashMap<String, FileStartupMeta>>,
    uwp_pkg_cache: RwLock<HashMap<String, Option<UwpPackageMeta>>>,
    drive_type_cache: RwLock<HashMap<u8, u32>>,
}

impl WindowsStartupManager {
    /// Creates a new Windows startup applications manager.
    pub fn new() -> Self {
        Self::default()
    }

    fn is_network_path(&self, path: &str) -> bool {
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
        // SAFETY: `root_wide` is a valid null-terminated UTF-16 drive root string.
        let drive_type = unsafe { GetDriveTypeW(root_wide.as_ptr()) };
        self.drive_type_cache.write().insert(drive, drive_type);
        drive_type == DRIVE_REMOTE
    }

    /// Resolves missing `.exe` extensions or bare System32 image names for local paths
    /// without ever touching UNC or network-mapped paths.
    fn resolve_existing_local_path(&self, raw_path: &str) -> String {
        let trimmed = raw_path.trim();
        if trimmed.is_empty() || self.is_network_path(trimmed) {
            return trimmed.to_owned();
        }
        let candidate = Path::new(trimmed);
        if candidate.is_file() {
            return trimmed.to_owned();
        }
        if candidate.extension().is_none() {
            let with_exe = format!("{trimmed}.exe");
            if Path::new(&with_exe).is_file() {
                return with_exe;
            }
        }
        if !trimmed.contains('\\')
            && !trimmed.contains('/')
            && let Some(sys_root) = std::env::var_os("SystemRoot")
        {
            let sys32 = PathBuf::from(sys_root).join("System32").join(trimmed);
            if sys32.is_file() {
                return sys32.to_string_lossy().into_owned();
            }
            if sys32.extension().is_none() {
                let mut with_exe = sys32.into_os_string();
                with_exe.push(".exe");
                let path_exe = PathBuf::from(with_exe);
                if path_exe.is_file() {
                    return path_exe.to_string_lossy().into_owned();
                }
            }
        }
        trimmed.to_owned()
    }

    fn lookup_file_meta(&self, file_path: &str) -> FileStartupMeta {
        let trimmed = file_path.trim();
        if trimmed.is_empty() {
            return FileStartupMeta::default();
        }

        let cache_key = trimmed.to_ascii_lowercase();
        if let Some(cached) = self.file_meta_cache.read().get(&cache_key) {
            return cached.clone();
        }

        let meta = if self.is_network_path(trimmed) {
            ensure_com_initialized();
            FileStartupMeta {
                publisher: None,
                file_description: None,
                product_name: None,
                icon: extract_icon_rgba(trimmed, true),
            }
        } else if Path::new(trimmed).is_file() {
            ensure_com_initialized();
            let pe = read_pe_version_info_full(trimmed);
            let icon = extract_icon_rgba(trimmed, false);
            FileStartupMeta {
                publisher: pe.publisher,
                file_description: pe.file_description,
                product_name: pe.product_name,
                icon,
            }
        } else {
            FileStartupMeta::default()
        };

        self.file_meta_cache.write().insert(cache_key, meta.clone());
        meta
    }

    fn lookup_uwp_package_meta(&self, package_family_name: &str) -> Option<UwpPackageMeta> {
        let cache_key = package_family_name.to_ascii_lowercase();
        if let Some(cached) = self.uwp_pkg_cache.read().get(&cache_key) {
            return cached.clone();
        }

        let resolved = uwp::resolve_uwp_package_meta_uncached(self, package_family_name);
        self.uwp_pkg_cache
            .write()
            .insert(cache_key, resolved.clone());
        resolved
    }
}

impl StartupManager for WindowsStartupManager {
    fn list_startup_entries(&self) -> Result<(Option<u64>, Vec<StartupEntry>), PalError> {
        let last_bios_time_ms = read_last_bios_time_ms();
        let mut entries = Vec::with_capacity(32);

        // 1. Registry Run keys (HKCU, HKLM 64-bit, HKLM WOW6432Node).
        registry::collect_run_key(
            self,
            HKEY_CURRENT_USER,
            RUN_SUBKEY,
            APPROVED_RUN_SUBKEY,
            0,
            "hkcu-run",
            &mut entries,
        );
        registry::collect_run_key(
            self,
            HKEY_LOCAL_MACHINE,
            RUN_SUBKEY,
            APPROVED_RUN_SUBKEY,
            KEY_WOW64_64KEY,
            "hklm-run",
            &mut entries,
        );
        registry::collect_run_key(
            self,
            HKEY_LOCAL_MACHINE,
            RUN32_SUBKEY,
            APPROVED_RUN32_SUBKEY,
            KEY_WOW64_64KEY,
            "hklm-run32",
            &mut entries,
        );

        // 2. Startup folders (User + Common).
        if let Some(appdata) = std::env::var_os("APPDATA") {
            let user_startup =
                PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\Startup");
            folder::collect_startup_folder(
                self,
                &user_startup,
                HKEY_CURRENT_USER,
                0,
                "user-folder",
                &mut entries,
            );
        }
        if let Some(program_data) = std::env::var_os("PROGRAMDATA") {
            let common_startup =
                PathBuf::from(program_data).join(r"Microsoft\Windows\Start Menu\Programs\StartUp");
            folder::collect_startup_folder(
                self,
                &common_startup,
                HKEY_LOCAL_MACHINE,
                KEY_WOW64_64KEY,
                "common-folder",
                &mut entries,
            );
        }

        // 3. UWP StartupTask entries.
        uwp::collect_uwp_startup_tasks(self, &mut entries);

        entries.sort_by(|a, b| {
            a.name
                .as_bytes()
                .iter()
                .map(u8::to_ascii_lowercase)
                .cmp(b.name.as_bytes().iter().map(u8::to_ascii_lowercase))
                .then_with(|| a.id.cmp(&b.id))
        });

        Ok((last_bios_time_ms, entries))
    }

    fn set_startup_enabled(&self, id: &str, enabled: bool) -> Result<(), PalError> {
        let target = parse_startup_id(id)
            .ok_or_else(|| PalError::InvalidArgument(format!("invalid startup entry id: {id}")))?;

        match target {
            StartupIdTarget::HkcuRun(val) => {
                write_startup_approved(HKEY_CURRENT_USER, APPROVED_RUN_SUBKEY, 0, val, enabled)
            }
            StartupIdTarget::HklmRun(val) => write_startup_approved(
                HKEY_LOCAL_MACHINE,
                APPROVED_RUN_SUBKEY,
                KEY_WOW64_64KEY,
                val,
                enabled,
            ),
            StartupIdTarget::HklmRun32(val) => write_startup_approved(
                HKEY_LOCAL_MACHINE,
                APPROVED_RUN32_SUBKEY,
                KEY_WOW64_64KEY,
                val,
                enabled,
            ),
            StartupIdTarget::UserFolder(file) => {
                write_startup_approved(HKEY_CURRENT_USER, APPROVED_FOLDER_SUBKEY, 0, file, enabled)
            }
            StartupIdTarget::CommonFolder(file) => write_startup_approved(
                HKEY_LOCAL_MACHINE,
                APPROVED_FOLDER_SUBKEY,
                KEY_WOW64_64KEY,
                file,
                enabled,
            ),
            StartupIdTarget::Uwp { package, task } => {
                let subkey = format!(r"{UWP_STARTUP_ROOT}\{package}\{task}");
                let state: u32 = if enabled { 2 } else { 1 };
                write_reg_dword(HKEY_CURRENT_USER, &subkey, 0, "State", state)
            }
        }
    }
}

/// Parses a structured [`StartupEntry::id`] back into its registry/folder/UWP target.
fn parse_startup_id(id: &str) -> Option<StartupIdTarget<'_>> {
    let (prefix, rest) = id.split_once(':')?;
    if rest.is_empty() {
        return None;
    }
    match prefix {
        "hkcu-run" => Some(StartupIdTarget::HkcuRun(rest)),
        "hklm-run" => Some(StartupIdTarget::HklmRun(rest)),
        "hklm-run32" => Some(StartupIdTarget::HklmRun32(rest)),
        "user-folder" => Some(StartupIdTarget::UserFolder(rest)),
        "common-folder" => Some(StartupIdTarget::CommonFolder(rest)),
        "uwp" => {
            let (package, task) = rest.split_once('|')?;
            if package.is_empty() || task.is_empty() {
                return None;
            }
            Some(StartupIdTarget::Uwp { package, task })
        }
        _ => None,
    }
}
