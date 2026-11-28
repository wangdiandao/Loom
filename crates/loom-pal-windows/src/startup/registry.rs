//! Registry `Run` / `StartupApproved` / BIOS POST time readers, writers, and command-line
//! executable path extraction.

use std::collections::HashMap;
use std::path::Path;
use std::ptr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use loom_pal_api::PalError;
use loom_types::{StartupEntry, StartupImpact};
use windows_sys::Win32::Foundation::{ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, REG_BINARY, REG_DWORD, REG_EXPAND_SZ,
    REG_OPTION_NON_VOLATILE, REG_SZ, RegCreateKeyExW, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW,
    RegQueryValueExW, RegSetValueExW,
};

use crate::handle::{OwnedRegKey, map_win32_error, to_wide_null};

use super::{FileStartupMeta, WindowsStartupManager};

pub(super) const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub(super) const RUN32_SUBKEY: &str = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Run";
pub(super) const APPROVED_RUN_SUBKEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
pub(super) const APPROVED_RUN32_SUBKEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run32";
pub(super) const APPROVED_FOLDER_SUBKEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\StartupFolder";
pub(super) const UWP_STARTUP_ROOT: &str = r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\SystemAppData";
const POWER_SESSION_SUBKEY: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Power";

/// Difference in seconds between Windows FILETIME epoch (`1601-01-01`) and Unix epoch (`1970-01-01`).
const WINDOWS_TO_UNIX_EPOCH_SECS: u64 = 11_644_473_600;

pub(super) fn collect_run_key(
    mgr: &WindowsStartupManager,
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
            extract_executable_path(&command).map(|p| mgr.resolve_existing_local_path(&p));
        let meta = file_path.as_deref().map(|p| mgr.lookup_file_meta(p));
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

/// Chooses the clearest user-facing display name for a Win32 registry `Run` startup entry.
///
/// Prefers PE `FileDescription`, then `ProductName`, then executable filename, and finally
/// the raw registry value name when the target executable is unavailable.
pub(super) fn pick_win32_run_display_name(
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

/// Reads firmware POST time (`FwPOSTTime` in milliseconds) from the Windows Power registry key.
pub(super) fn read_last_bios_time_ms() -> Option<u64> {
    let key = open_reg_key(HKEY_LOCAL_MACHINE, POWER_SESSION_SUBKEY, KEY_READ)?;
    read_reg_dword(key.as_raw(), "FwPOSTTime").map(u64::from)
}

pub(super) fn open_reg_key(root: HKEY, subkey: &str, sam_desired: u32) -> Option<OwnedRegKey> {
    let wide_sub = to_wide_null(subkey);
    let mut hkey: HKEY = ptr::null_mut();
    // SAFETY: `wide_sub` is null-terminated UTF-16 and `hkey` receives the opened handle.
    let status = unsafe { RegOpenKeyExW(root, wide_sub.as_ptr(), 0, sam_desired, &mut hkey) };
    (status == ERROR_SUCCESS)
        .then(|| OwnedRegKey::from_raw(hkey))
        .flatten()
}

pub(super) fn read_reg_dword(hkey: HKEY, value_name: &str) -> Option<u32> {
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

pub(super) fn write_reg_dword(
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
    let Some(key) = (status == ERROR_SUCCESS)
        .then(|| OwnedRegKey::from_raw(hkey))
        .flatten()
    else {
        return Err(map_win32_error(status, None, "RegOpenKeyExW(State)"));
    };

    let wide_name = to_wide_null(value_name);
    let bytes = value.to_le_bytes();
    // SAFETY: `bytes` is a 4-byte `REG_DWORD` payload.
    let write_status = unsafe {
        RegSetValueExW(
            key.as_raw(),
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

pub(super) fn write_startup_approved(
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
    let Some(key) = (status == ERROR_SUCCESS)
        .then(|| OwnedRegKey::from_raw(hkey))
        .flatten()
    else {
        return Err(map_win32_error(
            status,
            None,
            "RegCreateKeyExW(StartupApproved)",
        ));
    };

    let payload = encode_startup_approved_bytes(enabled, current_windows_filetime());
    let wide_name = to_wide_null(value_name);
    // SAFETY: `payload` is a valid 12-byte `REG_BINARY` buffer.
    let write_status = unsafe {
        RegSetValueExW(
            key.as_raw(),
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

pub(super) fn read_approved_map(root: HKEY, subkey: &str, sam_extra: u32) -> HashMap<String, bool> {
    let mut map = HashMap::new();
    let Some(key) = open_reg_key(root, subkey, KEY_READ | sam_extra) else {
        return map;
    };

    let mut index: u32 = 0;
    let mut name_buf = [0u16; 512];
    let mut data_buf = [0u8; 64];

    loop {
        let mut name_len = name_buf.len() as u32;
        let mut reg_type: u32 = 0;
        let mut data_len = data_buf.len() as u32;

        // SAFETY: Both `name_buf` and `data_buf` are valid writable buffers sized by their lengths.
        let status = unsafe {
            RegEnumValueW(
                key.as_raw(),
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
    let mut name_buf = [0u16; 512];
    let mut data_buf = vec![0u8; 4096];
    let mut utf16_scratch = Vec::with_capacity(256);

    loop {
        let mut name_len = name_buf.len() as u32;
        let mut reg_type: u32 = 0;
        let mut data_len = data_buf.len() as u32;

        // SAFETY: `name_buf` and `data_buf` are valid writable buffers.
        let status = unsafe {
            RegEnumValueW(
                key.as_raw(),
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
        utf16_scratch.clear();
        for chunk in data_buf[..u16_count * 2].as_chunks::<2>().0 {
            let code = u16::from_le_bytes(*chunk);
            if code == 0 {
                break;
            }
            utf16_scratch.push(code);
        }
        let val = String::from_utf16_lossy(&utf16_scratch);
        values.push((name, val));
    }

    values
}

pub(super) fn enum_reg_subkeys(hkey: HKEY) -> Vec<String> {
    let mut subkeys = Vec::new();
    let mut index: u32 = 0;
    let mut name_buf = [0u16; 512];

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
pub(super) fn decode_startup_approved_bytes(bytes: &[u8]) -> Option<bool> {
    let first = *bytes.first()?;
    Some(first % 2 == 0)
}

/// Encodes a 12-byte Windows `StartupApproved` `REG_BINARY` payload.
///
/// - Enabled: `[0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]`
/// - Disabled: `[0x03, 0, 0, 0]` followed by little-endian Windows FILETIME (`filetime_100ns`).
#[inline]
pub(super) fn encode_startup_approved_bytes(enabled: bool, filetime_100ns: u64) -> [u8; 12] {
    let mut out = [0u8; 12];
    if enabled {
        out[0] = 0x02;
    } else {
        out[0] = 0x03;
        out[4..12].copy_from_slice(&filetime_100ns.to_le_bytes());
    }
    out
}

/// Extracts the executable path from a Windows startup command line, expanding `%VAR%`
/// environment references cleanly without blocking shell or network calls.
pub(super) fn extract_executable_path(command: &str) -> Option<String> {
    extract_executable_path_with_env(command, |var| std::env::var(var).ok())
}

pub(super) fn extract_executable_path_with_env<F>(command: &str, lookup_env: F) -> Option<String>
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

fn current_windows_filetime() -> u64 {
    let unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    unix_secs
        .saturating_add(WINDOWS_TO_UNIX_EPOCH_SECS)
        .saturating_mul(10_000_000)
}
