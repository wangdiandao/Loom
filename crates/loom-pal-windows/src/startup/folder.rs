//! Startup folder enumeration and offline bounded MS-SHLLINK (`.lnk`) local-path parser.

use std::path::Path;
use std::sync::Arc;

use loom_types::{StartupEntry, StartupImpact};
use windows_sys::Win32::System::Registry::HKEY;

use crate::slow_prober::{ensure_com_initialized, extract_icon_rgba, is_unc_path};

use super::WindowsStartupManager;
use super::registry::{APPROVED_FOLDER_SUBKEY, read_approved_map};

/// Maximum `.lnk` shortcut file size inspected by the offline local-path parser.
const MAX_LNK_INSPECT_BYTES: u64 = 64 * 1024;

pub(super) fn collect_startup_folder(
    mgr: &WindowsStartupManager,
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
            mgr.resolve_existing_local_path(resolved_target.as_deref().unwrap_or(&raw_path));
        let target_meta = mgr.lookup_file_meta(&file_path);
        let icon = target_meta.icon.or_else(|| {
            if !mgr.is_network_path(&raw_path) {
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
