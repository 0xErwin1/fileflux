//! Filesystem-independent shared data for the read-only explorer.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::SystemTime;

pub mod admin;
pub mod apps;
pub mod archive;
pub mod bookmarks;
pub mod child;
pub mod clipboard;
pub mod custom_actions;
pub mod devices;
pub mod du;
pub mod elevate;
pub mod folder_views;
pub mod fs;
pub mod glob;
pub mod icons;
pub mod idnames;
pub mod images;
pub mod kinds;
pub mod links;
pub mod mimes;
pub mod navigation;
pub mod network;
pub mod ops;
pub mod owner_helper;
pub mod pause;
pub mod perms;
pub mod places;
pub mod reason;
pub mod recent;
pub mod runnable;
pub mod search;
pub mod search_match;
pub mod settings;
pub mod templates;
pub mod terminal;
pub mod thumbnailers;
pub mod thumbnails;
pub mod trash;
pub mod usage;
pub mod usage_chart;
pub mod watch;
pub mod zoom;

/// A navigable directory, the virtual read-only home Trash, the virtual
/// read-only list of recently used files or the virtual read-only results of
/// a recursive name search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    Directory(PathBuf),
    Trash,
    Recent,
    /// The files under `root` (and its subfolders) whose name contains
    /// `query`, streamed in by [`search`]. A view of files that live in other
    /// folders: nothing is created or pasted into it.
    Search {
        root: PathBuf,
        query: String,
    },
    /// The disk usage of `root`: its children ranked by the space they
    /// occupy, measured by [`usage`]. A view of measurements, not of
    /// entries: nothing is created or pasted into it, and a saved session
    /// reopens the folder `root` instead.
    Usage {
        root: PathBuf,
    },
}

impl Location {
    /// A view that is not the entries of one folder (Recent, search results,
    /// disk usage): nothing is created or pasted here. Recent and search
    /// results also show each entry's folder.
    #[must_use]
    pub fn lists_files_from_elsewhere(&self) -> bool {
        matches!(
            self,
            Self::Recent | Self::Search { .. } | Self::Usage { .. }
        )
    }
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
    /// Mode, owner and group, read by the scan with the rest of the
    /// metadata; `None` for entries that did not come from a scan with it
    /// (Trash, Recent, search results) or whose metadata is out of reach.
    pub unix: Option<UnixInfo>,
}

/// The unix facts of one entry, resolved once by the scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnixInfo {
    /// The permission bits (`st_mode & 0o7777`).
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// The user name, or the number when the uid has no name.
    pub owner: std::sync::Arc<str>,
    /// The group name, or the number when the gid has no name.
    pub group: std::sync::Arc<str>,
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
