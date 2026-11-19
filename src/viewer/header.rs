//! Image dimensions read from the file header alone, so a huge image can be
//! refused before anything decodes it.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// The largest side and pixel count the viewer decodes: beyond this a
/// full-resolution RGBA copy (plus GPU texture) is no longer reasonable.
pub(crate) const MAX_SIDE: u32 = 16_384;
pub(crate) const MAX_PIXELS: u64 = 100_000_000;

/// Whether an image of this size is too large to decode.
pub(crate) fn exceeds_cap(width: u32, height: u32) -> bool {
    width > MAX_SIDE || height > MAX_SIDE || u64::from(width) * u64::from(height) > MAX_PIXELS
}

/// Reads the pixel size from the header of PNG, JPEG, GIF, BMP, WebP and
/// QOI data. `None` for other formats and for headers that cannot be read.
pub(crate) fn dimensions<R: Read + Seek>(reader: &mut R) -> io::Result<Option<(u32, u32)>> {
    reader.seek(SeekFrom::Start(0))?;
    let mut head = [0u8; 32];
    let filled = read_up_to(reader, &mut head)?;
    let head = head.get(..filled).unwrap_or_default();
    let be32 = |at: usize| be_u32(head, at);
    let le16 = |at: usize| le_u32(head, at, 2);

    if head.starts_with(b"\x89PNG\r\n\x1a\n") && head.get(12..16) == Some(b"IHDR") {
        return Ok(be32(16).zip(be32(20)));
    }
    if head.starts_with(b"GIF8") {
        return Ok(le16(6).zip(le16(8)));
    }
    if head.starts_with(b"qoif") {
        return Ok(be32(4).zip(be32(8)));
    }
    if head.starts_with(b"BM") {
        return Ok(bmp_size(head));
    }
    if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        return Ok(webp_size(head));
    }
    if head.starts_with(&[0xFF, 0xD8]) {
        return jpeg_size(reader);
    }
    Ok(None)
}

fn read_up_to<R: Read>(reader: &mut R, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while let Some(rest) = buffer.get_mut(filled..).filter(|rest| !rest.is_empty()) {
        match reader.read(rest) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// A little-endian unsigned integer of `len` (1 to 4) bytes.
fn le_u32(bytes: &[u8], at: usize, len: usize) -> Option<u32> {
    let slice = bytes.get(at..at + len)?;
    Some(
        slice
            .iter()
            .rev()
            .fold(0u32, |value, &byte| (value << 8) | u32::from(byte)),
    )
}

fn bmp_size(head: &[u8]) -> Option<(u32, u32)> {
    if le_u32(head, 14, 4)? == 12 {
        return Some((le_u32(head, 18, 2)?, le_u32(head, 20, 2)?));
    }
    let width = i32::from_le_bytes(head.get(18..22)?.try_into().ok()?);
    let height = i32::from_le_bytes(head.get(22..26)?.try_into().ok()?);
    Some((width.unsigned_abs(), height.unsigned_abs()))
}

fn webp_size(head: &[u8]) -> Option<(u32, u32)> {
    match head.get(12..16)? {
        b"VP8X" => Some((le_u32(head, 24, 3)? + 1, le_u32(head, 27, 3)? + 1)),
        b"VP8L" if head.get(20) == Some(&0x2F) => {
            let bits = le_u32(head, 21, 4)?;
            Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
        }
        b"VP8 " if head.get(23..26) == Some(&[0x9D, 0x01, 0x2A]) => {
            Some((le_u32(head, 26, 2)? & 0x3FFF, le_u32(head, 28, 2)? & 0x3FFF))
        }
        _ => None,
    }
}

/// Walks the JPEG segments to the first frame header (SOF).
fn jpeg_size<R: Read + Seek>(reader: &mut R) -> io::Result<Option<(u32, u32)>> {
    reader.seek(SeekFrom::Start(2))?;
    loop {
        let mut byte = [0u8; 1];
        // Find the next marker: 0xFF followed by a non-0xFF code.
        loop {
            if read_up_to(reader, &mut byte)? == 0 {
                return Ok(None);
            }
            if byte[0] == 0xFF {
                break;
            }
        }
        let code = loop {
            if read_up_to(reader, &mut byte)? == 0 {
                return Ok(None);
            }
            if byte[0] != 0xFF {
                break byte[0];
            }
        };
        match code {
            // Markers without a length: padding, TEM and the RSTn family.
            0x00 | 0x01 | 0xD0..=0xD7 => {}
            // End of image, or the scan starts before any frame header.
            0xD9 | 0xDA => return Ok(None),
            _ => {
                let mut length = [0u8; 2];
                if read_up_to(reader, &mut length)? < 2 {
                    return Ok(None);
                }
                let length = u64::from(u16::from_be_bytes(length));
                let is_frame = matches!(code, 0xC0..=0xCF) && !matches!(code, 0xC4 | 0xC8 | 0xCC);
                if is_frame {
                    let mut frame = [0u8; 5];
                    if read_up_to(reader, &mut frame)? < 5 {
                        return Ok(None);
                    }
                    let height = u32::from(u16::from_be_bytes([frame[1], frame[2]]));
                    let width = u32::from(u16::from_be_bytes([frame[3], frame[4]]));
                    return Ok(Some((width, height)));
                }
                reader.seek(SeekFrom::Current(
                    i64::try_from(length.saturating_sub(2)).unwrap_or(0),
                ))?;
            }
        }
    }
}

/// [`dimensions`] of the file at `path`.
pub(crate) fn file_dimensions(path: &Path) -> io::Result<Option<(u32, u32)>> {
    let mut file = std::fs::File::open(path)?;
    // One large read instead of the parser's byte-sized ones: on a phone
    // (gvfs MTP) or network mount every read is a round trip to the device.
    let mut head = Vec::with_capacity(HEAD_READ);
    (&mut file).take(HEAD_READ as u64).read_to_end(&mut head)?;
    if let Some(size) = dimensions(&mut io::Cursor::new(&head))? {
        return Ok(Some(size));
    }
    if head.len() < HEAD_READ {
        // The whole file was read: there is no size to find.
        return Ok(None);
    }
    // The size sits past the first block (a large embedded preview, say).
    dimensions(&mut io::BufReader::with_capacity(HEAD_READ, file))
}

/// How much of a file [`file_dimensions`] reads up front; covers the EXIF
/// block phone cameras write before the frame header.
const HEAD_READ: usize = 256 * 1024;

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn read(bytes: Vec<u8>) -> Option<(u32, u32)> {
        dimensions(&mut Cursor::new(bytes)).expect("in-memory read")
    }

    #[test]
    fn png_reads_the_ihdr() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend([0, 0, 0x10, 0, 0, 0, 0x08, 0x00]);
        assert_eq!(read(png), Some((4096, 2048)));
    }

    #[test]
    fn gif_reads_the_logical_screen() {
        let mut gif = b"GIF89a".to_vec();
        gif.extend([0x40, 0x01, 0xF0, 0x00]);
        assert_eq!(read(gif), Some((320, 240)));
    }

    #[test]
    fn bmp_reads_both_header_flavours() {
        let mut bmp = b"BM".to_vec();
        bmp.extend([0; 12]);
        bmp.extend(40u32.to_le_bytes());
        bmp.extend(640i32.to_le_bytes());
        bmp.extend((-480i32).to_le_bytes());
        assert_eq!(read(bmp), Some((640, 480)));

        let mut core = b"BM".to_vec();
        core.extend([0; 12]);
        core.extend(12u32.to_le_bytes());
        core.extend(100u16.to_le_bytes());
        core.extend(50u16.to_le_bytes());
        assert_eq!(read(core), Some((100, 50)));
    }

    #[test]
    fn qoi_reads_big_endian_sizes() {
        let mut qoi = b"qoif".to_vec();
        qoi.extend(1920u32.to_be_bytes());
        qoi.extend(1080u32.to_be_bytes());
        assert_eq!(read(qoi), Some((1920, 1080)));
    }

    #[test]
    fn webp_reads_all_three_encodings() {
        let mut riff = b"RIFF\0\0\0\0WEBP".to_vec();

        let mut vp8x = riff.clone();
        vp8x.extend(b"VP8X");
        vp8x.extend(10u32.to_le_bytes());
        vp8x.extend([0, 0, 0, 0]);
        vp8x.extend([0x7F, 0x02, 0x00]); // width - 1 = 639
        vp8x.extend([0xDF, 0x01, 0x00]); // height - 1 = 479
        assert_eq!(read(vp8x), Some((640, 480)));

        let mut vp8l = riff.clone();
        vp8l.extend(b"VP8L");
        vp8l.extend(5u32.to_le_bytes());
        // signature, then 14-bit width-1 (99) and 14-bit height-1 (49).
        let packed: u32 = 99 | (49 << 14);
        vp8l.push(0x2F);
        vp8l.extend(packed.to_le_bytes());
        assert_eq!(read(vp8l), Some((100, 50)));

        riff.extend(b"VP8 ");
        riff.extend(10u32.to_le_bytes());
        riff.extend([0, 0, 0, 0x9D, 0x01, 0x2A]);
        riff.extend(321u16.to_le_bytes());
        riff.extend(241u16.to_le_bytes());
        assert_eq!(read(riff), Some((321, 241)));
    }

    #[test]
    fn jpeg_skips_segments_until_the_frame_header() {
        let mut jpeg = vec![0xFF, 0xD8];
        // APP1 with 6 payload bytes (length counts itself).
        jpeg.extend([0xFF, 0xE1, 0x00, 0x08, 1, 2, 3, 4, 5, 6]);
        // Fill bytes before the marker are allowed.
        jpeg.extend([0xFF, 0xFF, 0xC2, 0x00, 0x11, 0x08]);
        jpeg.extend(3000u16.to_be_bytes()); // height
        jpeg.extend(4000u16.to_be_bytes()); // width
        assert_eq!(read(jpeg), Some((4000, 3000)));
    }

    /// A JPEG whose frame header follows `app_segments` full APP1 blocks.
    fn jpeg_after_app_segments(app_segments: usize) -> Vec<u8> {
        let mut jpeg = vec![0xFF, 0xD8];
        for _ in 0..app_segments {
            jpeg.extend([0xFF, 0xE1, 0xFF, 0xFF]);
            jpeg.extend(std::iter::repeat_n(0u8, 0xFFFF - 2));
        }
        jpeg.extend([0xFF, 0xC0, 0x00, 0x11, 0x08]);
        jpeg.extend(3000u16.to_be_bytes());
        jpeg.extend(4000u16.to_be_bytes());
        jpeg
    }

    #[test]
    fn file_dimensions_finds_the_size_before_and_past_the_first_read() -> io::Result<()> {
        let dir = std::env::temp_dir().join(format!("fileflux-header-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let near = dir.join("near.jpg");
        let far = dir.join("far.jpg");
        std::fs::write(&near, jpeg_after_app_segments(1))?;
        // Five 64 KiB segments put the frame header past HEAD_READ.
        std::fs::write(&far, jpeg_after_app_segments(5))?;
        let found = (file_dimensions(&near), file_dimensions(&far));
        std::fs::remove_dir_all(&dir)?;
        assert_eq!(found.0?, Some((4000, 3000)));
        assert_eq!(found.1?, Some((4000, 3000)));
        Ok(())
    }

    #[test]
    fn unknown_truncated_and_scan_first_data_is_none() {
        assert_eq!(read(b"hello, not an image".to_vec()), None);
        assert_eq!(read(Vec::new()), None);
        assert_eq!(read(b"\x89PNG\r\n".to_vec()), None);
        assert_eq!(read(vec![0xFF, 0xD8, 0xFF, 0xDA, 0, 2]), None);
    }

    #[test]
    fn the_cap_limits_each_side_and_the_pixel_count() {
        assert!(!exceeds_cap(16_384, 6_000));
        assert!(exceeds_cap(16_385, 10));
        assert!(exceeds_cap(10, 16_385));
        assert!(exceeds_cap(12_000, 9_000));
        assert!(!exceeds_cap(10_000, 10_000));
    }
}
