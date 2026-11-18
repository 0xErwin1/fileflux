//! Rotation state and the CPU rotation of decoded pixels.
//!
//! GPUI cannot rotate an image element, so a rotated view is produced by
//! turning GPUI's own decoded BGRA pixels with `image::imageops` and handing
//! them back as a [`RenderImage`], with no re-encoding.

use std::sync::Arc;

use gpui::RenderImage;
use image::{Frame, RgbaImage, imageops};

/// A whole number of clockwise quarter turns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Rotation(u8);

impl Rotation {
    pub(crate) fn clockwise(self) -> Self {
        Self((self.0 + 1) % 4)
    }

    pub(crate) fn counter_clockwise(self) -> Self {
        Self((self.0 + 3) % 4)
    }

    pub(crate) fn is_identity(self) -> bool {
        self.0 == 0
    }

    pub(crate) fn quarter_turns(self) -> u8 {
        self.0
    }
}

/// Rotates `bytes` (`width × height` 4-byte pixels, row-major). `None` when
/// the buffer does not match the size.
pub(crate) fn rotate_pixels(
    bytes: &[u8],
    width: u32,
    height: u32,
    rotation: Rotation,
) -> Option<RgbaImage> {
    let pixels = RgbaImage::from_raw(width, height, bytes.to_vec())?;
    Some(match rotation.quarter_turns() {
        0 => pixels,
        1 => imageops::rotate90(&pixels),
        2 => imageops::rotate180(&pixels),
        _ => imageops::rotate270(&pixels),
    })
}

/// Wraps rotated BGRA pixels as a GPUI image. The pixels stay BGRA: the
/// buffer type only carries the bytes, which GPUI reads as they are.
pub(crate) fn render_image(pixels: RgbaImage) -> Arc<RenderImage> {
    Arc::new(RenderImage::new([Frame::new(pixels)]))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A 3×2 image whose pixels are numbered 0..6 in the first byte.
    fn grid() -> Vec<u8> {
        (0..6u8).flat_map(|n| [n, 0, 0, 255]).collect()
    }

    fn ids(pixels: &RgbaImage) -> Vec<u8> {
        pixels
            .as_raw()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| p[0])
            .collect()
    }

    #[test]
    fn quarter_turns_wrap_around() {
        let mut r = Rotation::default();
        assert!(r.is_identity());
        for expected in [1, 2, 3, 0] {
            r = r.clockwise();
            assert_eq!(r.quarter_turns(), expected);
        }
        assert!(r.is_identity());
        assert_eq!(Rotation::default().counter_clockwise().quarter_turns(), 3);
        assert!(
            Rotation::default()
                .clockwise()
                .counter_clockwise()
                .is_identity()
        );
    }

    #[test]
    fn rotating_clockwise_turns_rows_into_columns() {
        // 0 1 2      3 0
        // 3 4 5  ->  4 1
        //            5 2
        let turned = rotate_pixels(&grid(), 3, 2, Rotation::default().clockwise());
        let turned = turned.expect("matching buffer");
        assert_eq!((turned.width(), turned.height()), (2, 3));
        assert_eq!(ids(&turned), [3, 0, 4, 1, 5, 2]);
    }

    #[test]
    fn rotating_counter_clockwise_is_the_opposite_turn() {
        let turned = rotate_pixels(&grid(), 3, 2, Rotation::default().counter_clockwise());
        let turned = turned.expect("matching buffer");
        assert_eq!((turned.width(), turned.height()), (2, 3));
        assert_eq!(ids(&turned), [2, 5, 1, 4, 0, 3]);
    }

    #[test]
    fn half_turn_reverses_and_keeps_the_size() {
        let turned = rotate_pixels(&grid(), 3, 2, Rotation::default().clockwise().clockwise());
        let turned = turned.expect("matching buffer");
        assert_eq!((turned.width(), turned.height()), (3, 2));
        assert_eq!(ids(&turned), [5, 4, 3, 2, 1, 0]);
    }

    #[test]
    fn identity_and_mismatched_buffers() {
        let same = rotate_pixels(&grid(), 3, 2, Rotation::default()).expect("matching buffer");
        assert_eq!(same.as_raw(), &grid());
        assert_eq!(
            rotate_pixels(&grid(), 4, 2, Rotation::default().clockwise()),
            None
        );
    }

    #[test]
    fn the_gpui_image_keeps_the_rotated_bytes_and_size() {
        let turned =
            rotate_pixels(&grid(), 3, 2, Rotation::default().clockwise()).expect("matching buffer");
        let expected = turned.as_raw().clone();
        let image = render_image(turned);
        assert_eq!(image.as_bytes(0), Some(expected.as_slice()));
        let size = image.size(0);
        assert_eq!((i32::from(size.width), i32::from(size.height)), (2, 3));
    }
}
