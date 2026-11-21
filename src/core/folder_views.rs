//! Per-folder view memory: `$XDG_STATE_HOME/fileflux/folders.toml` (fallback
//! `~/.local/state`) remembers, for each folder the user changed the view of,
//! the view (grid, list, compact), the sort, folders-first, the hidden-files
//! toggle and the type/date/size filters ([`FolderView`]).
//!
//! The file is a bounded least-recently-used list keyed by the absolute path
//! as given (no symlink resolution): recording a folder puts it at the most
//! recent end, and past [`MAX_FOLDERS`] the least recent folder is forgotten.
//! It is versioned and loaded like `session.toml` (see [`super::settings`]):
//! tolerant of unknown keys, a corrupt file is kept aside as
//! `folders.toml.bad-<timestamp>` with one notice, a file written by a newer
//! Fileflux is never overwritten. Saving is atomic and generation-ordered.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::Location;
use super::settings::{
    ArrangementPrefs, Loaded, SaveOutcome, Stored, ViewMode, load, path_one, save_ordered, xdg_file,
};

/// The `version` of the folders file this build reads and writes.
pub const FOLDERS_VERSION: u32 = 1;
/// How many folders are remembered; the least recently used is forgotten.
pub const MAX_FOLDERS: usize = 1000;

const FOLDERS_FILE: &str = "folders.toml";

/// Where the folders file lives; `None` when neither the XDG variable nor
/// `$HOME` is usable, in which case nothing is remembered between runs.
#[must_use]
pub fn folders_path(home: Option<&OsStr>, xdg_state_home: Option<&OsStr>) -> Option<PathBuf> {
    xdg_file(home, xdg_state_home, &[".local", "state"], FOLDERS_FILE)
}

/// How one folder is presented: what a tab holds besides its location. The
/// same stored types a session tab uses.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FolderView {
    pub view: ViewMode,
    pub arrangement: ArrangementPrefs,
}

/// One remembered folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FolderRecord {
    #[serde(with = "path_one")]
    pub path: PathBuf,
    pub view: ViewMode,
    pub arrangement: ArrangementPrefs,
}

impl Default for FolderRecord {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            view: ViewMode::default(),
            arrangement: ArrangementPrefs::default(),
        }
    }
}

/// `folders.toml`: the remembered folders, least recently used first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FolderViews {
    pub version: u32,
    pub folders: Vec<FolderRecord>,
}

impl Default for FolderViews {
    fn default() -> Self {
        Self {
            version: FOLDERS_VERSION,
            folders: Vec::new(),
        }
    }
}

impl Stored for FolderViews {
    const LABEL: &'static str = "Folder views";
    const VERSION: u32 = FOLDERS_VERSION;

    fn migrate(_table: &mut toml::Table) -> bool {
        false
    }
}

impl FolderViews {
    /// What is remembered for `path`, without counting as a use.
    #[must_use]
    pub fn get(&self, path: &Path) -> Option<FolderView> {
        self.folders
            .iter()
            .find(|record| record.path == path)
            .map(|record| FolderView {
                view: record.view,
                arrangement: record.arrangement.clone(),
            })
    }

    /// Remembers `view` for `path` and makes it the most recently used,
    /// forgetting the least recently used folder past [`MAX_FOLDERS`]. A
    /// relative path names no folder and is ignored.
    pub fn record(&mut self, path: &Path, view: &FolderView) {
        if !path.is_absolute() {
            return;
        }
        self.folders.retain(|record| record.path != path);
        self.folders.push(FolderRecord {
            path: path.to_path_buf(),
            view: view.view,
            arrangement: view.arrangement.clone(),
        });
        self.enforce_cap();
    }

    /// Forgets `path`; whether it was remembered.
    pub fn forget(&mut self, path: &Path) -> bool {
        let before = self.folders.len();
        self.folders.retain(|record| record.path != path);
        self.folders.len() != before
    }

    /// How many folders are remembered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.folders.len()
    }

    /// Nothing is remembered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.folders.is_empty()
    }

    fn enforce_cap(&mut self) {
        let excess = self.folders.len().saturating_sub(MAX_FOLDERS);
        self.folders.drain(..excess);
    }

    /// A file read from disk: relative paths dropped, a repeated path kept
    /// once at its most recent place, the cap enforced.
    fn normalized(mut self) -> Self {
        let mut seen = std::collections::BTreeSet::new();
        let mut kept: Vec<FolderRecord> = Vec::with_capacity(self.folders.len());
        for record in self.folders.into_iter().rev() {
            if record.path.is_absolute() && seen.insert(record.path.clone()) {
                kept.push(record);
            }
        }
        kept.reverse();
        self.folders = kept;
        self.enforce_cap();
        self
    }
}

/// The presentation a tab takes when it commits to `location`.
///
/// - A folder with a remembered view uses it;
/// - any other location keeps what the tab `current`ly has, which is how
///   every folder looked before folders were remembered: the last choice
///   carries over to folders without a memory of their own, and the Trash,
///   the recent files and a search are never remembered.
#[must_use]
pub fn presentation_for(
    location: &Location,
    memory: &FolderViews,
    current: &FolderView,
) -> FolderView {
    match location {
        Location::Directory(path) => memory.get(path).unwrap_or_else(|| current.clone()),
        Location::Trash | Location::Recent | Location::Search { .. } | Location::Usage { .. } => {
            current.clone()
        }
    }
}

/// Reads `folders.toml` at `path`; `now` stamps the aside copy of a corrupt
/// file. See [`super::settings::load_session`].
#[must_use]
pub fn load_folders(path: &Path, now: SystemTime) -> Loaded<FolderViews> {
    let loaded = load::<FolderViews>(path, now);
    Loaded {
        value: loaded.value.normalized(),
        ..loaded
    }
}

/// Writes `folders.toml` atomically, creating its directory; under the write
/// lock a `generation` older than the newest one written to `path` is skipped.
///
/// # Errors
///
/// Any I/O failure; the previous file is left as it was.
pub fn save_folders_ordered(
    path: &Path,
    views: &FolderViews,
    generation: u64,
) -> io::Result<SaveOutcome> {
    save_ordered(path, views, generation)
}
