//! Win32 registry adapter and atomic file helpers for IFEO `taskmgr.exe`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::ptr;

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, GetLastError,
};
use windows_sys::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_ENUMERATE_SUB_KEYS, KEY_QUERY_VALUE, KEY_SET_VALUE,
    REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS, REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW,
    RegDeleteValueW, RegEnumKeyExW, RegFlushKey, RegOpenKeyExW, RegQueryInfoKeyW, RegQueryValueExW,
    RegSetValueExW,
};

use crate::handle::{os_str_to_wide_null, to_wide_null};

use super::state::{Journal, Observation, RegValue, Store};

/// Target IFEO subkey under `HKEY_LOCAL_MACHINE` for Windows Task Manager.
pub const TASKMGR_IFEO_KEY: &str =
    r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\taskmgr.exe";

/// Journal file name stored in `%ProgramData%\Loom`.
pub const JOURNAL_FILE: &str = "taskmgr-recovery.toml";

/// Human-readable emergency recovery guide stored beside the journal.
pub const RECOVERY_TEXT_FILE: &str = "taskmgr-recovery.txt";

const READ_ACCESS: REG_SAM_FLAGS = KEY_QUERY_VALUE | KEY_ENUMERATE_SUB_KEYS;
const WRITE_ACCESS: REG_SAM_FLAGS = KEY_QUERY_VALUE | KEY_SET_VALUE;
const MAX_VALUE_BYTES: u32 = 16 * 1024;

/// RAII wrapper around an opened Win32 registry key handle.
pub struct RegKey(pub HKEY);

impl Drop for RegKey {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is a valid HKEY returned by `RegOpenKeyExW` or `RegCreateKeyExW`.
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }
}

/// Detailed description of conflicting values/subkeys found under `IFEO\taskmgr.exe`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ConflictReport {
    /// Existing `Debugger` value, if any.
    pub debugger: Option<RegValue>,
    /// Whether `UseFilter` is present.
    pub use_filter: bool,
    /// First few subkey names found under the key.
    pub subkeys: Vec<String>,
    /// Total subkey count under the key.
    pub subkey_count: u32,
}

impl ConflictReport {
    /// Formats a compact, user-readable conflict summary omitting Loom's own value.
    pub fn describe(&self, ours: &RegValue) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(ref val) = self.debugger
            && val != ours
        {
            match val.as_text() {
                Some(text) if text.is_empty() => parts.push("Debugger = <empty>".to_owned()),
                Some(text) => {
                    let shown: String = if text.chars().count() > 160 {
                        text.chars().take(160).chain(Some('…')).collect()
                    } else {
                        text
                    };
                    parts.push(format!("Debugger = {shown}"));
                }
                None => parts.push(format!("Debugger (non-string type {})", val.kind)),
            }
        }
        if self.use_filter {
            parts.push("UseFilter".to_owned());
        }
        if self.subkey_count > 0 {
            let names = self.subkeys.join(", ");
            if self.subkey_count as usize > self.subkeys.len() {
                parts.push(format!("subkeys ({}): {names}...", self.subkey_count));
            } else {
                parts.push(format!("subkeys: {names}"));
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("; "))
        }
    }
}

fn open_in(root: HKEY, path: &str, access: REG_SAM_FLAGS) -> Result<Option<RegKey>, String> {
    let wide = to_wide_null(path);
    let mut key: HKEY = ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated UTF-16 and `key` is a valid out-pointer.
    let code = unsafe { RegOpenKeyExW(root, wide.as_ptr(), 0, access, &mut key) };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if code != ERROR_SUCCESS {
        return Err(format_reg_error("RegOpenKeyExW", code));
    }
    Ok(Some(RegKey(key)))
}

fn create_in(root: HKEY, path: &str) -> Result<RegKey, String> {
    let wide = to_wide_null(path);
    let mut key: HKEY = ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated UTF-16 and `key` is a valid out-pointer.
    let code = unsafe {
        RegCreateKeyExW(
            root,
            wide.as_ptr(),
            0,
            ptr::null(),
            REG_OPTION_NON_VOLATILE,
            WRITE_ACCESS,
            ptr::null(),
            &mut key,
            ptr::null_mut(),
        )
    };
    if code != ERROR_SUCCESS {
        return Err(format_reg_error("RegCreateKeyExW", code));
    }
    Ok(RegKey(key))
}

fn format_reg_error(api: &str, code: u32) -> String {
    if code == ERROR_ACCESS_DENIED {
        format!("{api} failed with ERROR_ACCESS_DENIED (os error 5)")
    } else {
        format!("{api} failed (os error {code})")
    }
}

fn read_value(key: &RegKey, name: &str) -> Result<Option<RegValue>, String> {
    let wide = to_wide_null(name);
    let mut kind: REG_VALUE_TYPE = 0;
    let mut size: u32 = 0;
    // SAFETY: Queries type and byte length without writing into a data buffer.
    let code = unsafe {
        RegQueryValueExW(
            key.0,
            wide.as_ptr(),
            ptr::null_mut(),
            &mut kind,
            ptr::null_mut(),
            &mut size,
        )
    };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if code != ERROR_SUCCESS {
        return Err(format_reg_error("RegQueryValueExW", code));
    }
    if size > MAX_VALUE_BYTES {
        return Err("IFEO registry value exceeds maximum expected size (16 KiB)".into());
    }
    let mut bytes = vec![0u8; size as usize];
    // SAFETY: `bytes` has capacity `size` for `RegQueryValueExW`.
    let code = unsafe {
        RegQueryValueExW(
            key.0,
            wide.as_ptr(),
            ptr::null_mut(),
            &mut kind,
            bytes.as_mut_ptr(),
            &mut size,
        )
    };
    if code != ERROR_SUCCESS {
        return Err(format_reg_error("RegQueryValueExW", code));
    }
    bytes.truncate(size as usize);
    Ok(Some(RegValue { kind, bytes }))
}

fn set_debugger(key: &RegKey, value: Option<&RegValue>) -> Result<(), String> {
    let wide = to_wide_null("Debugger");
    let code = match value {
        Some(v) => {
            // SAFETY: `v.bytes` is valid for `v.bytes.len()` bytes.
            unsafe {
                RegSetValueExW(
                    key.0,
                    wide.as_ptr(),
                    0,
                    v.kind,
                    v.bytes.as_ptr(),
                    v.bytes.len() as u32,
                )
            }
        }
        None => {
            // SAFETY: Deletes only the `Debugger` value under `key.0`.
            unsafe { RegDeleteValueW(key.0, wide.as_ptr()) }
        }
    };
    if code != ERROR_SUCCESS && !(value.is_none() && code == ERROR_FILE_NOT_FOUND) {
        let api = if value.is_some() {
            "RegSetValueExW"
        } else {
            "RegDeleteValueW"
        };
        return Err(format_reg_error(api, code));
    }
    // Flush hive metadata so registry state survives sudden power loss.
    // SAFETY: `key.0` is an open registry key handle.
    let flush_code = unsafe { RegFlushKey(key.0) };
    if flush_code != ERROR_SUCCESS {
        return Err(format_reg_error("RegFlushKey", flush_code));
    }
    Ok(())
}

/// Observes `IFEO\taskmgr.exe` under `HKEY_LOCAL_MACHINE`.
pub fn observe_hklm(path: &str) -> Result<Observation, String> {
    observe_in(HKEY_LOCAL_MACHINE, path)
}

pub(crate) fn observe_in(root: HKEY, path: &str) -> Result<Observation, String> {
    let Some(key) = open_in(root, path, READ_ACCESS)? else {
        return Ok(Observation::default());
    };
    let mut name_buf = [0u16; 256];
    let mut name_len = name_buf.len() as u32;
    // SAFETY: Probes whether at least one subkey exists.
    let enum_code = unsafe {
        RegEnumKeyExW(
            key.0,
            0,
            name_buf.as_mut_ptr(),
            &mut name_len,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if enum_code != ERROR_NO_MORE_ITEMS && enum_code != ERROR_SUCCESS {
        return Err(format_reg_error("RegEnumKeyExW", enum_code));
    }
    let has_subkeys = enum_code == ERROR_SUCCESS;
    let use_filter = read_value(&key, "UseFilter")?.is_some();
    let debugger = read_value(&key, "Debugger")?;
    Ok(Observation {
        debugger,
        filtered: use_filter || has_subkeys,
    })
}

/// Inspects `IFEO\taskmgr.exe` under `HKEY_LOCAL_MACHINE` to build a conflict description.
pub fn describe_hklm(path: &str) -> Result<ConflictReport, String> {
    describe_in(HKEY_LOCAL_MACHINE, path)
}

pub(crate) fn describe_in(root: HKEY, path: &str) -> Result<ConflictReport, String> {
    let Some(key) = open_in(root, path, READ_ACCESS)? else {
        return Ok(ConflictReport::default());
    };
    let mut subkey_count: u32 = 0;
    // SAFETY: Queries subkey count only.
    let info_code = unsafe {
        RegQueryInfoKeyW(
            key.0,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut subkey_count,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if info_code != ERROR_SUCCESS {
        return Err(format_reg_error("RegQueryInfoKeyW", info_code));
    }

    let mut subkeys = Vec::new();
    for idx in 0..subkey_count.min(4) {
        let mut name_buf = [0u16; 256];
        let mut name_len = name_buf.len() as u32;
        // SAFETY: Enumerates subkey name `idx`.
        let code = unsafe {
            RegEnumKeyExW(
                key.0,
                idx,
                name_buf.as_mut_ptr(),
                &mut name_len,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if code == ERROR_NO_MORE_ITEMS {
            break;
        }
        if code != ERROR_SUCCESS {
            return Err(format_reg_error("RegEnumKeyExW", code));
        }
        subkeys.push(String::from_utf16_lossy(
            &name_buf[..(name_len as usize).min(name_buf.len())],
        ));
    }

    Ok(ConflictReport {
        debugger: read_value(&key, "Debugger")?,
        use_filter: read_value(&key, "UseFilter")?.is_some(),
        subkeys,
        subkey_count,
    })
}

pub(crate) fn write_in(root: HKEY, path: &str, value: Option<&RegValue>) -> Result<(), String> {
    let key = match open_in(root, path, WRITE_ACCESS)? {
        Some(k) => k,
        None if value.is_none() => return Ok(()),
        None => create_in(root, path)?,
    };
    set_debugger(&key, value)
}

/// Removes the `Debugger` value ONLY if it strictly matches `expected` (never clobbering third-party entries).
pub fn remove_owned_hklm(path: &str, expected: &RegValue) -> Result<(), String> {
    remove_owned_in(HKEY_LOCAL_MACHINE, path, expected)
}

pub(crate) fn remove_owned_in(root: HKEY, path: &str, expected: &RegValue) -> Result<(), String> {
    let Some(read_key) = open_in(root, path, KEY_QUERY_VALUE)? else {
        return Ok(());
    };
    if read_value(&read_key, "Debugger")?.as_ref() != Some(expected) {
        return Ok(());
    }
    let Some(write_key) = open_in(root, path, WRITE_ACCESS)? else {
        return Ok(());
    };
    if read_value(&write_key, "Debugger")?.as_ref() == Some(expected) {
        set_debugger(&write_key, None)?;
    }
    if read_value(&write_key, "Debugger")?.as_ref() == Some(expected) {
        return Err("Debugger value remained present after deletion".into());
    }
    Ok(())
}

/// Atomically writes `bytes` to `dir/filename` via a sibling temporary file + `sync_all` + `MoveFileExW`.
pub fn write_atomic_file(dir: &Path, filename: &str, bytes: &[u8]) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    let tmp_name = format!(
        ".{filename}.{}.{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    let tmp_path = dir.join(tmp_name);
    let target_path = dir.join(filename);

    let write_res = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .map_err(|e| format!("failed to create temp file {}: {e}", tmp_path.display()))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("failed to write temp file {}: {e}", tmp_path.display()))?;
        drop(file);

        let src_w = os_str_to_wide_null(tmp_path.as_os_str());
        let dst_w = os_str_to_wide_null(target_path.as_os_str());
        // SAFETY: Both paths are valid NUL-terminated UTF-16 buffers.
        let ok = unsafe {
            MoveFileExW(
                src_w.as_ptr(),
                dst_w.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok == 0 {
            // SAFETY: Reads Win32 error set by `MoveFileExW`.
            let code = unsafe { GetLastError() };
            return Err(format!("MoveFileExW failed (os error {code})"));
        }
        Ok(())
    })();

    if write_res.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_res
}

/// Live machine store backed by `HKLM\...\IFEO\taskmgr.exe` and `%ProgramData%\Loom`.
pub struct MachineStore {
    /// Protected state directory (`%ProgramData%\Loom`).
    pub dir: PathBuf,
}

impl MachineStore {
    /// Creates a new [`MachineStore`] rooted at `dir`.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Loads the persisted [`Journal`] if present.
    pub fn load_journal(&self) -> Result<Option<Journal>, String> {
        let path = self.dir.join(JOURNAL_FILE);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("failed to read {}: {e}", path.display())),
        };
        if text.len() > 64 * 1024 {
            return Err("taskmgr recovery journal exceeds maximum size".into());
        }
        let journal: Journal =
            toml::from_str(&text).map_err(|e| format!("corrupt taskmgr recovery journal: {e}"))?;
        if journal.version != 1 {
            return Err(format!(
                "unsupported taskmgr journal version {}",
                journal.version
            ));
        }
        Ok(Some(journal))
    }
}

impl Store for MachineStore {
    fn observe(&mut self) -> Result<Observation, String> {
        observe_hklm(TASKMGR_IFEO_KEY)
    }

    fn write_debugger(&mut self, value: Option<&RegValue>) -> Result<(), String> {
        write_in(HKEY_LOCAL_MACHINE, TASKMGR_IFEO_KEY, value)
    }

    fn save(&mut self, journal: &Journal) -> Result<(), String> {
        let text = toml::to_string_pretty(journal)
            .map_err(|e| format!("failed to serialize recovery journal: {e}"))?;
        write_atomic_file(&self.dir, JOURNAL_FILE, text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteTreeW};

    struct EphemeralHkcuKey(String);

    impl EphemeralHkcuKey {
        fn new(suffix: &str) -> Self {
            Self(format!(
                r"Software\LoomTaskmgrTest-{}-{suffix}",
                std::process::id()
            ))
        }
    }

    impl Drop for EphemeralHkcuKey {
        fn drop(&mut self) {
            let wide = to_wide_null(&self.0);
            // SAFETY: Deletes only our ephemeral HKCU test subtree.
            unsafe {
                let _ = RegDeleteTreeW(HKEY_CURRENT_USER, wide.as_ptr());
            }
        }
    }

    #[test]
    fn hkcu_registry_round_trip_and_owned_removal_preserves_third_party() {
        let key = EphemeralHkcuKey::new("roundtrip");
        let ours = RegValue::reg_sz(r#""C:\Loom\loom-app.exe" --task-manager"#);
        let foreign = RegValue::reg_sz(r#""C:\Sysinternals\procexp64.exe""#);

        // Missing key observes clean default.
        assert_eq!(
            observe_in(HKEY_CURRENT_USER, &key.0).unwrap(),
            Observation::default()
        );

        // Write ours, observe, and remove_owned removes it cleanly.
        write_in(HKEY_CURRENT_USER, &key.0, Some(&ours)).unwrap();
        let obs = observe_in(HKEY_CURRENT_USER, &key.0).unwrap();
        assert_eq!(obs.debugger.as_ref(), Some(&ours));
        assert!(!obs.filtered);

        remove_owned_in(HKEY_CURRENT_USER, &key.0, &ours).unwrap();
        assert_eq!(
            observe_in(HKEY_CURRENT_USER, &key.0).unwrap(),
            Observation::default()
        );

        // Foreign value must NEVER be deleted by remove_owned_in(ours).
        write_in(HKEY_CURRENT_USER, &key.0, Some(&foreign)).unwrap();
        remove_owned_in(HKEY_CURRENT_USER, &key.0, &ours).unwrap();
        let obs_after = observe_in(HKEY_CURRENT_USER, &key.0).unwrap();
        assert_eq!(obs_after.debugger.as_ref(), Some(&foreign));

        let report = describe_in(HKEY_CURRENT_USER, &key.0).unwrap();
        let detail = report.describe(&ours).expect("conflict description");
        assert!(detail.contains("procexp64.exe"), "got: {detail}");
    }
}
