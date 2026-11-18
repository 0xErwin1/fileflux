//! Text for the info panel and the top bar, free of GPUI.

use std::fmt::Display;
use std::path::Path;
use std::time::SystemTime;

use chrono::{DateTime, TimeZone, Utc};

/// `1.5 MB`-style size, 1024-based, one decimal above bytes.
pub(crate) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS.get(unit).copied().unwrap_or("TB"))
}

/// The format named by the file extension (`PNG`, `JPEG`, ...), or `Unknown`.
pub(crate) fn format_name(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("png") => "PNG",
        Some("jpg" | "jpeg") => "JPEG",
        Some("gif") => "GIF",
        Some("webp") => "WebP",
        Some("bmp") => "BMP",
        Some("tif" | "tiff") => "TIFF",
        Some("ico") => "ICO",
        Some("svg") => "SVG",
        Some("qoi") => "QOI",
        Some("tga") => "TGA",
        Some("hdr") => "Radiance HDR",
        Some("exr") => "OpenEXR",
        _ => "Unknown",
    }
}

/// `2026-11-18 15:42` in the local time zone.
pub(crate) fn modified_label(time: SystemTime) -> String {
    modified_in(time, &chrono::Local)
}

fn modified_in<Tz>(time: SystemTime, zone: &Tz) -> String
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    DateTime::<Utc>::from(time)
        .with_timezone(zone)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// `4000 × 3000 px`.
pub(crate) fn dimensions_label(width: u32, height: u32) -> String {
    format!("{width} × {height} px")
}

/// The sentence shown instead of an image the viewer refuses to decode.
pub(crate) fn too_large_message(width: u32, height: u32) -> String {
    format!(
        "This image is {} — too large to open here (limit {} px per side or {} megapixels).",
        dimensions_label(width, height),
        super::header::MAX_SIDE,
        super::header::MAX_PIXELS / 1_000_000,
    )
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn sizes_scale_by_1024() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn format_comes_from_the_extension_in_any_case() {
        assert_eq!(format_name(Path::new("a.JPG")), "JPEG");
        assert_eq!(format_name(Path::new("a.jpeg")), "JPEG");
        assert_eq!(format_name(Path::new("a.tif")), "TIFF");
        assert_eq!(format_name(Path::new("a.svg")), "SVG");
        assert_eq!(format_name(Path::new("a.xyz")), "Unknown");
        assert_eq!(format_name(Path::new("noext")), "Unknown");
    }

    #[test]
    fn modified_uses_the_given_zone() {
        let time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        assert_eq!(modified_in(time, &Utc), "2023-11-14 22:13");
        let plus_two = chrono::FixedOffset::east_opt(2 * 3600).expect("valid offset");
        assert_eq!(modified_in(time, &plus_two), "2023-11-15 00:13");
    }

    #[test]
    fn labels() {
        assert_eq!(dimensions_label(4000, 3000), "4000 × 3000 px");
        let message = too_large_message(20_000, 10_000);
        assert!(message.contains("20000 × 10000 px"), "{message}");
        assert!(message.contains("too large"), "{message}");
    }
}
