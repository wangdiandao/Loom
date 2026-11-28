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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use loom_pal_api::{PalError, StartupManager};
use loom_types::{ExeIconRgba, StartupEntry, StartupImpact};
use parking_lot::RwLock;
use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, S_OK,
};
use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
use windows_sys::Win32::Storage::Packaging::Appx::{
    GetPackagePathByFullName, GetPackagesByPackageFamily,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, KEY_WOW64_64KEY,
    REG_BINARY, REG_DWORD, REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOTE;
use windows_sys::Win32::UI::Shell::SHLoadIndirectString;

use crate::handle::{map_win32_error, to_wide_null};
use crate::slow_prober::{
    ensure_com_initialized, extract_drive_letter, extract_icon_rgba, is_unc_path,
    read_pe_version_info_full, resample_rgba_premultiplied, target_icon_dim,
};

const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN32_SUBKEY: &str = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Run";
const APPROVED_RUN_SUBKEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const APPROVED_RUN32_SUBKEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run32";
const APPROVED_FOLDER_SUBKEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\StartupFolder";
const UWP_STARTUP_ROOT: &str = r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\SystemAppData";
const POWER_SESSION_SUBKEY: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Power";

/// Difference in seconds between Windows FILETIME epoch (`1601-01-01`) and Unix epoch (`1970-01-01`).
const WINDOWS_TO_UNIX_EPOCH_SECS: u64 = 11_644_473_600;

/// Maximum `.lnk` shortcut file size inspected by the offline local-path parser.
const MAX_LNK_INSPECT_BYTES: u64 = 64 * 1024;

/// Maximum `AppxManifest.xml` file size read during UWP startup metadata resolution.
const MAX_MANIFEST_BYTES: u64 = 512 * 1024;

/// Maximum PNG logo asset size decoded for a UWP/MSIX startup icon.
const MAX_LOGO_BYTES: u64 = 2 * 1024 * 1024;

/// RAII owner of a Win32 registry `HKEY`.
struct RegKeyGuard(HKEY);

impl Drop for RegKeyGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is an exclusively owned valid registry key handle.
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }
}

/// Parsed target location encoded in [`StartupEntry::id`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StartupIdTarget<'a> {
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

/// Cached AppxManifest metadata and icon for an installed UWP/MSIX package family.
#[derive(Debug, Clone)]
struct UwpPackageMeta {
    package_display_name: Option<Arc<str>>,
    app_display_name: Option<Arc<str>>,
    publisher: Option<Arc<str>>,
    task_display_names: HashMap<String, Arc<str>>,
    task_executables: HashMap<String, PathBuf>,
    default_executable: Option<PathBuf>,
    icon: Option<ExeIconRgba>,
}

/// Raw fields parsed from `AppxManifest.xml` prior to `ms-resource:` expansion.
#[derive(Debug, Default, PartialEq, Eq)]
struct ParsedAppxManifest {
    package_display_name: Option<String>,
    publisher_display_name: Option<String>,
    store_logo: Option<String>,
    app_display_name: Option<String>,
    square44_logo: Option<String>,
    default_executable: Option<String>,
    startup_tasks: Vec<ParsedStartupTask>,
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedStartupTask {
    task_id: String,
    display_name: Option<String>,
    executable: Option<String>,
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

        let resolved = self.resolve_uwp_package_meta_uncached(package_family_name);
        self.uwp_pkg_cache
            .write()
            .insert(cache_key, resolved.clone());
        resolved
    }

    fn resolve_uwp_package_meta_uncached(
        &self,
        package_family_name: &str,
    ) -> Option<UwpPackageMeta> {
        let (package_full_name, install_dir) = find_installed_package(package_family_name)?;
        let install_dir_str = install_dir.to_string_lossy();
        if self.is_network_path(&install_dir_str) {
            return None;
        }

        let manifest_path = install_dir.join("AppxManifest.xml");
        let manifest_meta = std::fs::metadata(&manifest_path).ok()?;
        if manifest_meta.len() == 0 || manifest_meta.len() > MAX_MANIFEST_BYTES {
            return None;
        }
        let xml = std::fs::read_to_string(&manifest_path).ok()?;
        let parsed = parse_appx_manifest(&xml);

        let resolve_str = |raw: Option<&str>| -> Option<Arc<str>> {
            let value = raw?.trim();
            if value.is_empty() {
                return None;
            }
            resolve_appx_display_string(&package_full_name, package_family_name, value)
                .map(Arc::<str>::from)
        };

        let package_display_name = resolve_str(parsed.package_display_name.as_deref());
        let app_display_name = resolve_str(parsed.app_display_name.as_deref());
        let publisher = resolve_str(parsed.publisher_display_name.as_deref());

        let mut task_display_names = HashMap::new();
        let mut task_executables = HashMap::new();
        for task in &parsed.startup_tasks {
            let task_key = task.task_id.to_ascii_lowercase();
            if let Some(name) = resolve_str(task.display_name.as_deref()) {
                task_display_names.insert(task_key.clone(), name);
            }
            if let Some(ref exe_rel) = task.executable {
                let candidate = resolve_package_relative_path(&install_dir, exe_rel);
                if candidate.is_file() {
                    task_executables.insert(task_key, candidate);
                }
            }
        }

        let default_executable = parsed
            .default_executable
            .as_deref()
            .map(|rel| resolve_package_relative_path(&install_dir, rel))
            .filter(|p| p.is_file());

        let target_dim = target_icon_dim();
        let mut icon = None;
        for candidate_logo in [
            parsed.square44_logo.as_deref(),
            parsed.store_logo.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(logo_path) =
                find_best_uwp_logo_file(&install_dir, candidate_logo, target_dim)
                && let Some(decoded) = load_png_icon_rgba(&logo_path, target_dim)
            {
                icon = Some(decoded);
                break;
            }
        }

        if icon.is_none()
            && let Some(ref exe_path) = default_executable
        {
            ensure_com_initialized();
            icon = extract_icon_rgba(&exe_path.to_string_lossy(), false);
        }

        Some(UwpPackageMeta {
            package_display_name,
            app_display_name,
            publisher,
            task_display_names,
            task_executables,
            default_executable,
            icon,
        })
    }

    fn collect_run_key(
        &self,
        root: HKEY,
        run_subkey: &str,
        approved_subkey: &str,
        sam_extra: u32,
        id_prefix: &str,
        out: &mut Vec<StartupEntry>,
    ) {
        let approved = read_approved_map(root, approved_subkey, sam_extra);
        let values = enum_reg_string_values(root, run_subkey, sam_extra);

        for (val_name, command) in values {
            if val_name.trim().is_empty() {
                continue;
            }
            let enabled = approved
                .get(&val_name.to_ascii_lowercase())
                .copied()
                .unwrap_or(true);
            let file_path =
                extract_executable_path(&command).map(|p| self.resolve_existing_local_path(&p));
            let meta = file_path.as_deref().map(|p| self.lookup_file_meta(p));
            let display_name =
                pick_win32_run_display_name(&val_name, file_path.as_deref(), meta.as_ref());
            let (publisher, icon) = match meta {
                Some(m) => (m.publisher, m.icon),
                None => (None, None),
            };
            let impact = if enabled {
                StartupImpact::NotMeasured
            } else {
                StartupImpact::None
            };

            out.push(StartupEntry {
                id: Arc::from(format!("{id_prefix}:{val_name}")),
                name: Arc::from(display_name),
                publisher,
                command: if command.trim().is_empty() {
                    None
                } else {
                    Some(Arc::from(command))
                },
                file_path: file_path.map(Arc::<str>::from),
                icon,
                enabled,
                impact,
            });
        }
    }

    fn collect_startup_folder(
        &self,
        folder: &Path,
        approved_root: HKEY,
        approved_sam_extra: u32,
        id_prefix: &str,
        out: &mut Vec<StartupEntry>,
    ) {
        let Ok(read_dir) = std::fs::read_dir(folder) else {
            return;
        };
        let approved = read_approved_map(approved_root, APPROVED_FOLDER_SUBKEY, approved_sam_extra);

        for entry in read_dir.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(file_name_os) = path.file_name() else {
                continue;
            };
            let file_name = file_name_os.to_string_lossy();
            if file_name.eq_ignore_ascii_case("desktop.ini") {
                continue;
            }

            let enabled = approved
                .get(&file_name.to_ascii_lowercase())
                .copied()
                .unwrap_or(true);

            let display_name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| file_name.clone().into_owned());

            let raw_path = path.to_string_lossy().into_owned();
            let resolved_target = if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("lnk"))
            {
                parse_lnk_local_target(&path)
            } else {
                None
            };

            let file_path =
                self.resolve_existing_local_path(resolved_target.as_deref().unwrap_or(&raw_path));
            let target_meta = self.lookup_file_meta(&file_path);
            let icon = target_meta.icon.or_else(|| {
                if !self.is_network_path(&raw_path) {
                    ensure_com_initialized();
                    extract_icon_rgba(&raw_path, false)
                } else {
                    None
                }
            });
            let impact = if enabled {
                StartupImpact::NotMeasured
            } else {
                StartupImpact::None
            };

            out.push(StartupEntry {
                id: Arc::from(format!("{id_prefix}:{file_name}")),
                name: Arc::from(display_name),
                publisher: target_meta.publisher,
                command: Some(Arc::from(file_path.clone())),
                file_path: Some(Arc::from(file_path)),
                icon,
                enabled,
                impact,
            });
        }
    }

    fn collect_uwp_startup_tasks(&self, out: &mut Vec<StartupEntry>) {
        let Some(root_key) = open_reg_key(HKEY_CURRENT_USER, UWP_STARTUP_ROOT, KEY_READ) else {
            return;
        };

        for pkg in enum_reg_subkeys(root_key.0) {
            let Some(pkg_key) = open_reg_key(root_key.0, &pkg, KEY_READ) else {
                continue;
            };
            let tasks = enum_reg_subkeys(pkg_key.0);
            let valid_tasks: Vec<(String, u32)> = tasks
                .into_iter()
                .filter_map(|task| {
                    let task_key = open_reg_key(pkg_key.0, &task, KEY_READ)?;
                    let state = read_reg_dword(task_key.0, "State")?;
                    Some((task, state))
                })
                .collect();

            if valid_tasks.is_empty() {
                continue;
            }

            let multi = valid_tasks.len() > 1;
            let pkg_meta = self.lookup_uwp_package_meta(&pkg);
            let fallback_pkg_display = format_uwp_package_name(&pkg);

            for (task, state) in valid_tasks {
                // UWP StartupTask State: 2 = Enabled, 4 = EnabledByPolicy; 0/1/3 = Disabled*.
                let enabled = matches!(state, 2 | 4);
                let task_lower = task.to_ascii_lowercase();

                let name: Arc<str> = if let Some(ref meta) = pkg_meta {
                    if let Some(task_name) = meta.task_display_names.get(&task_lower) {
                        Arc::clone(task_name)
                    } else if let Some(base) = meta
                        .app_display_name
                        .as_ref()
                        .or(meta.package_display_name.as_ref())
                    {
                        if multi {
                            Arc::from(format!("{base} ({task})"))
                        } else {
                            Arc::clone(base)
                        }
                    } else if multi {
                        Arc::from(format!("{fallback_pkg_display} ({task})"))
                    } else {
                        Arc::from(fallback_pkg_display.clone())
                    }
                } else if multi {
                    Arc::from(format!("{fallback_pkg_display} ({task})"))
                } else {
                    Arc::from(fallback_pkg_display.clone())
                };

                let publisher = pkg_meta.as_ref().and_then(|m| m.publisher.clone());
                let exe_path = pkg_meta.as_ref().and_then(|m| {
                    m.task_executables
                        .get(&task_lower)
                        .or(m.default_executable.as_ref())
                });
                let file_path = exe_path.map(|p| Arc::<str>::from(p.to_string_lossy().as_ref()));
                let icon = pkg_meta.as_ref().and_then(|m| m.icon.clone()).or_else(|| {
                    let path_str = file_path.as_deref()?;
                    self.lookup_file_meta(path_str).icon
                });

                let impact = if enabled {
                    StartupImpact::NotMeasured
                } else {
                    StartupImpact::None
                };

                out.push(StartupEntry {
                    id: Arc::from(format!("uwp:{pkg}|{task}")),
                    name,
                    publisher,
                    command: Some(Arc::from(format!("{pkg}!{task}"))),
                    file_path,
                    icon,
                    enabled,
                    impact,
                });
            }
        }
    }
}

impl StartupManager for WindowsStartupManager {
    fn list_startup_entries(&self) -> Result<(Option<u64>, Vec<StartupEntry>), PalError> {
        let last_bios_time_ms = read_last_bios_time_ms();
        let mut entries = Vec::with_capacity(32);

        // 1. Registry Run keys (HKCU, HKLM 64-bit, HKLM WOW6432Node).
        self.collect_run_key(
            HKEY_CURRENT_USER,
            RUN_SUBKEY,
            APPROVED_RUN_SUBKEY,
            0,
            "hkcu-run",
            &mut entries,
        );
        self.collect_run_key(
            HKEY_LOCAL_MACHINE,
            RUN_SUBKEY,
            APPROVED_RUN_SUBKEY,
            KEY_WOW64_64KEY,
            "hklm-run",
            &mut entries,
        );
        self.collect_run_key(
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
            self.collect_startup_folder(
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
            self.collect_startup_folder(
                &common_startup,
                HKEY_LOCAL_MACHINE,
                KEY_WOW64_64KEY,
                "common-folder",
                &mut entries,
            );
        }

        // 3. UWP StartupTask entries.
        self.collect_uwp_startup_tasks(&mut entries);

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

/// Chooses the clearest user-facing display name for a Win32 registry `Run` startup entry.
///
/// Prefers PE `FileDescription`, then `ProductName`, then executable filename, and finally
/// the raw registry value name when the target executable is unavailable.
fn pick_win32_run_display_name(
    val_name: &str,
    file_path: Option<&str>,
    meta: Option<&FileStartupMeta>,
) -> String {
    if let Some(m) = meta {
        if let Some(desc) = m
            .file_description
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return desc.to_owned();
        }
        if let Some(prod) = m
            .product_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return prod.to_owned();
        }
    }

    if let Some(file_name) = file_path
        .and_then(|p| Path::new(p).file_name())
        .and_then(|n| n.to_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return file_name.to_owned();
    }

    val_name.trim().to_owned()
}

/// Resolves the installed `(PackageFullName, InstallLocation)` for a UWP/MSIX `PackageFamilyName`
/// via Win32 `GetPackagesByPackageFamily` + `GetPackagePathByFullName`.
fn find_installed_package(package_family_name: &str) -> Option<(String, PathBuf)> {
    let wide_family = to_wide_null(package_family_name);
    let mut count: u32 = 0;
    let mut buf_len: u32 = 0;

    // First call discovers package count and UTF-16 buffer length.
    // SAFETY: Null output pointers are documented for size discovery.
    let rc = unsafe {
        GetPackagesByPackageFamily(
            wide_family.as_ptr(),
            &mut count,
            ptr::null_mut(),
            &mut buf_len,
            ptr::null_mut(),
        )
    };
    if rc != ERROR_INSUFFICIENT_BUFFER
        || count == 0
        || buf_len == 0
        || count > 64
        || buf_len > 65_536
    {
        return None;
    }

    let mut ptrs: Vec<*mut u16> = vec![ptr::null_mut(); count as usize];
    let mut buf: Vec<u16> = vec![0u16; buf_len as usize];

    // SAFETY: `ptrs` and `buf` are sized according to the preceding query.
    let rc2 = unsafe {
        GetPackagesByPackageFamily(
            wide_family.as_ptr(),
            &mut count,
            ptrs.as_mut_ptr(),
            &mut buf_len,
            buf.as_mut_ptr(),
        )
    };
    if rc2 != ERROR_SUCCESS || count == 0 {
        return None;
    }

    let buf_start = buf.as_ptr() as usize;
    let buf_end = buf_start.saturating_add(buf.len() * std::mem::size_of::<u16>());

    for &raw_ptr in ptrs.iter().take(count as usize) {
        if raw_ptr.is_null() {
            continue;
        }
        let addr = raw_ptr as usize;
        if addr < buf_start || addr >= buf_end {
            continue;
        }
        let offset = (addr - buf_start) / std::mem::size_of::<u16>();
        let tail = &buf[offset..];
        let len = tail.iter().position(|&c| c == 0).unwrap_or(tail.len());
        if len == 0 {
            continue;
        }
        let full_name = String::from_utf16_lossy(&tail[..len]);
        if let Some(install_dir) = query_package_path_by_full_name(&full_name) {
            return Some((full_name, install_dir));
        }
    }

    None
}

/// Resolves the installation directory of an installed package full name via
/// `GetPackagePathByFullName`.
fn query_package_path_by_full_name(package_full_name: &str) -> Option<PathBuf> {
    let wide_full = to_wide_null(package_full_name);
    let mut path_len: u32 = 0;

    // SAFETY: Null path buffer queries the required UTF-16 length.
    let rc =
        unsafe { GetPackagePathByFullName(wide_full.as_ptr(), &mut path_len, ptr::null_mut()) };
    if rc != ERROR_INSUFFICIENT_BUFFER || path_len == 0 || path_len > 32_768 {
        return None;
    }

    let mut path_buf = vec![0u16; path_len as usize];
    // SAFETY: `path_buf` has exact length `path_len` returned above.
    let rc2 = unsafe {
        GetPackagePathByFullName(wide_full.as_ptr(), &mut path_len, path_buf.as_mut_ptr())
    };
    if rc2 != ERROR_SUCCESS {
        return None;
    }

    let nul = path_buf
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(path_buf.len());
    let path_str = String::from_utf16_lossy(&path_buf[..nul]);
    let trimmed = path_str.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

/// Resolves an Appx display string, expanding `ms-resource:` PRI references via
/// `SHLoadIndirectString`.
fn resolve_appx_display_string(
    package_full_name: &str,
    package_family_name: &str,
    raw: &str,
) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_ascii_lowercase();
    if !lower.starts_with("ms-resource:") {
        return Some(trimmed.to_owned());
    }

    let direct = format!("@{{{package_full_name}?{trimmed}}}");
    if let Some(resolved) = sh_load_indirect_string(&direct) {
        return Some(resolved);
    }

    if !lower.starts_with("ms-resource://") {
        let key = trimmed["ms-resource:".len()..].trim_start_matches('/');
        if !key.is_empty() {
            let pkg_name = format_uwp_package_name(package_family_name);
            let with_resources =
                format!("@{{{package_full_name}?ms-resource://{pkg_name}/Resources/{key}}}");
            if let Some(resolved) = sh_load_indirect_string(&with_resources) {
                return Some(resolved);
            }
            let without_resources =
                format!("@{{{package_full_name}?ms-resource://{pkg_name}/{key}}}");
            if let Some(resolved) = sh_load_indirect_string(&without_resources) {
                return Some(resolved);
            }
        }
    }

    None
}

/// Evaluates a Windows Shell indirect string (`@{PackageFullName?ms-resource:...}`).
fn sh_load_indirect_string(source: &str) -> Option<String> {
    let wide_src = to_wide_null(source);
    let mut out_buf = [0u16; 512];
    // SAFETY: `wide_src` is null-terminated UTF-16 and `out_buf` has capacity 512 code units.
    let hr = unsafe {
        SHLoadIndirectString(
            wide_src.as_ptr(),
            out_buf.as_mut_ptr(),
            out_buf.len() as u32,
            ptr::null_mut(),
        )
    };
    if hr != S_OK {
        return None;
    }
    let nul = out_buf
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(out_buf.len());
    let text = String::from_utf16_lossy(&out_buf[..nul]);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// Joins an `AppxManifest.xml` relative path onto `install_dir`, normalizing `/` to `\`.
fn resolve_package_relative_path(install_dir: &Path, relative: &str) -> PathBuf {
    let normalized = relative
        .trim()
        .replace('/', "\\")
        .trim_start_matches('\\')
        .to_owned();
    install_dir.join(normalized)
}

/// Locates the highest-quality existing PNG logo file matching an `AppxManifest.xml`
/// MRT-qualified asset path (e.g., `Assets\Square44x44Logo.png` ->
/// `Assets\Square44x44Logo.targetsize-16_altform-unplated.png` at 100% DPI or
/// `targetsize-24_altform-unplated.png` at 150% DPI).
fn find_best_uwp_logo_file(
    install_dir: &Path,
    relative_logo: &str,
    target_dim: u16,
) -> Option<PathBuf> {
    let base = resolve_package_relative_path(install_dir, relative_logo);
    let parent = base.parent()?;
    let stem = base.file_stem()?.to_str()?;
    let stem_lower = stem.to_ascii_lowercase();
    let dot_prefix = format!("{stem_lower}.");

    let mut best: Option<(i32, PathBuf)> = None;

    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            if !ext.eq_ignore_ascii_case("png") {
                continue;
            }
            let Some(cand_stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let cand_lower = cand_stem.to_ascii_lowercase();
            let qualifiers = if cand_lower == stem_lower {
                ""
            } else if let Some(rest) = cand_lower.strip_prefix(&dot_prefix) {
                rest
            } else {
                continue;
            };

            let score = score_uwp_logo_qualifiers(qualifiers, target_dim);
            if best
                .as_ref()
                .is_none_or(|(best_score, _)| score > *best_score)
            {
                best = Some((score, path));
            }
        }
    }

    best.map(|(_, path)| path)
        .or_else(|| base.is_file().then_some(base))
}

/// Extracts a numeric `targetsize-<N>` value from a lowercased MRT qualifier string.
fn parse_targetsize_qualifier(qualifiers: &str) -> Option<u16> {
    let idx = qualifiers.find("targetsize-")?;
    let rest = &qualifiers[idx + "targetsize-".len()..];
    let digits_end = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits_end == 0 {
        return None;
    }
    rest[..digits_end].parse::<u16>().ok()
}

/// Ranks MRT filename qualifiers (after `<stem>.`) for a crisp `target_dim x target_dim` icon.
fn score_uwp_logo_qualifiers(qualifiers: &str, target_dim: u16) -> i32 {
    if qualifiers.is_empty() {
        return 70;
    }

    let mut score = 0i32;

    if qualifiers.contains("contrast-") || qualifiers.contains("contrast_") {
        score -= 1000;
    }
    if qualifiers.contains("backup") {
        score -= 500;
    }
    if qualifiers.contains("altform-unplated") {
        score += 300;
    } else if qualifiers.contains("altform-lightunplated") {
        score += 220;
    } else if qualifiers.contains("altform-colorful") {
        score += 180;
    }

    if let Some(ts) = parse_targetsize_qualifier(qualifiers) {
        if ts == target_dim {
            // Exact 1:1 physical pixel match — zero resampling!
            score += 220;
        } else if ts > target_dim {
            // Prefer the closest larger size so downsampling retains crisp detail.
            let diff = i32::from(ts - target_dim);
            score += (160 - diff.min(120)).max(40);
        } else {
            // Smaller than target would require upscaling; strongly disfavor.
            score -= 60;
        }
    }

    if qualifiers.contains("scale-100") || qualifiers.contains("scale-125") {
        score += 90;
    } else if qualifiers.contains("scale-150") || qualifiers.contains("scale-200") {
        score += 80;
    } else if qualifiers.contains("scale-400") {
        score += 50;
    }

    score
}

/// Decodes a local PNG logo file and returns a crisp `target_dim x target_dim` RGBA8 [`ExeIconRgba`],
/// using zero-resample pass-through on exact match and premultiplied Lanczos3 otherwise.
fn load_png_icon_rgba(path: &Path, target_dim: u16) -> Option<ExeIconRgba> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() == 0 || meta.len() > MAX_LOGO_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).ok()?;
    let rgba_img = decoded.to_rgba8();
    let src_w = rgba_img.width() as usize;
    let src_h = rgba_img.height() as usize;
    if src_w == 0 || src_h == 0 || src_w > 2048 || src_h > 2048 {
        return None;
    }

    let raw = rgba_img.into_raw();
    let dst_dim = usize::from(target_dim);
    let rgba = if src_w == dst_dim && src_h == dst_dim {
        raw
    } else {
        resample_rgba_premultiplied(&raw, src_w, src_h, dst_dim, dst_dim)
    };

    Some(ExeIconRgba {
        width: target_dim,
        height: target_dim,
        rgba: Arc::from(rgba),
    })
}

/// Lightweight, bounded XML extractor for `AppxManifest.xml` properties, visual elements,
/// and `StartupTask` declarations.
fn parse_appx_manifest(xml: &str) -> ParsedAppxManifest {
    let xml = xml.strip_prefix('\u{feff}').unwrap_or(xml);
    let mut out = ParsedAppxManifest::default();

    let mut pos = 0usize;
    let bytes = xml.as_bytes();
    let mut in_properties = false;
    let mut active_startup_ext_exe: Option<String> = None;

    while pos < bytes.len() {
        let Some(rel_lt) = xml[pos..].find('<') else {
            break;
        };
        let tag_start = pos + rel_lt;

        // Skip XML comments `<!-- ... -->` and processing instructions `<? ... ?>` / `<! ... >`.
        if xml[tag_start..].starts_with("<!--") {
            if let Some(end_comment) = xml[tag_start + 4..].find("-->") {
                pos = tag_start + 4 + end_comment + 3;
                continue;
            }
            break;
        }
        if xml[tag_start..].starts_with("<?") || xml[tag_start..].starts_with("<!") {
            if let Some(end_decl) = xml[tag_start + 2..].find('>') {
                pos = tag_start + 2 + end_decl + 1;
                continue;
            }
            break;
        }

        let Some(tag_end) = find_xml_tag_end(xml, tag_start) else {
            break;
        };
        let tag_body = xml[tag_start + 1..tag_end].trim();
        pos = tag_end + 1;

        if tag_body.is_empty() {
            continue;
        }

        if let Some(closing) = tag_body.strip_prefix('/') {
            let local = xml_local_name(closing.split_whitespace().next().unwrap_or(""));
            if local.eq_ignore_ascii_case("Properties") {
                in_properties = false;
            } else if local.eq_ignore_ascii_case("Extension") {
                active_startup_ext_exe = None;
            }
            continue;
        }

        let self_closing = tag_body.ends_with('/');
        let inner = tag_body.trim_end_matches('/').trim();
        let name_end = inner
            .find(|c: char| c.is_ascii_whitespace())
            .unwrap_or(inner.len());
        let raw_tag_name = &inner[..name_end];
        let attrs_str = inner[name_end..].trim();
        let local = xml_local_name(raw_tag_name);

        if local.eq_ignore_ascii_case("Properties") {
            if !self_closing {
                in_properties = true;
            }
            continue;
        }

        if in_properties
            && !self_closing
            && (local.eq_ignore_ascii_case("DisplayName")
                || local.eq_ignore_ascii_case("PublisherDisplayName")
                || local.eq_ignore_ascii_case("Logo"))
        {
            if let Some((text, next_pos)) = read_xml_element_text(xml, pos, raw_tag_name) {
                pos = next_pos;
                let cleaned = unescape_xml_entities(text.trim());
                if !cleaned.is_empty() {
                    if local.eq_ignore_ascii_case("DisplayName")
                        && out.package_display_name.is_none()
                    {
                        out.package_display_name = Some(cleaned);
                    } else if local.eq_ignore_ascii_case("PublisherDisplayName")
                        && out.publisher_display_name.is_none()
                    {
                        out.publisher_display_name = Some(cleaned);
                    } else if local.eq_ignore_ascii_case("Logo") && out.store_logo.is_none() {
                        out.store_logo = Some(cleaned);
                    }
                }
            }
            continue;
        }

        if local.eq_ignore_ascii_case("Application") {
            if out.default_executable.is_none()
                && let Some(exe) = extract_xml_attr(attrs_str, "Executable")
                && !exe.is_empty()
            {
                out.default_executable = Some(exe);
            }
            continue;
        }

        if local.eq_ignore_ascii_case("VisualElements") {
            if out.app_display_name.is_none()
                && let Some(disp) = extract_xml_attr(attrs_str, "DisplayName")
                && !disp.is_empty()
            {
                out.app_display_name = Some(disp);
            }
            if out.square44_logo.is_none()
                && let Some(logo) = extract_xml_attr(attrs_str, "Square44x44Logo")
                && !logo.is_empty()
            {
                out.square44_logo = Some(logo);
            }
            continue;
        }

        if local.eq_ignore_ascii_case("Extension") {
            let is_startup_ext = extract_xml_attr(attrs_str, "Category")
                .is_some_and(|c| c.eq_ignore_ascii_case("windows.startupTask"));
            if is_startup_ext && !self_closing {
                active_startup_ext_exe =
                    extract_xml_attr(attrs_str, "Executable").filter(|s| !s.is_empty());
            } else if self_closing {
                active_startup_ext_exe = None;
            }
            continue;
        }

        if local.eq_ignore_ascii_case("StartupTask")
            && let Some(task_id) = extract_xml_attr(attrs_str, "TaskId")
            && !task_id.is_empty()
        {
            let display_name = extract_xml_attr(attrs_str, "DisplayName").filter(|s| !s.is_empty());
            let executable = extract_xml_attr(attrs_str, "Executable")
                .filter(|s| !s.is_empty())
                .or_else(|| active_startup_ext_exe.clone());
            out.startup_tasks.push(ParsedStartupTask {
                task_id,
                display_name,
                executable,
            });
        }
    }

    out
}

fn find_xml_tag_end(xml: &str, start: usize) -> Option<usize> {
    let mut quote: Option<u8> = None;
    for (rel, &b) in xml.as_bytes()[start + 1..].iter().enumerate() {
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'"' || b == b'\'' => quote = Some(b),
            None if b == b'>' => return Some(start + 1 + rel),
            None => {}
        }
    }
    None
}

fn xml_local_name(name: &str) -> &str {
    name.rsplit_once(':')
        .map(|(_, local)| local)
        .unwrap_or(name)
}

fn read_xml_element_text<'a>(
    xml: &'a str,
    content_start: usize,
    raw_tag_name: &str,
) -> Option<(&'a str, usize)> {
    let close_tag = format!("</{raw_tag_name}>");
    if let Some(rel) = xml[content_start..].find(&close_tag) {
        let end = content_start + rel;
        return Some((&xml[content_start..end], end + close_tag.len()));
    }
    // Fallback for whitespace before `>` in closing tag or case difference.
    let close_prefix = format!("</{raw_tag_name}");
    if let Some(rel) = xml[content_start..].find(&close_prefix) {
        let end = content_start + rel;
        if let Some(gt) = xml[end..].find('>') {
            return Some((&xml[content_start..end], end + gt + 1));
        }
    }
    None
}

fn extract_xml_attr(attrs: &str, attr_name: &str) -> Option<String> {
    let bytes = attrs.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }

        let name_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' {
            i += 1;
        }
        let raw_name = &attrs[name_start..i];

        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }
        i += 1; // skip '='
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }

        let quote = bytes[i];
        if quote != b'"' && quote != b'\'' {
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            continue;
        }
        i += 1;
        let val_start = i;
        while i < bytes.len() && bytes[i] != quote {
            i += 1;
        }
        let raw_val = &attrs[val_start..i];
        if i < bytes.len() {
            i += 1; // skip closing quote
        }

        if xml_local_name(raw_name).eq_ignore_ascii_case(attr_name) {
            return Some(unescape_xml_entities(raw_val.trim()));
        }
    }

    None
}

fn unescape_xml_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_owned();
    }

    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after_amp = &rest[amp + 1..];
        if let Some(semi) = after_amp.find(';') {
            let entity = &after_amp[..semi];
            let decoded_char = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                    u32::from_str_radix(&entity[2..], 16)
                        .ok()
                        .and_then(char::from_u32)
                }
                _ if entity.starts_with('#') => {
                    entity[1..].parse::<u32>().ok().and_then(char::from_u32)
                }
                _ => None,
            };
            if let Some(ch) = decoded_char {
                out.push(ch);
            } else {
                out.push('&');
                out.push_str(entity);
                out.push(';');
            }
            rest = &after_amp[semi + 1..];
        } else {
            out.push('&');
            out.push_str(after_amp);
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// Reads firmware POST time (`FwPOSTTime` in milliseconds) from the Windows Power registry key.
fn read_last_bios_time_ms() -> Option<u64> {
    let key = open_reg_key(HKEY_LOCAL_MACHINE, POWER_SESSION_SUBKEY, KEY_READ)?;
    read_reg_dword(key.0, "FwPOSTTime").map(u64::from)
}

fn open_reg_key(root: HKEY, subkey: &str, sam_desired: u32) -> Option<RegKeyGuard> {
    let wide_sub = to_wide_null(subkey);
    let mut hkey: HKEY = ptr::null_mut();
    // SAFETY: `wide_sub` is null-terminated UTF-16 and `hkey` receives the opened handle.
    let status = unsafe { RegOpenKeyExW(root, wide_sub.as_ptr(), 0, sam_desired, &mut hkey) };
    if status == ERROR_SUCCESS && !hkey.is_null() {
        Some(RegKeyGuard(hkey))
    } else {
        None
    }
}

fn read_reg_dword(hkey: HKEY, value_name: &str) -> Option<u32> {
    let wide_name = to_wide_null(value_name);
    let mut reg_type: u32 = 0;
    let mut value: u32 = 0;
    let mut byte_len: u32 = std::mem::size_of::<u32>() as u32;
    // SAFETY: `value` is a 4-byte writable buffer matching `byte_len`.
    let status = unsafe {
        RegQueryValueExW(
            hkey,
            wide_name.as_ptr(),
            ptr::null_mut(),
            &mut reg_type,
            (&mut value as *mut u32).cast::<u8>(),
            &mut byte_len,
        )
    };
    if status == ERROR_SUCCESS && reg_type == REG_DWORD && byte_len == 4 {
        Some(value)
    } else {
        None
    }
}

fn write_reg_dword(
    root: HKEY,
    subkey: &str,
    sam_extra: u32,
    value_name: &str,
    value: u32,
) -> Result<(), PalError> {
    let wide_sub = to_wide_null(subkey);
    let mut hkey: HKEY = ptr::null_mut();
    // SAFETY: Opens existing registry key with write access.
    let status = unsafe {
        RegOpenKeyExW(
            root,
            wide_sub.as_ptr(),
            0,
            KEY_SET_VALUE | sam_extra,
            &mut hkey,
        )
    };
    if status != ERROR_SUCCESS || hkey.is_null() {
        return Err(map_win32_error(status, None, "RegOpenKeyExW(State)"));
    }
    let key = RegKeyGuard(hkey);

    let wide_name = to_wide_null(value_name);
    let bytes = value.to_le_bytes();
    // SAFETY: `bytes` is a 4-byte `REG_DWORD` payload.
    let write_status = unsafe {
        RegSetValueExW(
            key.0,
            wide_name.as_ptr(),
            0,
            REG_DWORD,
            bytes.as_ptr(),
            bytes.len() as u32,
        )
    };
    if write_status != ERROR_SUCCESS {
        return Err(map_win32_error(write_status, None, "RegSetValueExW(State)"));
    }
    Ok(())
}

fn write_startup_approved(
    root: HKEY,
    subkey: &str,
    sam_extra: u32,
    value_name: &str,
    enabled: bool,
) -> Result<(), PalError> {
    let wide_sub = to_wide_null(subkey);
    let mut hkey: HKEY = ptr::null_mut();
    let mut disposition: u32 = 0;
    // SAFETY: Creates or opens the `StartupApproved` key with `KEY_SET_VALUE`.
    let status = unsafe {
        RegCreateKeyExW(
            root,
            wide_sub.as_ptr(),
            0,
            ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE | sam_extra,
            ptr::null(),
            &mut hkey,
            &mut disposition,
        )
    };
    if status != ERROR_SUCCESS || hkey.is_null() {
        return Err(map_win32_error(
            status,
            None,
            "RegCreateKeyExW(StartupApproved)",
        ));
    }
    let key = RegKeyGuard(hkey);

    let payload = encode_startup_approved_bytes(enabled, current_windows_filetime());
    let wide_name = to_wide_null(value_name);
    // SAFETY: `payload` is a valid 12-byte `REG_BINARY` buffer.
    let write_status = unsafe {
        RegSetValueExW(
            key.0,
            wide_name.as_ptr(),
            0,
            REG_BINARY,
            payload.as_ptr(),
            payload.len() as u32,
        )
    };
    if write_status != ERROR_SUCCESS {
        return Err(map_win32_error(
            write_status,
            None,
            "RegSetValueExW(StartupApproved)",
        ));
    }
    Ok(())
}

fn read_approved_map(root: HKEY, subkey: &str, sam_extra: u32) -> HashMap<String, bool> {
    let mut map = HashMap::new();
    let Some(key) = open_reg_key(root, subkey, KEY_READ | sam_extra) else {
        return map;
    };

    let mut index: u32 = 0;
    let mut name_buf = vec![0u16; 512];
    let mut data_buf = vec![0u8; 64];

    loop {
        let mut name_len = name_buf.len() as u32;
        let mut reg_type: u32 = 0;
        let mut data_len = data_buf.len() as u32;

        // SAFETY: Both `name_buf` and `data_buf` are valid writable buffers sized by their lengths.
        let status = unsafe {
            RegEnumValueW(
                key.0,
                index,
                name_buf.as_mut_ptr(),
                &mut name_len,
                ptr::null_mut(),
                &mut reg_type,
                data_buf.as_mut_ptr(),
                &mut data_len,
            )
        };
        if status == ERROR_NO_MORE_ITEMS {
            break;
        }
        index += 1;
        if status != ERROR_SUCCESS || reg_type != REG_BINARY {
            continue;
        }

        let name = String::from_utf16_lossy(&name_buf[..(name_len as usize).min(name_buf.len())]);
        let slice = &data_buf[..(data_len as usize).min(data_buf.len())];
        if let Some(enabled) = decode_startup_approved_bytes(slice) {
            map.insert(name.to_ascii_lowercase(), enabled);
        }
    }

    map
}

fn enum_reg_string_values(root: HKEY, subkey: &str, sam_extra: u32) -> Vec<(String, String)> {
    let mut values = Vec::new();
    let Some(key) = open_reg_key(root, subkey, KEY_READ | sam_extra) else {
        return values;
    };

    let mut index: u32 = 0;
    let mut name_buf = vec![0u16; 512];
    let mut data_buf = vec![0u8; 4096];

    loop {
        let mut name_len = name_buf.len() as u32;
        let mut reg_type: u32 = 0;
        let mut data_len = data_buf.len() as u32;

        // SAFETY: `name_buf` and `data_buf` are valid writable buffers.
        let status = unsafe {
            RegEnumValueW(
                key.0,
                index,
                name_buf.as_mut_ptr(),
                &mut name_len,
                ptr::null_mut(),
                &mut reg_type,
                data_buf.as_mut_ptr(),
                &mut data_len,
            )
        };
        if status == ERROR_NO_MORE_ITEMS {
            break;
        }
        index += 1;
        if status != ERROR_SUCCESS || (reg_type != REG_SZ && reg_type != REG_EXPAND_SZ) {
            continue;
        }

        let name = String::from_utf16_lossy(&name_buf[..(name_len as usize).min(name_buf.len())]);
        let byte_count = (data_len as usize).min(data_buf.len());
        let u16_count = byte_count / 2;
        let mut utf16 = Vec::with_capacity(u16_count);
        for chunk in data_buf[..u16_count * 2].as_chunks::<2>().0 {
            let code = u16::from_le_bytes(*chunk);
            if code == 0 {
                break;
            }
            utf16.push(code);
        }
        let val = String::from_utf16_lossy(&utf16);
        values.push((name, val));
    }

    values
}

fn enum_reg_subkeys(hkey: HKEY) -> Vec<String> {
    let mut subkeys = Vec::new();
    let mut index: u32 = 0;
    let mut name_buf = vec![0u16; 512];

    loop {
        let mut name_len = name_buf.len() as u32;
        // SAFETY: `name_buf` is writable for `name_len` UTF-16 code units.
        let status = unsafe {
            RegEnumKeyExW(
                hkey,
                index,
                name_buf.as_mut_ptr(),
                &mut name_len,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if status == ERROR_NO_MORE_ITEMS {
            break;
        }
        index += 1;
        if status != ERROR_SUCCESS {
            continue;
        }
        let name = String::from_utf16_lossy(&name_buf[..(name_len as usize).min(name_buf.len())]);
        if !name.is_empty() {
            subkeys.push(name);
        }
    }

    subkeys
}

/// Decodes a Windows `StartupApproved` `REG_BINARY` payload into `Some(enabled)`.
///
/// Windows encodes enabled states with an even leading byte (typically `0x02` or `0x06`)
/// and disabled states with an odd leading byte (typically `0x03` or `0x07`).
#[inline]
pub(crate) fn decode_startup_approved_bytes(bytes: &[u8]) -> Option<bool> {
    let first = *bytes.first()?;
    Some(first % 2 == 0)
}

/// Encodes a 12-byte Windows `StartupApproved` `REG_BINARY` payload.
///
/// - Enabled: `[0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]`
/// - Disabled: `[0x03, 0, 0, 0]` followed by little-endian Windows FILETIME (`filetime_100ns`).
#[inline]
pub(crate) fn encode_startup_approved_bytes(enabled: bool, filetime_100ns: u64) -> [u8; 12] {
    let mut out = [0u8; 12];
    if enabled {
        out[0] = 0x02;
    } else {
        out[0] = 0x03;
        out[4..12].copy_from_slice(&filetime_100ns.to_le_bytes());
    }
    out
}

/// Parses a structured [`StartupEntry::id`] back into its registry/folder/UWP target.
pub(crate) fn parse_startup_id(id: &str) -> Option<StartupIdTarget<'_>> {
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

/// Extracts the executable path from a Windows startup command line, expanding `%VAR%`
/// environment references cleanly without blocking shell or network calls.
pub(crate) fn extract_executable_path(command: &str) -> Option<String> {
    extract_executable_path_with_env(command, |var| std::env::var(var).ok())
}

fn extract_executable_path_with_env<F>(command: &str, lookup_env: F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return None;
    }

    let raw_exe = if let Some(after_quote) = trimmed.strip_prefix('"') {
        if let Some(close_idx) = after_quote.find('"') {
            &after_quote[..close_idx]
        } else {
            after_quote
        }
    } else if let Some(end_idx) = find_unquoted_executable_end(trimmed) {
        &trimmed[..end_idx]
    } else {
        match trimmed.find(char::is_whitespace) {
            Some(idx) => &trimmed[..idx],
            None => trimmed,
        }
    };

    let raw_exe = raw_exe.trim();
    if raw_exe.is_empty() {
        return None;
    }

    let expanded = expand_percent_env_vars(raw_exe, &lookup_env);
    let expanded = expanded.trim();
    if expanded.is_empty() {
        None
    } else {
        Some(expanded.to_owned())
    }
}

fn find_unquoted_executable_end(cmd: &str) -> Option<usize> {
    const EXTS: [&str; 7] = [".exe", ".com", ".bat", ".cmd", ".msc", ".vbs", ".ps1"];
    let lower = cmd.to_ascii_lowercase();
    let bytes = lower.as_bytes();

    for ext in EXTS {
        let mut search_from = 0usize;
        while let Some(rel) = lower[search_from..].find(ext) {
            let end = search_from + rel + ext.len();
            let boundary_ok = end == bytes.len()
                || bytes[end].is_ascii_whitespace()
                || bytes[end] == b','
                || bytes[end] == b';';
            if boundary_ok {
                return Some(end);
            }
            search_from = end;
        }
    }
    None
}

fn expand_percent_env_vars<F>(input: &str, lookup_env: &F) -> String
where
    F: Fn(&str) -> Option<String>,
{
    if !input.contains('%') {
        return input.to_owned();
    }

    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after_first = &rest[start + 1..];
        if let Some(end) = after_first.find('%') {
            let var_name = &after_first[..end];
            if !var_name.is_empty()
                && let Some(val) = lookup_env(var_name)
            {
                out.push_str(&val);
            } else {
                out.push('%');
                out.push_str(var_name);
                out.push('%');
            }
            rest = &after_first[end + 1..];
        } else {
            out.push('%');
            out.push_str(after_first);
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// Strips the trailing `_<publisher_hash>` from a UWP `PackageFamilyName` for display.
fn format_uwp_package_name(package_family_name: &str) -> String {
    package_family_name
        .rsplit_once('_')
        .map(|(base, _)| base)
        .filter(|base| !base.is_empty())
        .unwrap_or(package_family_name)
        .to_owned()
}

/// Offline bounded MS-SHLLINK (`.lnk`) local base path extractor.
///
/// Reads only the local `LinkInfo` section directly from bytes (never invokes
/// `IShellLinkW::Resolve`, preventing Windows Installer or network share hangs).
fn parse_lnk_local_target(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() < 76 || meta.len() > MAX_LNK_INSPECT_BYTES {
        return None;
    }
    let data = std::fs::read(path).ok()?;
    parse_lnk_local_target_bytes(&data)
}

fn parse_lnk_local_target_bytes(data: &[u8]) -> Option<String> {
    // ShellLinkHeader is 76 bytes; HeaderSize == 0x4C.
    if data.len() < 76 {
        return None;
    }
    let header_size = u32::from_le_bytes(data[0..4].try_into().ok()?);
    if header_size != 0x4C {
        return None;
    }
    let flags = u32::from_le_bytes(data[20..24].try_into().ok()?);
    const HAS_LINK_TARGET_ID_LIST: u32 = 0x01;
    const HAS_LINK_INFO: u32 = 0x02;

    if (flags & HAS_LINK_INFO) == 0 {
        return None;
    }

    let mut offset = 76usize;
    if (flags & HAS_LINK_TARGET_ID_LIST) != 0 {
        if offset + 2 > data.len() {
            return None;
        }
        let id_list_size = u16::from_le_bytes(data[offset..offset + 2].try_into().ok()?) as usize;
        offset = offset.checked_add(2)?.checked_add(id_list_size)?;
    }

    // LinkInfo header is at least 28 bytes.
    if offset + 28 > data.len() {
        return None;
    }
    let link_info_size = u32::from_le_bytes(data[offset..offset + 4].try_into().ok()?) as usize;
    let link_info_hdr_size =
        u32::from_le_bytes(data[offset + 4..offset + 8].try_into().ok()?) as usize;
    let link_info_flags = u32::from_le_bytes(data[offset + 8..offset + 12].try_into().ok()?);

    if link_info_size < 28 || offset + link_info_size > data.len() {
        return None;
    }
    const VOLUME_ID_AND_LOCAL_BASE_PATH: u32 = 0x01;
    if (link_info_flags & VOLUME_ID_AND_LOCAL_BASE_PATH) == 0 {
        return None;
    }

    let info = &data[offset..offset + link_info_size];

    // Prefer UTF-16LE LocalBasePathUnicode when LinkInfoHeaderSize >= 0x24.
    if link_info_hdr_size >= 0x24 && info.len() >= 36 {
        let local_uni_off = u32::from_le_bytes(info[28..32].try_into().ok()?) as usize;
        let suffix_uni_off = u32::from_le_bytes(info[32..36].try_into().ok()?) as usize;
        if local_uni_off > 0 && local_uni_off < info.len() {
            let base = read_utf16le_z(&info[local_uni_off..]);
            let suffix = if suffix_uni_off > 0 && suffix_uni_off < info.len() {
                read_utf16le_z(&info[suffix_uni_off..])
            } else {
                String::new()
            };
            let full = join_lnk_base_and_suffix(&base, &suffix);
            if !full.is_empty() && !is_unc_path(&full) {
                return Some(full);
            }
        }
    }

    // Fallback to ANSI LocalBasePath + CommonPathSuffix.
    let local_off = u32::from_le_bytes(info[16..20].try_into().ok()?) as usize;
    let suffix_off = u32::from_le_bytes(info[24..28].try_into().ok()?) as usize;
    if local_off == 0 || local_off >= info.len() {
        return None;
    }
    let base = read_ascii_z(&info[local_off..]);
    let suffix = if suffix_off > 0 && suffix_off < info.len() {
        read_ascii_z(&info[suffix_off..])
    } else {
        String::new()
    };
    let full = join_lnk_base_and_suffix(&base, &suffix);
    if full.is_empty() || is_unc_path(&full) {
        None
    } else {
        Some(full)
    }
}

fn read_utf16le_z(slice: &[u8]) -> String {
    let mut units = Vec::new();
    for pair in slice.as_chunks::<2>().0 {
        let ch = u16::from_le_bytes(*pair);
        if ch == 0 {
            break;
        }
        units.push(ch);
    }
    String::from_utf16_lossy(&units)
}

fn read_ascii_z(slice: &[u8]) -> String {
    let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    String::from_utf8_lossy(&slice[..end]).into_owned()
}

fn join_lnk_base_and_suffix(base: &str, suffix: &str) -> String {
    let base = base.trim();
    let suffix = suffix.trim();
    if suffix.is_empty() {
        base.to_owned()
    } else if base.ends_with('\\') || suffix.starts_with('\\') {
        format!("{base}{suffix}")
    } else {
        format!(r"{base}\{suffix}")
    }
}

fn current_windows_filetime() -> u64 {
    let unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    unix_secs
        .saturating_add(WINDOWS_TO_UNIX_EPOCH_SECS)
        .saturating_mul(10_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_approved_binary_decode_and_encode_round_trip() {
        assert_eq!(decode_startup_approved_bytes(&[]), None);
        assert_eq!(
            decode_startup_approved_bytes(&[0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            Some(true)
        );
        assert_eq!(
            decode_startup_approved_bytes(&[0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            Some(true)
        );
        assert_eq!(
            decode_startup_approved_bytes(&[0x03, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]),
            Some(false)
        );
        assert_eq!(
            decode_startup_approved_bytes(&[0x07, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]),
            Some(false)
        );

        let enabled_bytes = encode_startup_approved_bytes(true, 0x1122_3344_5566_7788);
        assert_eq!(enabled_bytes[0], 0x02);
        assert_eq!(&enabled_bytes[4..12], &[0u8; 8]);
        assert_eq!(decode_startup_approved_bytes(&enabled_bytes), Some(true));

        let ft = 133_700_000_000_000_000u64;
        let disabled_bytes = encode_startup_approved_bytes(false, ft);
        assert_eq!(disabled_bytes[0], 0x03);
        assert_eq!(
            u64::from_le_bytes(disabled_bytes[4..12].try_into().unwrap()),
            ft
        );
        assert_eq!(decode_startup_approved_bytes(&disabled_bytes), Some(false));
    }

    #[test]
    fn extract_executable_path_handles_quotes_spaces_and_env_vars() {
        let env = |var: &str| match var {
            "ProgramFiles" => Some(r"C:\Program Files".to_owned()),
            "LOCALAPPDATA" => Some(r"C:\Users\Alice\AppData\Local".to_owned()),
            _ => None,
        };

        assert_eq!(
            extract_executable_path_with_env(
                r#""C:\Program Files\My App\launcher.exe" --silent -bg"#,
                env
            )
            .as_deref(),
            Some(r"C:\Program Files\My App\launcher.exe")
        );
        assert_eq!(
            extract_executable_path_with_env(
                r"C:\Program Files\Vendor Suite\agent.exe /autorun",
                env
            )
            .as_deref(),
            Some(r"C:\Program Files\Vendor Suite\agent.exe")
        );
        assert_eq!(
            extract_executable_path_with_env(r#""%ProgramFiles%\Foo\bar.exe" -m"#, env).as_deref(),
            Some(r"C:\Program Files\Foo\bar.exe")
        );
        assert_eq!(
            extract_executable_path_with_env(r"%LOCALAPPDATA%\Loom\loom.exe --tray", env)
                .as_deref(),
            Some(r"C:\Users\Alice\AppData\Local\Loom\loom.exe")
        );
        assert_eq!(
            extract_executable_path_with_env("notepad.exe notes.txt", env).as_deref(),
            Some("notepad.exe")
        );
        assert_eq!(extract_executable_path_with_env("   ", env), None);
    }

    #[test]
    fn parse_startup_id_validates_all_sources_and_rejects_malformed() {
        assert_eq!(
            parse_startup_id("hkcu-run:OneDrive"),
            Some(StartupIdTarget::HkcuRun("OneDrive"))
        );
        assert_eq!(
            parse_startup_id("hklm-run:SecurityHealth"),
            Some(StartupIdTarget::HklmRun("SecurityHealth"))
        );
        assert_eq!(
            parse_startup_id("hklm-run32:LegacyTool"),
            Some(StartupIdTarget::HklmRun32("LegacyTool"))
        );
        assert_eq!(
            parse_startup_id("user-folder:MyApp.lnk"),
            Some(StartupIdTarget::UserFolder("MyApp.lnk"))
        );
        assert_eq!(
            parse_startup_id("common-folder:Shared.lnk"),
            Some(StartupIdTarget::CommonFolder("Shared.lnk"))
        );
        assert_eq!(
            parse_startup_id("uwp:Microsoft.Todos_8wekyb3d8bbwe|TodoStartupTask"),
            Some(StartupIdTarget::Uwp {
                package: "Microsoft.Todos_8wekyb3d8bbwe",
                task: "TodoStartupTask",
            })
        );

        assert_eq!(parse_startup_id(""), None);
        assert_eq!(parse_startup_id("hkcu-run:"), None);
        assert_eq!(parse_startup_id("unknown:foo"), None);
        assert_eq!(parse_startup_id("uwp:missing_pipe"), None);
        assert_eq!(parse_startup_id("uwp:|task"), None);
        assert_eq!(parse_startup_id("uwp:pkg|"), None);
    }

    #[test]
    fn parse_appx_manifest_extracts_properties_visual_elements_and_startup_tasks() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10"
         xmlns:uap="http://schemas.microsoft.com/appx/manifest/uap/windows10"
         xmlns:desktop="http://schemas.microsoft.com/appx/manifest/desktop/windows10">
  <!-- <desktop:StartupTask TaskId="IgnoredComment" DisplayName="Nope" /> -->
  <Properties>
    <DisplayName>Watt Toolkit &amp; Tools</DisplayName>
    <PublisherDisplayName>软妹币玩家</PublisherDisplayName>
    <Logo>images\StoreLogo.png</Logo>
  </Properties>
  <Applications>
    <Application Id="App" Executable="bin\Steam++.exe">
      <uap:VisualElements DisplayName="ms-resource:AppName" Square44x44Logo="images\Square44x44Logo.png" />
      <Extensions>
        <desktop:Extension Category="windows.startupTask" Executable="bin\BootHelper.exe">
          <desktop:StartupTask TaskId="BootAutoStartTask" Enabled="true" DisplayName="Steam++ System Boot Run" />
        </desktop:Extension>
      </Extensions>
    </Application>
  </Applications>
</Package>"#;

        let parsed = parse_appx_manifest(xml);
        assert_eq!(
            parsed.package_display_name.as_deref(),
            Some("Watt Toolkit & Tools")
        );
        assert_eq!(parsed.publisher_display_name.as_deref(), Some("软妹币玩家"));
        assert_eq!(parsed.store_logo.as_deref(), Some(r"images\StoreLogo.png"));
        assert_eq!(
            parsed.app_display_name.as_deref(),
            Some("ms-resource:AppName")
        );
        assert_eq!(
            parsed.square44_logo.as_deref(),
            Some(r"images\Square44x44Logo.png")
        );
        assert_eq!(
            parsed.default_executable.as_deref(),
            Some(r"bin\Steam++.exe")
        );
        assert_eq!(
            parsed.startup_tasks,
            vec![ParsedStartupTask {
                task_id: "BootAutoStartTask".to_owned(),
                display_name: Some("Steam++ System Boot Run".to_owned()),
                executable: Some(r"bin\BootHelper.exe".to_owned()),
            }]
        );
    }

    #[test]
    fn win32_run_display_name_prefers_file_description_then_product_then_filename() {
        let with_desc = FileStartupMeta {
            publisher: Some(Arc::from("Google LLC")),
            file_description: Some(Arc::from("Google Chrome")),
            product_name: Some(Arc::from("Google Chrome")),
            icon: None,
        };
        assert_eq!(
            pick_win32_run_display_name(
                "GoogleChromeAutoLaunch_332B101ED56858E",
                Some(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
                Some(&with_desc),
            ),
            "Google Chrome"
        );

        let only_product = FileStartupMeta {
            publisher: Some(Arc::from("Skutta Software GmbH")),
            file_description: None,
            product_name: Some(Arc::from("Wallpaper Engine")),
            icon: None,
        };
        assert_eq!(
            pick_win32_run_display_name(
                "WallpaperEngine",
                Some(r"F:\SteamLibrary\wallpaper32.exe"),
                Some(&only_product),
            ),
            "Wallpaper Engine"
        );

        let empty_meta = FileStartupMeta::default();
        assert_eq!(
            pick_win32_run_display_name(
                "RawKeyName",
                Some(r"C:\Apps\widget.exe"),
                Some(&empty_meta),
            ),
            "widget.exe"
        );

        assert_eq!(
            pick_win32_run_display_name("OrphanKey", None, None),
            "OrphanKey"
        );
    }

    #[test]
    fn uwp_logo_qualifier_scoring_prefers_exact_unplated_icons_over_contrast_and_upscale() {
        let exact_16 = score_uwp_logo_qualifiers("targetsize-16_altform-unplated", 16);
        let larger_24 = score_uwp_logo_qualifiers("targetsize-24_altform-unplated", 16);
        let scale_100 = score_uwp_logo_qualifiers("scale-100", 16);
        let exact = score_uwp_logo_qualifiers("", 16);
        let contrast = score_uwp_logo_qualifiers("targetsize-16_contrast-black", 16);
        assert!(exact_16 > larger_24);
        assert!(larger_24 > scale_100);
        assert!(scale_100 > exact);
        assert!(exact > contrast);

        // At 150% DPI (target_dim = 24), exact 24 beats 16 (which would require upscaling).
        let hidpi_24 = score_uwp_logo_qualifiers("targetsize-24_altform-unplated", 24);
        let hidpi_16 = score_uwp_logo_qualifiers("targetsize-16_altform-unplated", 24);
        assert!(hidpi_24 > hidpi_16);
    }
}
