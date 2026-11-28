//! Shell icon extraction (`SHGetFileInfoW` + RAII GDI guards) and premultiplied Lanczos3 resampler.

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr;
use std::sync::Arc;

use loom_types::ExeIconRgba;
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDIBits, GetObjectW, HDC, HGDIOBJ,
};
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows_sys::Win32::UI::Shell::{
    SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_SMALLICON, SHGFI_USEFILEATTRIBUTES,
    SHGetFileInfoW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

use crate::handle::to_wide_null;

use super::target_icon_dim;

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

/// Extracts an executable icon via `SHGetFileInfoW` matched to [`target_icon_dim()`].
///
/// Prefers the native `SHGFI_SMALLICON` bitmap directly with zero resampling whenever its
/// dimensions already match the display's small-icon pixel size, and falls back to
/// downsampling `SHGFI_LARGEICON` when the small icon would otherwise require upscaling.
///
/// When `memory_only` is `true` (for UNC / `DRIVE_REMOTE` paths), passes
/// `SHGFI_USEFILEATTRIBUTES` so Windows resolves the generic extension icon from memory
/// without touching the network share.
pub(crate) fn extract_icon_rgba(exe_path: &str, memory_only: bool) -> Option<ExeIconRgba> {
    let target_dim = target_icon_dim();
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

    let small_icon = fetch_shell_icon(&wide_path, memory_only, true);
    if let Some((src_w, src_h, rgba)) = small_icon {
        let target_usize = usize::from(target_dim);
        if src_w == target_usize && src_h == target_usize {
            return Some(ExeIconRgba {
                width: target_dim,
                height: target_dim,
                rgba: Arc::from(rgba),
            });
        }
        // Avoid upscaling a smaller small-icon bitmap when a crisp large icon is available.
        if (src_w < target_usize || src_h < target_usize)
            && let Some((lg_w, lg_h, lg_rgba)) = fetch_shell_icon(&wide_path, memory_only, false)
            && lg_w >= target_usize
            && lg_h >= target_usize
        {
            let final_rgba = if lg_w == target_usize && lg_h == target_usize {
                lg_rgba
            } else {
                resample_rgba_premultiplied(&lg_rgba, lg_w, lg_h, target_usize, target_usize)
            };
            return Some(ExeIconRgba {
                width: target_dim,
                height: target_dim,
                rgba: Arc::from(final_rgba),
            });
        }

        let final_rgba =
            resample_rgba_premultiplied(&rgba, src_w, src_h, target_usize, target_usize);
        return Some(ExeIconRgba {
            width: target_dim,
            height: target_dim,
            rgba: Arc::from(final_rgba),
        });
    }

    // Fallback to large icon if small icon query failed.
    let (lg_w, lg_h, lg_rgba) = fetch_shell_icon(&wide_path, memory_only, false)?;
    let target_usize = usize::from(target_dim);
    let final_rgba = if lg_w == target_usize && lg_h == target_usize {
        lg_rgba
    } else {
        resample_rgba_premultiplied(&lg_rgba, lg_w, lg_h, target_usize, target_usize)
    };
    Some(ExeIconRgba {
        width: target_dim,
        height: target_dim,
        rgba: Arc::from(final_rgba),
    })
}

fn fetch_shell_icon(
    wide_path: &[u16],
    memory_only: bool,
    small: bool,
) -> Option<(usize, usize, Vec<u8>)> {
    // SAFETY: `SHFILEINFOW` is POD.
    let mut shfi: SHFILEINFOW = unsafe { zeroed() };
    let mut flags = SHGFI_ICON
        | if small {
            SHGFI_SMALLICON
        } else {
            SHGFI_LARGEICON
        };
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
    hicon_to_raw_rgba(icon_guard.0)
}

/// Converts a Win32 `HICON` into tightly packed top-down RGBA8 pixels at its native size,
/// ensuring every GDI bitmap and device context handle is released via RAII guards.
fn hicon_to_raw_rgba(hicon: HICON) -> Option<(usize, usize, Vec<u8>)> {
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

    Some((src_w, src_h, bgra))
}

#[inline]
fn lanczos3_kernel(x: f32) -> f32 {
    let ax = x.abs();
    if ax < 1e-6 {
        return 1.0;
    }
    if ax >= 3.0 {
        return 0.0;
    }
    let pix = std::f32::consts::PI * ax;
    let pix_a = pix / 3.0;
    (pix.sin() / pix) * (pix_a.sin() / pix_a)
}

/// High-quality two-pass separable Lanczos3 resampler operating in premultiplied alpha
/// space so downsampled icons stay sharp at small sizes without dark fringe halos.
pub(crate) fn resample_rgba_premultiplied(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
) -> Vec<u8> {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return Vec::new();
    }
    if src_w == dst_w && src_h == dst_h {
        return src.to_vec();
    }

    // Horizontal pass: premultiply straight RGBA8 -> `[f32; 4]` at `dst_w x src_h`.
    let mut temp = vec![[0.0f32; 4]; dst_w * src_h];
    let x_ratio = src_w as f32 / dst_w as f32;
    let x_scale = x_ratio.max(1.0);
    let x_support = 3.0 * x_scale;
    let mut weights = Vec::with_capacity((x_support.ceil() as usize * 2 + 2).max(16));

    for dx in 0..dst_w {
        let center = (dx as f32 + 0.5) * x_ratio - 0.5;
        let left = (center - x_support).floor().max(0.0) as usize;
        let right = (center + x_support).ceil().min((src_w - 1) as f32) as usize;

        weights.clear();
        let mut weight_sum = 0.0f32;
        for sx in left..=right {
            let w = lanczos3_kernel((sx as f32 - center) / x_scale);
            weights.push(w);
            weight_sum += w;
        }
        let inv_w = if weight_sum.abs() > 1e-6 {
            1.0 / weight_sum
        } else {
            1.0
        };

        for sy in 0..src_h {
            let mut acc = [0.0f32; 4];
            let row_off = sy * src_w;
            for (k, sx) in (left..=right).enumerate() {
                let w = weights[k] * inv_w;
                let idx = (row_off + sx) * 4;
                let a = f32::from(src[idx + 3]) / 255.0;
                acc[0] += f32::from(src[idx]) * a * w;
                acc[1] += f32::from(src[idx + 1]) * a * w;
                acc[2] += f32::from(src[idx + 2]) * a * w;
                acc[3] += a * w;
            }
            temp[sy * dst_w + dx] = acc;
        }
    }

    // Vertical pass: `dst_w x src_h` -> `dst_w x dst_h`, un-premultiplying back to straight RGBA8.
    let mut dst = vec![0u8; dst_w * dst_h * 4];
    let y_ratio = src_h as f32 / dst_h as f32;
    let y_scale = y_ratio.max(1.0);
    let y_support = 3.0 * y_scale;

    for dy in 0..dst_h {
        let center = (dy as f32 + 0.5) * y_ratio - 0.5;
        let top = (center - y_support).floor().max(0.0) as usize;
        let bottom = (center + y_support).ceil().min((src_h - 1) as f32) as usize;

        weights.clear();
        let mut weight_sum = 0.0f32;
        for sy in top..=bottom {
            let w = lanczos3_kernel((sy as f32 - center) / y_scale);
            weights.push(w);
            weight_sum += w;
        }
        let inv_w = if weight_sum.abs() > 1e-6 {
            1.0 / weight_sum
        } else {
            1.0
        };

        for dx in 0..dst_w {
            let mut acc = [0.0f32; 4];
            for (k, sy) in (top..=bottom).enumerate() {
                let w = weights[k] * inv_w;
                let px = temp[sy * dst_w + dx];
                acc[0] += px[0] * w;
                acc[1] += px[1] * w;
                acc[2] += px[2] * w;
                acc[3] += px[3] * w;
            }

            let out_idx = (dy * dst_w + dx) * 4;
            let a = acc[3].clamp(0.0, 1.0);
            if a > 1e-4 {
                dst[out_idx] = (acc[0] / a).round().clamp(0.0, 255.0) as u8;
                dst[out_idx + 1] = (acc[1] / a).round().clamp(0.0, 255.0) as u8;
                dst[out_idx + 2] = (acc[2] / a).round().clamp(0.0, 255.0) as u8;
                dst[out_idx + 3] = (a * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
    }

    dst
}
