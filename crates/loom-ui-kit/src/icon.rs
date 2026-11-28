//! Conversion helpers from platform-neutral RGBA8 executable icons to GPUI [`RenderImage`].

use std::sync::Arc;

use gpui_kit::RenderImage;
use image::{Frame, RgbaImage};
use loom_types::ExeIconRgba;

/// Converts a tightly packed top-down RGBA8 [`ExeIconRgba`] into a GPUI [`RenderImage`].
///
/// Returns `None` if either dimension is zero, if `width * height * 4` overflows,
/// or if `icon.rgba.len() != width * height * 4`.
///
/// GPUI's [`RenderImage`] stores top-down BGRA8 frames, so this helper swaps the
/// red and blue channels in place on the copied frame buffer while preserving row
/// order and alpha so icons render right-side-up with accurate colors.
pub fn rgba_icon_to_render_image(icon: &ExeIconRgba) -> Option<Arc<RenderImage>> {
    if icon.width == 0 || icon.height == 0 {
        return None;
    }

    let expected_len = usize::from(icon.width)
        .checked_mul(usize::from(icon.height))?
        .checked_mul(4)?;
    if icon.rgba.len() != expected_len {
        return None;
    }

    let mut bgra = icon.rgba.to_vec();
    for pixel in bgra.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }

    let buffer = RgbaImage::from_raw(u32::from(icon.width), u32::from(icon.height), bgra)?;
    let frame = Frame::new(buffer);
    Some(Arc::new(RenderImage::new([frame])))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui_kit::{DevicePixels, Size};
    use loom_types::ExeIconRgba;

    use super::rgba_icon_to_render_image;

    #[test]
    fn rgba_icon_to_render_image_validates_and_swaps_rb_channels() {
        let icon = ExeIconRgba {
            width: 2,
            height: 1,
            rgba: Arc::from([0x11, 0x22, 0x33, 0xFF, 0xAA, 0xBB, 0xCC, 0x80]),
        };
        let rendered = rgba_icon_to_render_image(&icon).expect("valid 2x1 RGBA icon");
        assert_eq!(rendered.frame_count(), 1);
        assert_eq!(
            rendered.size(0),
            Size {
                width: DevicePixels(2),
                height: DevicePixels(1),
            }
        );
        assert_eq!(
            rendered.as_bytes(0),
            Some(&[0x33, 0x22, 0x11, 0xFF, 0xCC, 0xBB, 0xAA, 0x80][..])
        );

        assert!(
            rgba_icon_to_render_image(&ExeIconRgba {
                width: 0,
                height: 16,
                rgba: Arc::from([]),
            })
            .is_none()
        );
        assert!(
            rgba_icon_to_render_image(&ExeIconRgba {
                width: 16,
                height: 0,
                rgba: Arc::from([]),
            })
            .is_none()
        );
        assert!(
            rgba_icon_to_render_image(&ExeIconRgba {
                width: 2,
                height: 2,
                rgba: Arc::from([0u8; 12]),
            })
            .is_none()
        );
        assert!(
            rgba_icon_to_render_image(&ExeIconRgba {
                width: 1,
                height: 1,
                rgba: Arc::from([0u8; 8]),
            })
            .is_none()
        );
    }
}
