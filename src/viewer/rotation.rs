//! Rotation state and the CPU rotation of decoded pixels.
//!
//! GPUI cannot rotate an image element and the `image` crate is not a
//! direct dependency, so a rotated view is produced by turning GPUI's own
//! decoded BGRA pixels and handing them back to GPUI as an (uncompressed)
//! PNG, which its loader decodes like any other file.

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

    /// Whether width and height trade places.
    pub(crate) fn swaps_axes(self) -> bool {
        self.0 % 2 == 1
    }

    pub(crate) fn quarter_turns(self) -> u8 {
        self.0
    }
}

/// Pixels (4 bytes each) with their size.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Pixels4 {
    pub(crate) bytes: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// Rotates `bytes` (`width × height` 4-byte pixels, row-major). `None` when
/// the buffer does not match the size.
pub(crate) fn rotate_pixels(
    bytes: &[u8],
    width: u32,
    height: u32,
    rotation: Rotation,
) -> Option<Pixels4> {
    let (w, h) = (usize::try_from(width).ok()?, usize::try_from(height).ok()?);
    if bytes.len() != w.checked_mul(h)?.checked_mul(4)? {
        return None;
    }
    if rotation.is_identity() {
        return Some(Pixels4 {
            bytes: bytes.to_vec(),
            width,
            height,
        });
    }
    let (out_w, out_h) = if rotation.swaps_axes() {
        (h, w)
    } else {
        (w, h)
    };
    let mut out = vec![0u8; bytes.len()];
    for (index, pixel) in bytes.as_chunks::<4>().0.iter().enumerate() {
        let (x, y) = (index % w, index / w);
        let (nx, ny) = match rotation.quarter_turns() {
            1 => (h - 1 - y, x),
            2 => (w - 1 - x, h - 1 - y),
            _ => (y, w - 1 - x),
        };
        let at = (ny * out_w + nx) * 4;
        out.get_mut(at..at + 4)?.copy_from_slice(pixel);
    }
    Some(Pixels4 {
        bytes: out,
        width: u32::try_from(out_w).ok()?,
        height: u32::try_from(out_h).ok()?,
    })
}

/// Encodes BGRA pixels as an uncompressed PNG (RGB when every pixel is
/// opaque, RGBA otherwise). `None` when the buffer does not match the size.
pub(crate) fn encode_png(bgra: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let (w, h) = (usize::try_from(width).ok()?, usize::try_from(height).ok()?);
    if bytes_len(w, h)? != bgra.len() || w == 0 || h == 0 {
        return None;
    }
    let opaque = bgra
        .as_chunks::<4>()
        .0
        .iter()
        .all(|&[_, _, _, alpha]| alpha == 255);
    let channels = if opaque { 3 } else { 4 };

    // One filter byte (0 = none) per scanline, then the pixels as RGB(A).
    let mut raw = Vec::with_capacity(h * (1 + w * channels));
    for row in bgra.chunks_exact(w * 4) {
        raw.push(0);
        for &[blue, green, red, alpha] in row.as_chunks::<4>().0 {
            raw.extend_from_slice(&[red, green, blue]);
            if !opaque {
                raw.push(alpha);
            }
        }
    }

    let mut zlib = Vec::with_capacity(raw.len() + raw.len() / 65_535 * 5 + 16);
    zlib.extend_from_slice(&[0x78, 0x01]);
    let mut blocks = raw.chunks(STORED_BLOCK).peekable();
    while let Some(block) = blocks.next() {
        let len = u16::try_from(block.len()).ok()?;
        zlib.push(u8::from(blocks.peek().is_none()));
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, if opaque { 2 } else { 6 }, 0, 0, 0]);

    let mut png = Vec::with_capacity(zlib.len() + 64);
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    push_chunk(&mut png, *b"IHDR", &header);
    push_chunk(&mut png, *b"IDAT", &zlib);
    push_chunk(&mut png, *b"IEND", &[]);
    Some(png)
}

/// The most a stored deflate block carries.
const STORED_BLOCK: usize = 65_535;

fn bytes_len(width: usize, height: usize) -> Option<usize> {
    width.checked_mul(height)?.checked_mul(4)
}

fn push_chunk(png: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    let len = u32::try_from(data.len()).unwrap_or(u32::MAX);
    png.extend_from_slice(&len.to_be_bytes());
    let start = png.len();
    png.extend_from_slice(&kind);
    png.extend_from_slice(data);
    let crc = crc32(png.get(start..).unwrap_or_default());
    png.extend_from_slice(&crc.to_be_bytes());
}

/// CRC-32 (IEEE), as PNG chunks use.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Adler-32, the zlib stream checksum.
pub(crate) fn adler32(data: &[u8]) -> u32 {
    const MODULUS: u32 = 65_521;
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5_552) {
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= MODULUS;
        b %= MODULUS;
    }
    (b << 16) | a
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A 3×2 image whose pixels are numbered 0..6 in the first byte.
    fn grid() -> Vec<u8> {
        (0..6u8).flat_map(|n| [n, 0, 0, 255]).collect()
    }

    fn ids(pixels: &Pixels4) -> Vec<u8> {
        pixels
            .bytes
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
        assert!(Rotation::default().clockwise().swaps_axes());
        assert!(!Rotation::default().clockwise().clockwise().swaps_axes());
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
        assert_eq!((turned.width, turned.height), (2, 3));
        assert_eq!(ids(&turned), [3, 0, 4, 1, 5, 2]);
    }

    #[test]
    fn rotating_counter_clockwise_is_the_opposite_turn() {
        let turned = rotate_pixels(&grid(), 3, 2, Rotation::default().counter_clockwise());
        let turned = turned.expect("matching buffer");
        assert_eq!((turned.width, turned.height), (2, 3));
        assert_eq!(ids(&turned), [2, 5, 1, 4, 0, 3]);
    }

    #[test]
    fn half_turn_reverses_and_keeps_the_size() {
        let turned = rotate_pixels(&grid(), 3, 2, Rotation::default().clockwise().clockwise());
        let turned = turned.expect("matching buffer");
        assert_eq!((turned.width, turned.height), (3, 2));
        assert_eq!(ids(&turned), [5, 4, 3, 2, 1, 0]);
    }

    #[test]
    fn identity_and_mismatched_buffers() {
        let same = rotate_pixels(&grid(), 3, 2, Rotation::default()).expect("matching buffer");
        assert_eq!(same.bytes, grid());
        assert_eq!(
            rotate_pixels(&grid(), 4, 2, Rotation::default().clockwise()),
            None
        );
    }

    #[test]
    fn checksums_match_their_reference_values() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    /// Splits a PNG into `(type, data)` chunks, checking signature and CRCs.
    fn chunks(png: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let mut at = 8;
        let mut out = Vec::new();
        while at < png.len() {
            let len = u32::from_be_bytes(png[at..at + 4].try_into().expect("4 bytes")) as usize;
            let kind: [u8; 4] = png[at + 4..at + 8].try_into().expect("4 bytes");
            let data = png[at + 8..at + 8 + len].to_vec();
            let crc = u32::from_be_bytes(
                png[at + 8 + len..at + 12 + len]
                    .try_into()
                    .expect("4 bytes"),
            );
            assert_eq!(crc, crc32(&png[at + 4..at + 8 + len]), "chunk CRC");
            out.push((kind, data));
            at += 12 + len;
        }
        out
    }

    /// Joins the stored deflate blocks of a zlib stream.
    fn unstore(zlib: &[u8]) -> Vec<u8> {
        let mut at = 2;
        let mut raw = Vec::new();
        loop {
            let last = zlib[at] & 1 == 1;
            assert_eq!(zlib[at] >> 1, 0, "stored block");
            let len = usize::from(u16::from_le_bytes([zlib[at + 1], zlib[at + 2]]));
            let nlen = u16::from_le_bytes([zlib[at + 3], zlib[at + 4]]);
            assert_eq!(nlen, !(len as u16));
            raw.extend_from_slice(&zlib[at + 5..at + 5 + len]);
            at += 5 + len;
            if last {
                break;
            }
        }
        let sum = u32::from_be_bytes(zlib[at..at + 4].try_into().expect("4 bytes"));
        assert_eq!(sum, adler32(&raw), "adler32");
        raw
    }

    #[test]
    fn png_of_an_opaque_image_is_rgb_and_round_trips() {
        // BGRA (1,2,3) and (4,5,6), 2×1, opaque.
        let bgra = [3, 2, 1, 255, 6, 5, 4, 255];
        let png = encode_png(&bgra, 2, 1).expect("matching buffer");
        let parts = chunks(&png);
        let kinds: Vec<_> = parts.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(kinds, [*b"IHDR", *b"IDAT", *b"IEND"]);
        let header = &parts[0].1;
        assert_eq!(&header[..8], [0, 0, 0, 2, 0, 0, 0, 1]);
        assert_eq!(&header[8..], [8, 2, 0, 0, 0]);
        assert_eq!(unstore(&parts[1].1), [0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn png_with_transparency_keeps_alpha_and_splits_large_images() {
        let bgra = [3, 2, 1, 128];
        let png = encode_png(&bgra, 1, 1).expect("matching buffer");
        let parts = chunks(&png);
        assert_eq!(&parts[0].1[8..], [8, 6, 0, 0, 0]);
        assert_eq!(unstore(&parts[1].1), [0, 1, 2, 3, 128]);

        // 300 × 300 opaque RGB is larger than one 65535-byte stored block.
        let big = vec![255u8; 300 * 300 * 4];
        let png = encode_png(&big, 300, 300).expect("matching buffer");
        let parts = chunks(&png);
        let raw = unstore(&parts[1].1);
        assert_eq!(raw.len(), 300 * (1 + 300 * 3));
        assert_eq!(encode_png(&big, 301, 300), None);
    }
}
