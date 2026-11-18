//! The file categories the explorer filters and sorts by. A category is
//! decided from the entry's kind and name alone, never by reading the file,
//! so the grid, the list and the filter chips always agree.

use std::ffi::OsStr;
use std::path::Path;

use super::images::is_image_name;
use super::{Entry, EntryKind};

/// What a listing entry is, coarsely. The declaration order is the order of
/// the filter chips and of the "Type" sort.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileCategory {
    Folders,
    Images,
    Videos,
    Audio,
    Documents,
    Archives,
    Code,
    Other,
}

const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "webm", "avi", "mov", "wmv", "flv", "m4v", "mpg", "mpeg", "3gp", "ogv",
];
const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "wav", "ogg", "oga", "opus", "m4a", "aac", "wma", "aiff", "aif", "mid", "midi",
];
const DOCUMENT_EXTENSIONS: &[&str] = &[
    "pdf", "doc", "docx", "odt", "rtf", "txt", "md", "markdown", "epub", "xls", "xlsx", "ods",
    "csv", "ppt", "pptx", "odp", "tex",
];
const ARCHIVE_EXTENSIONS: &[&str] = &[
    "zip", "tar", "gz", "tgz", "bz2", "xz", "zst", "7z", "rar", "lz", "lzma", "iso",
];
const CODE_EXTENSIONS: &[&str] = &[
    "rs", "py", "js", "ts", "jsx", "tsx", "c", "h", "cpp", "cc", "hpp", "go", "java", "kt",
    "swift", "rb", "php", "sh", "bash", "zsh", "fish", "lua", "toml", "yaml", "yml", "json", "xml",
    "html", "css", "scss", "sql", "nix", "cs", "hs", "ex", "exs", "zig", "dart", "vue", "svelte",
];

impl FileCategory {
    /// Every category, in chip order.
    pub const ALL: [Self; 8] = [
        Self::Folders,
        Self::Images,
        Self::Videos,
        Self::Audio,
        Self::Documents,
        Self::Archives,
        Self::Code,
        Self::Other,
    ];

    /// The chip and menu label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Folders => "Folders",
            Self::Images => "Images",
            Self::Videos => "Videos",
            Self::Audio => "Audio",
            Self::Documents => "Documents",
            Self::Archives => "Archives",
            Self::Code => "Code",
            Self::Other => "Other",
        }
    }

    /// The category of an entry named `name`: every directory is
    /// [`Self::Folders`]; anything else goes by its extension, compared
    /// case-insensitively. Dotfiles and extensionless names are
    /// [`Self::Other`].
    pub fn of_name(name: &OsStr, is_directory: bool) -> Self {
        if is_directory {
            return Self::Folders;
        }
        if is_image_name(name) {
            return Self::Images;
        }
        let Some(extension) = extension_of(name) else {
            return Self::Other;
        };
        let listed = |known: &[&str]| known.contains(&extension.as_str());
        if listed(VIDEO_EXTENSIONS) {
            Self::Videos
        } else if listed(AUDIO_EXTENSIONS) {
            Self::Audio
        } else if listed(DOCUMENT_EXTENSIONS) {
            Self::Documents
        } else if listed(ARCHIVE_EXTENSIONS) {
            Self::Archives
        } else if listed(CODE_EXTENSIONS) {
            Self::Code
        } else {
            Self::Other
        }
    }

    /// The category of `entry`.
    pub fn of_entry(entry: &Entry) -> Self {
        Self::of_name(&entry.name, entry.kind == EntryKind::Directory)
    }
}

/// The lowercase extension of `name` (lossy), when it has one.
pub fn extension_of(name: &OsStr) -> Option<String> {
    Path::new(name)
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn category(name: &str) -> FileCategory {
        FileCategory::of_name(OsStr::new(name), false)
    }

    #[test]
    fn extensions_map_to_their_category_in_any_case() {
        for (name, expected) in [
            ("a.PNG", FileCategory::Images),
            ("a.svg", FileCategory::Images),
            ("a.MKV", FileCategory::Videos),
            ("a.flac", FileCategory::Audio),
            ("a.PDF", FileCategory::Documents),
            ("notes.md", FileCategory::Documents),
            ("a.tar", FileCategory::Archives),
            ("a.7z", FileCategory::Archives),
            ("main.RS", FileCategory::Code),
            ("a.json", FileCategory::Code),
            ("a.unknown", FileCategory::Other),
        ] {
            assert_eq!(category(name), expected, "{name}");
        }
    }

    #[test]
    fn dotfiles_and_extensionless_names_are_other() {
        for name in [".png", ".bashrc", "Makefile", "trailing.", ""] {
            assert_eq!(category(name), FileCategory::Other, "{name:?}");
        }
    }

    #[test]
    fn only_the_last_extension_counts() {
        assert_eq!(category("photo.png.bak"), FileCategory::Other);
        assert_eq!(category("backup.tar.gz"), FileCategory::Archives);
    }

    #[test]
    fn directories_are_folders_whatever_their_name() {
        assert_eq!(
            FileCategory::of_name(OsStr::new("pics.png"), true),
            FileCategory::Folders
        );
    }

    #[test]
    fn every_image_extension_is_an_image() {
        for extension in super::super::images::IMAGE_EXTENSIONS {
            assert_eq!(category(&format!("a.{extension}")), FileCategory::Images);
        }
    }

    #[test]
    fn chips_are_declared_in_category_order() {
        let mut sorted = FileCategory::ALL;
        sorted.sort();
        assert_eq!(sorted, FileCategory::ALL);
        assert_eq!(FileCategory::ALL[0].label(), "Folders");
        assert_eq!(FileCategory::ALL[7].label(), "Other");
    }
}
