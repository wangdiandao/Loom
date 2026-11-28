//! Local PE version-resource reader (`CompanyName`, `FileDescription`, `ProductName`).
//!
//! Reads strictly local PE resources without performing any online CRL/OCSP checks.

use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;

use windows_sys::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};

use crate::handle::to_wide_null;

/// Local PE version-resource strings extracted without network certificate checks.
#[derive(Debug, Default, Clone)]
pub(crate) struct PeVersionInfo {
    pub(crate) publisher: Option<Arc<str>>,
    pub(crate) file_description: Option<Arc<str>>,
    pub(crate) product_name: Option<Arc<str>>,
}

/// Reads local PE version resources (`CompanyName`, `FileDescription`) without performing
/// any network certificate revocation (CRL/OCSP) checks.
pub(crate) fn read_pe_version_info(exe_path: &str) -> (Option<Arc<str>>, Option<Arc<str>>) {
    let info = read_pe_version_info_full(exe_path);
    (info.publisher, info.file_description)
}

/// Reads local PE version resources (`CompanyName`, `FileDescription`, `ProductName`)
/// without performing any network certificate revocation (CRL/OCSP) checks.
pub(crate) fn read_pe_version_info_full(exe_path: &str) -> PeVersionInfo {
    let wide_path = to_wide_null(exe_path);
    let mut unused_handle: u32 = 0;

    // SAFETY: `wide_path` is null-terminated UTF-16.
    let size = unsafe { GetFileVersionInfoSizeW(wide_path.as_ptr(), &mut unused_handle) };
    if size == 0 || size > 4 * 1024 * 1024 {
        return PeVersionInfo::default();
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
        return PeVersionInfo::default();
    }

    let translations = read_version_translations(&data);

    let mut info = PeVersionInfo::default();

    for &(lang, codepage) in &translations {
        if info.publisher.is_none() {
            info.publisher = query_version_string(&data, lang, codepage, "CompanyName");
        }
        if info.file_description.is_none() {
            info.file_description = query_version_string(&data, lang, codepage, "FileDescription");
        }
        if info.product_name.is_none() {
            info.product_name = query_version_string(&data, lang, codepage, "ProductName");
        }
        if info.publisher.is_some()
            && info.file_description.is_some()
            && info.product_name.is_some()
        {
            break;
        }
    }

    info
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
