//! Which files the Fileflux viewer treats as images. Decided from the file
//! name alone, never by reading the file, so the command line and (later)
//! the explorer agree without touching the disk.

use std::ffi::OsStr;
use std::path::Path;

/// The extensions (lowercase, no dot) the viewer opens: the formats GPUI's
/// image loader decodes.
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "ico", "svg", "qoi", "tga", "hdr",
    "exr",
];

/// The MIME types of [`IMAGE_EXTENSIONS`], as freedesktop's shared-mime-info
/// names them; `packaging/fileflux.desktop` must list every one.
pub const IMAGE_MIME_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/bmp",
    "image/tiff",
    "image/vnd.microsoft.icon",
    "image/svg+xml",
    "image/qoi",
    "image/x-tga",
    "image/vnd.radiance",
    "image/x-exr",
];

/// Whether `name` ends in one of [`IMAGE_EXTENSIONS`], compared
/// case-insensitively. Dotfiles such as `.png` have no extension.
pub fn is_image_name(name: &OsStr) -> bool {
    Path::new(name)
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(extension))
        })
}

/// [`is_image_name`] for the final component of `path`.
pub fn is_image_path(path: &Path) -> bool {
    path.file_name().is_some_and(is_image_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_extension_is_an_image_in_any_case() {
        for extension in IMAGE_EXTENSIONS {
            assert!(is_image_name(OsStr::new(&format!("a.{extension}"))));
            assert!(is_image_name(OsStr::new(&format!(
                "A.{}",
                extension.to_ascii_uppercase()
            ))));
        }
    }

    #[test]
    fn other_names_are_not_images() {
        for name in [
            "notes.txt",
            "png",
            ".png",
            "photo.png.bak",
            "a.avif",
            "a.",
            "",
        ] {
            assert!(!is_image_name(OsStr::new(name)), "{name}");
        }
    }

    #[test]
    fn paths_use_their_last_component() {
        assert!(is_image_path(Path::new("/tmp/dir.d/photo.JPG")));
        assert!(!is_image_path(Path::new("/")));
    }
}
