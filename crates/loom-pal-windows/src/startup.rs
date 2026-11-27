//! Windows startup applications enumeration and `StartupApproved` / UWP toggle manager.
//!
//! Aggregates:
//! 1. Registry `Run` keys (`HKCU`, `HKLM` 64-bit, `HKLM\WOW6432Node`) cross-referenced
//!    with `Explorer\StartupApproved\Run` and `Run32`.
//! 2. Per-user and common Startup folders cross-referenced with `StartupApproved\StartupFolder`.
//! 3. UWP `StartupTask` entries under `AppModel\SystemAppData`.
//!
//! Strictly bypasses UNC and `DRIVE_REMOTE` paths before reading PE version resources and
//! maps Win32 `ERROR_ACCESS_DENIED` to [`PalError::ElevationRequired`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use loom_pal_api::{PalError, StartupManager};
use loom_types::{StartupEntry, StartupImpact};
use parking_lot::RwLock;
use windows_sys::Win32::Foundation::{ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, KEY_WOW64_64KEY,
    REG_BINARY, REG_DWORD, REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOTE;

use crate::handle::{map_win32_error, to_wide_null};
use crate::slow_prober::{extract_drive_letter, is_unc_path, read_pe_version_info};

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

/// Windows implementation of [`StartupManager`].
#[derive(Debug, Default)]
pub struct WindowsStartupManager {
    publisher_cache: RwLock<HashMap<String, Option<Arc<str>>>>,
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

    fn lookup_publisher(&self, file_path: &str) -> Option<Arc<str>> {
        let trimmed = file_path.trim();
        if trimmed.is_empty() || self.is_network_path(trimmed) {
            return None;
        }
        let cache_key = trimmed.to_ascii_lowercase();
        if let Some(cached) = self.publisher_cache.read().get(&cache_key) {
            return cached.clone();
        }

        let publisher = if Path::new(trimmed).is_file() {
            read_pe_version_info(trimmed).0
        } else {
            None
        };

        self.publisher_cache
            .write()
            .insert(cache_key, publisher.clone());
        publisher
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
            let file_path = extract_executable_path(&command);
            let publisher = file_path.as_deref().and_then(|p| self.lookup_publisher(p));
            let impact = if enabled {
                StartupImpact::NotMeasured
            } else {
                StartupImpact::None
            };

            out.push(StartupEntry {
                id: Arc::from(format!("{id_prefix}:{val_name}")),
                name: Arc::from(val_name),
                publisher,
                command: if command.trim().is_empty() {
                    None
                } else {
                    Some(Arc::from(command))
                },
                file_path: file_path.map(Arc::<str>::from),
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

            let file_path = resolved_target.unwrap_or_else(|| raw_path.clone());
            let publisher = self.lookup_publisher(&file_path);
            let impact = if enabled {
                StartupImpact::NotMeasured
            } else {
                StartupImpact::None
            };

            out.push(StartupEntry {
                id: Arc::from(format!("{id_prefix}:{file_name}")),
                name: Arc::from(display_name),
                publisher,
                command: Some(Arc::from(file_path.clone())),
                file_path: Some(Arc::from(file_path)),
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

            let multi = valid_tasks.len() > 1;
            let pkg_display = format_uwp_package_name(&pkg);

            for (task, state) in valid_tasks {
                // UWP StartupTask State: 2 = Enabled, 4 = EnabledByPolicy; 0/1/3 = Disabled*.
                let enabled = matches!(state, 2 | 4);
                let name = if multi {
                    format!("{pkg_display} ({task})")
                } else {
                    pkg_display.clone()
                };
                let impact = if enabled {
                    StartupImpact::NotMeasured
                } else {
                    StartupImpact::None
                };

                out.push(StartupEntry {
                    id: Arc::from(format!("uwp:{pkg}|{task}")),
                    name: Arc::from(name),
                    publisher: None,
                    command: Some(Arc::from(format!("{pkg}!{task}"))),
                    file_path: None,
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
}
