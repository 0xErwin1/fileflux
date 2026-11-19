//! Filesystem-independent shared data for the read-only explorer.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::SystemTime;

pub mod apps;
pub mod bookmarks;
pub mod devices;
pub mod fs;
pub mod images;
pub mod kinds;
pub mod navigation;
pub mod ops;
pub mod places;
pub mod recent;
pub mod settings;
pub mod terminal;
pub mod thumbnails;
pub mod trash;
pub mod watch;

/// A navigable directory, the virtual read-only home Trash or the virtual
/// read-only list of recently used files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    Directory(PathBuf),
    Trash,
    Recent,
}

/// The kind observed when the directory was scanned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

/// A display entry. Paths retain their original bytes, even for lossy labels.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: OsString,
    pub path: PathBuf,
    pub kind: EntryKind,
    pub is_symlink: bool,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    /// Original location from home Trash metadata; display-only, never opened.
    pub origin: Option<PathBuf>,
}

impl Entry {
    pub fn display_name(&self) -> std::borrow::Cow<'_, str> {
        self.name.to_string_lossy()
    }

    pub fn is_directory(&self) -> bool {
        self.kind == EntryKind::Directory
    }
}

/// A completed scan. Partial metadata/enumeration errors are explicit warnings.
#[derive(Clone, Debug, Default)]
pub struct Listing {
    pub entries: Vec<Entry>,
    pub warnings: Vec<String>,
    /// The names the folder's `.hidden` file lists (Nautilus and Thunar's
    /// convention): hidden like dotfiles, but still in `entries`.
    pub hidden_names: BTreeSet<OsString>,
}
