//! A file's MIME type, decided from its name alone: no content sniffing and
//! no read of the shared-mime-info database, so naming a file never touches
//! the disk. The table covers what the icon theme, the system thumbnailers
//! and the "Open with" lookups ask about; anything it does not name has no
//! MIME type here and keeps the plain file glyph.

use std::ffi::OsStr;
use std::path::Path;

/// The extension of `name`, lowercase, without the dot. A dotfile such as
/// `.bashrc` has none.
#[must_use]
pub fn extension_of(name: &OsStr) -> Option<String> {
    Path::new(name)
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .filter(|extension| !extension.is_empty())
}

/// The MIME type of a file named `name`, from its extension alone.
#[must_use]
pub fn of_name(name: &OsStr) -> Option<&'static str> {
    of_extension(extension_of(name)?.as_str())
}

/// [`of_name`] for the final component of `path`.
#[must_use]
pub fn of_path(path: &Path) -> Option<&'static str> {
    path.file_name().and_then(of_name)
}

/// The MIME type of one lowercase extension.
#[must_use]
pub fn of_extension(extension: &str) -> Option<&'static str> {
    Some(match extension {
        // Images.
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "svg" => "image/svg+xml",
        "ico" => "image/vnd.microsoft.icon",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "avif" => "image/avif",
        "jxl" => "image/jxl",
        "psd" => "image/vnd.adobe.photoshop",
        "xcf" => "image/x-xcf",
        "qoi" => "image/qoi",
        "tga" => "image/x-tga",
        "hdr" => "image/vnd.radiance",
        "exr" => "image/x-exr",
        // Video.
        "mp4" => "video/mp4",
        "m4v" => "video/x-m4v",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "mpg" | "mpeg" => "video/mpeg",
        "wmv" => "video/x-ms-wmv",
        "flv" => "video/x-flv",
        "3gp" => "video/3gpp",
        "ogv" => "video/ogg",
        // Audio, which the thumbnailers render as cover art.
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "wav" => "audio/x-wav",
        "wma" => "audio/x-ms-wma",
        "aiff" | "aif" => "audio/x-aiff",
        // Documents.
        "pdf" => "application/pdf",
        "epub" => "application/epub+zip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "odt" => "application/vnd.oasis.opendocument.text",
        "rtf" => "application/rtf",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "csv" => "text/csv",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odp" => "application/vnd.oasis.opendocument.presentation",
        "txt" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        // Scripts, source code and the data formats around them, which the
        // icon theme names per type (see [`icons`]).
        "sh" | "bash" | "zsh" | "fish" => "application/x-shellscript",
        "py" => "text/x-python",
        "rs" => "text/rust",
        "blend" => "application/x-blender",
        "pl" => "text/x-perl",
        "rb" => "application/x-ruby",
        "lua" => "text/x-lua",
        "js" | "mjs" => "text/javascript",
        "ts" => "text/x-typescript",
        "json" => "application/json",
        "toml" => "application/toml",
        "yaml" | "yml" => "application/yaml",
        "nix" => "text/x-nix",
        "desktop" => "application/x-desktop",
        "diff" | "patch" => "text/x-diff",
        _ => return None,
    })
}

/// Whether a file of `mime` may get a preview: the formats the app decodes
/// itself and the ones a system thumbnailer may cover. Decided without
/// asking whether such a thumbnailer is installed, which needs the disk.
#[must_use]
pub fn may_have_preview(mime: &str) -> bool {
    mime.starts_with("image/")
        || mime.starts_with("video/")
        || mime.starts_with("audio/")
        || matches!(
            mime,
            "application/pdf"
                | "application/epub+zip"
                | "application/msword"
                | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                | "application/vnd.oasis.opendocument.text"
                | "application/vnd.ms-excel"
                | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                | "application/vnd.oasis.opendocument.spreadsheet"
                | "application/vnd.ms-powerpoint"
                | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
                | "application/vnd.oasis.opendocument.presentation"
        )
}

/// [`may_have_preview`] for a file named `name`.
#[must_use]
pub fn previewable(name: &OsStr) -> bool {
    of_name(name).is_some_and(may_have_preview)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_name_their_mime_type_in_any_case() {
        assert_eq!(of_name(OsStr::new("a.PDF")), Some("application/pdf"));
        assert_eq!(of_name(OsStr::new("holiday.MKV")), Some("video/x-matroska"));
        assert_eq!(of_name(OsStr::new("photo.jpeg")), Some("image/jpeg"));
        assert_eq!(of_name(OsStr::new(".bashrc")), None, "no extension");
        assert_eq!(of_name(OsStr::new("blender")), None);
        assert_eq!(of_name(OsStr::new("x.unknownext")), None);
    }

    #[test]
    fn scripts_and_source_files_name_their_mime_type() {
        assert_eq!(of_extension("sh"), Some("application/x-shellscript"));
        assert_eq!(of_extension("bash"), Some("application/x-shellscript"));
        assert_eq!(of_extension("zsh"), Some("application/x-shellscript"));
        assert_eq!(of_extension("fish"), Some("application/x-shellscript"));
        assert_eq!(of_extension("py"), Some("text/x-python"));
        assert_eq!(of_extension("rs"), Some("text/rust"));
        assert_eq!(of_extension("blend"), Some("application/x-blender"));
        assert_eq!(of_extension("pl"), Some("text/x-perl"));
        assert_eq!(of_extension("rb"), Some("application/x-ruby"));
        assert_eq!(of_extension("lua"), Some("text/x-lua"));
        assert_eq!(of_extension("js"), Some("text/javascript"));
        assert_eq!(of_extension("mjs"), Some("text/javascript"));
        assert_eq!(of_extension("ts"), Some("text/x-typescript"));
        assert_eq!(of_extension("json"), Some("application/json"));
        assert_eq!(of_extension("toml"), Some("application/toml"));
        assert_eq!(of_extension("yaml"), Some("application/yaml"));
        assert_eq!(of_extension("yml"), Some("application/yaml"));
        assert_eq!(of_extension("nix"), Some("text/x-nix"));
        assert_eq!(of_extension("desktop"), Some("application/x-desktop"));
        assert_eq!(of_extension("diff"), Some("text/x-diff"));
        assert_eq!(of_extension("patch"), Some("text/x-diff"));
        // No thumbnailer claims any of them: the type keeps its glyph.
        for extension in [
            "sh", "py", "rs", "blend", "pl", "rb", "lua", "js", "ts", "json", "toml", "yaml",
            "nix", "desktop", "diff",
        ] {
            let mime = of_extension(extension).unwrap_or_default();
            assert!(!may_have_preview(mime), "{extension} keeps its glyph");
        }
    }

    #[test]
    fn previews_are_expected_for_media_and_documents_but_not_for_scripts() {
        assert!(previewable(OsStr::new("a.mp4")));
        assert!(previewable(OsStr::new("a.pdf")));
        assert!(previewable(OsStr::new("a.odt")));
        assert!(previewable(OsStr::new("a.png")));
        assert!(!previewable(OsStr::new("a.rs")));
        assert!(!previewable(OsStr::new("Makefile")));
        assert!(!may_have_preview("text/plain"));
    }
}
