//! UWP / MSIX `StartupTask` discovery, `AppxManifest.xml` parsing, `ms-resource:` expansion,
//! and MRT-qualified PNG logo asset decoding.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;

use loom_types::{ExeIconRgba, StartupEntry, StartupImpact};
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, S_OK};
use windows_sys::Win32::Storage::Packaging::Appx::{
    GetPackagePathByFullName, GetPackagesByPackageFamily,
};
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, KEY_READ};
use windows_sys::Win32::UI::Shell::SHLoadIndirectString;

use crate::handle::{decode_wide_nul_nonempty, to_wide_null};
use crate::slow_prober::{
    ensure_com_initialized, extract_icon_rgba, resample_rgba_premultiplied, target_icon_dim,
};

use super::WindowsStartupManager;
use super::registry::{UWP_STARTUP_ROOT, enum_reg_subkeys, open_reg_key, read_reg_dword};

/// Maximum `AppxManifest.xml` file size read during UWP startup metadata resolution.
const MAX_MANIFEST_BYTES: u64 = 512 * 1024;

/// Maximum PNG logo asset size decoded for a UWP/MSIX startup icon.
const MAX_LOGO_BYTES: u64 = 2 * 1024 * 1024;

/// Cached AppxManifest metadata and icon for an installed UWP/MSIX package family.
#[derive(Debug, Clone)]
pub(super) struct UwpPackageMeta {
    pub(super) package_display_name: Option<Arc<str>>,
    pub(super) app_display_name: Option<Arc<str>>,
    pub(super) publisher: Option<Arc<str>>,
    pub(super) task_display_names: HashMap<String, Arc<str>>,
    pub(super) task_executables: HashMap<String, PathBuf>,
    pub(super) default_executable: Option<PathBuf>,
    pub(super) icon: Option<ExeIconRgba>,
}

/// Raw fields parsed from `AppxManifest.xml` prior to `ms-resource:` expansion.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ParsedAppxManifest {
    pub(super) package_display_name: Option<String>,
    pub(super) publisher_display_name: Option<String>,
    pub(super) store_logo: Option<String>,
    pub(super) app_display_name: Option<String>,
    pub(super) square44_logo: Option<String>,
    pub(super) default_executable: Option<String>,
    pub(super) startup_tasks: Vec<ParsedStartupTask>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ParsedStartupTask {
    pub(super) task_id: String,
    pub(super) display_name: Option<String>,
    pub(super) executable: Option<String>,
}

pub(super) fn collect_uwp_startup_tasks(mgr: &WindowsStartupManager, out: &mut Vec<StartupEntry>) {
    let Some(root_key) = open_reg_key(HKEY_CURRENT_USER, UWP_STARTUP_ROOT, KEY_READ) else {
        return;
    };

    for pkg in enum_reg_subkeys(root_key.as_raw()) {
        let Some(pkg_key) = open_reg_key(root_key.as_raw(), &pkg, KEY_READ) else {
            continue;
        };
        let tasks = enum_reg_subkeys(pkg_key.as_raw());
        let valid_tasks: Vec<(String, u32)> = tasks
            .into_iter()
            .filter_map(|task| {
                let task_key = open_reg_key(pkg_key.as_raw(), &task, KEY_READ)?;
                let state = read_reg_dword(task_key.as_raw(), "State")?;
                Some((task, state))
            })
            .collect();

        if valid_tasks.is_empty() {
            continue;
        }

        let multi = valid_tasks.len() > 1;
        let pkg_meta = mgr.lookup_uwp_package_meta(&pkg);
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
                mgr.lookup_file_meta(path_str).icon
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

pub(super) fn resolve_uwp_package_meta_uncached(
    mgr: &WindowsStartupManager,
    package_family_name: &str,
) -> Option<UwpPackageMeta> {
    let (package_full_name, install_dir) = find_installed_package(package_family_name)?;
    let install_dir_str = install_dir.to_string_lossy();
    if mgr.is_network_path(&install_dir_str) {
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
        if let Some(logo_path) = find_best_uwp_logo_file(&install_dir, candidate_logo, target_dim)
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

    decode_wide_nul_nonempty(&path_buf).map(PathBuf::from)
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
    decode_wide_nul_nonempty(&out_buf)
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
pub(super) fn score_uwp_logo_qualifiers(qualifiers: &str, target_dim: u16) -> i32 {
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
pub(super) fn parse_appx_manifest(xml: &str) -> ParsedAppxManifest {
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

/// Strips the trailing `_<publisher_hash>` from a UWP `PackageFamilyName` for display.
fn format_uwp_package_name(package_family_name: &str) -> String {
    package_family_name
        .rsplit_once('_')
        .map(|(base, _)| base)
        .filter(|base| !base.is_empty())
        .unwrap_or(package_family_name)
        .to_owned()
}
