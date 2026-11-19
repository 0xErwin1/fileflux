//! The two files Fileflux keeps between runs, free of GPUI:
//!
//! - `$XDG_CONFIG_HOME/fileflux/settings.toml` (fallback `~/.config`): the
//!   global preferences and the favorites ([`Settings`]);
//! - `$XDG_STATE_HOME/fileflux/session.toml` (fallback `~/.local/state`): the
//!   tabs that were open when the window closed ([`Session`]).
//!
//! Both carry a `version` so they can be migrated later. Loading never
//! fails: unknown keys are ignored, missing keys take their defaults, a
//! corrupt file loads defaults and is kept aside as `<name>.bad-<timestamp>`,
//! and a file written by a *newer* Fileflux loads defaults and is never
//! overwritten, so the newer app's file survives. A corrupt file is healed:
//! once its copy is safely aside, the next save writes a valid file. If the
//! copy cannot be made the file is left alone, never overwritten.
//!
//! Saving writes a uniquely named temporary file next to the target (so two
//! instances never share one), syncs it and renames it over, so a crash never
//! leaves half a file. A symlinked file (a dotfile manager) is written
//! through to its target and stays a symlink. Saves carry a generation so a
//! stale one never overwrites a newer write, and the settings of another
//! instance that changed the file meanwhile are merged (see
//! [`merge_settings`]).
//!
//! Paths are stored losslessly: a UTF-8 path is a plain TOML string, any
//! other path is `{ bytes = [..] }` holding its raw bytes, so a favorite or
//! tab on a non-UTF-8 name round-trips instead of being lossy or dropped.
//!
//! Both files are written with mode `0600`: they list the user's folders.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{self, Write as _};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::Location;

/// The `version` of the settings file this build reads and writes.
pub const SETTINGS_VERSION: u32 = 2;
/// The `version` of the session file this build reads and writes.
pub const SESSION_VERSION: u32 = 2;

/// The directory both files live in, below the XDG base directory.
const APP_DIR: &str = "fileflux";
const SETTINGS_FILE: &str = "settings.toml";
const SESSION_FILE: &str = "session.toml";

/// The sidebar's width bounds, in CSS pixels, and its default.
pub const SIDEBAR_WIDTH_RANGE: (u32, u32) = (200, 600);
pub const DEFAULT_SIDEBAR_WIDTH: u32 = 290;
/// The details panel's width bounds, in CSS pixels, and its default.
pub const DETAILS_WIDTH_RANGE: (u32, u32) = (240, 600);
pub const DEFAULT_DETAILS_WIDTH: u32 = 300;
/// The window size bounds and defaults (the design brief's 1440×900).
pub const WINDOW_WIDTH_RANGE: (u32, u32) = (480, 16_384);
pub const WINDOW_HEIGHT_RANGE: (u32, u32) = (320, 16_384);
const DEFAULT_WINDOW: (u32, u32) = (1440, 900);

// --- locations ----------------------------------------------------------------

/// `<base>/fileflux/<file>` where `base` is the absolute XDG variable, else
/// `home/<fallback>`. A relative or empty variable is ignored, as the XDG
/// specification says.
fn xdg_file(
    home: Option<&OsStr>,
    xdg: Option<&OsStr>,
    fallback: &[&str],
    file: &str,
) -> Option<PathBuf> {
    let base = xdg
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .map(Path::to_path_buf)
        .or_else(|| {
            let home = Path::new(home.filter(|value| !value.is_empty())?);
            Some(
                fallback
                    .iter()
                    .fold(home.to_path_buf(), |dir, part| dir.join(part)),
            )
        })?;
    Some(base.join(APP_DIR).join(file))
}

/// Where the settings file lives; `None` when neither the XDG variable nor
/// `$HOME` is usable, in which case nothing is persisted.
pub fn settings_path(home: Option<&OsStr>, xdg_config_home: Option<&OsStr>) -> Option<PathBuf> {
    xdg_file(home, xdg_config_home, &[".config"], SETTINGS_FILE)
}

/// Where the session file lives; see [`settings_path`].
pub fn session_path(home: Option<&OsStr>, xdg_state_home: Option<&OsStr>) -> Option<PathBuf> {
    xdg_file(home, xdg_state_home, &[".local", "state"], SESSION_FILE)
}

// --- lossless paths -----------------------------------------------------------

/// How a path sits in the file: its text when it is UTF-8, else its bytes.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum PathRepr {
    Text(String),
    Bytes { bytes: Vec<u8> },
}

impl From<&Path> for PathRepr {
    fn from(path: &Path) -> Self {
        match path.to_str() {
            Some(text) => Self::Text(text.to_owned()),
            None => Self::Bytes {
                bytes: path.as_os_str().as_bytes().to_vec(),
            },
        }
    }
}

impl From<PathRepr> for PathBuf {
    fn from(repr: PathRepr) -> Self {
        match repr {
            PathRepr::Text(text) => Self::from(text),
            PathRepr::Bytes { bytes } => Self::from(std::ffi::OsString::from_vec(bytes)),
        }
    }
}

/// `#[serde(with)]` for a list of paths.
mod path_list {
    use super::{PathBuf, PathRepr};
    use serde::{Deserialize as _, Deserializer, Serialize as _, Serializer};

    pub(super) fn serialize<S: Serializer>(paths: &[PathBuf], out: S) -> Result<S::Ok, S::Error> {
        let reprs: Vec<PathRepr> = paths
            .iter()
            .map(|path| PathRepr::from(path.as_path()))
            .collect();
        reprs.serialize(out)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        input: D,
    ) -> Result<Vec<PathBuf>, D::Error> {
        Ok(Vec::<PathRepr>::deserialize(input)?
            .into_iter()
            .map(PathBuf::from)
            .collect())
    }
}

/// `#[serde(with)]` for an optional path.
mod path_option {
    use super::{PathBuf, PathRepr};
    use serde::{Deserialize as _, Deserializer, Serialize as _, Serializer};

    // The signature is fixed by `#[serde(with)]`.
    #[allow(clippy::ref_option)]
    pub(super) fn serialize<S: Serializer>(
        path: &Option<PathBuf>,
        out: S,
    ) -> Result<S::Ok, S::Error> {
        path.as_deref().map(PathRepr::from).serialize(out)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        input: D,
    ) -> Result<Option<PathBuf>, D::Error> {
        Ok(Option::<PathRepr>::deserialize(input)?.map(PathBuf::from))
    }
}

// --- preference types ---------------------------------------------------------

/// Grid or list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    #[default]
    Grid,
    List,
}

/// What the folder is sorted by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortKeyPref {
    #[default]
    Name,
    Size,
    Modified,
    Type,
}

/// The sort direction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortDirPref {
    #[default]
    Ascending,
    Descending,
}

/// A file category filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CategoryPref {
    Folders,
    Images,
    Videos,
    Audio,
    Documents,
    Archives,
    Code,
    Other,
}

/// A modification-time filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatePref {
    Today,
    Last7Days,
    Last30Days,
    ThisYear,
    Older,
}

/// A file-size filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizePref {
    Under1Mb,
    From1To100Mb,
    From100MbTo1Gb,
    Over1Gb,
}

/// A sort and the filters: what the explorer calls an arrangement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ArrangementPrefs {
    pub sort_key: SortKeyPref,
    pub sort_dir: SortDirPref,
    pub folders_first: bool,
    pub natural: bool,
    /// Hide dotfiles and the names listed in a folder's `.hidden` file.
    pub hide_hidden: bool,
    /// The selected categories; none means every category.
    pub kinds: Vec<CategoryPref>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<DatePref>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<SizePref>,
}

impl Default for ArrangementPrefs {
    fn default() -> Self {
        Self {
            sort_key: SortKeyPref::Modified,
            sort_dir: SortDirPref::Descending,
            folders_first: true,
            natural: true,
            hide_hidden: true,
            kinds: Vec::new(),
            modified: None,
            size: None,
        }
    }
}

/// The global preferences.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    /// The view new tabs and the next launch start in: the active tab's.
    #[serde(rename = "default_view")]
    pub view: ViewMode,
    /// The sort and filters new tabs and the next launch start with: the
    /// active tab's.
    pub arrangement: ArrangementPrefs,
    pub sidebar_width: u32,
    /// The details panel's width; files written before it existed load the
    /// default.
    pub details_width: u32,
    pub sidebar_collapsed: bool,
    /// The details panel (Ctrl+I) is open.
    pub details_open: bool,
    pub window_width: u32,
    pub window_height: u32,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            view: ViewMode::Grid,
            arrangement: ArrangementPrefs::default(),
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            details_width: DEFAULT_DETAILS_WIDTH,
            sidebar_collapsed: false,
            details_open: true,
            window_width: DEFAULT_WINDOW.0,
            window_height: DEFAULT_WINDOW.1,
        }
    }
}

impl Preferences {
    /// The preferences with the panel and window sizes clamped to sane
    /// bounds, so a hand-edited file cannot open an unusable window.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        self.sidebar_width = self
            .sidebar_width
            .clamp(SIDEBAR_WIDTH_RANGE.0, SIDEBAR_WIDTH_RANGE.1);
        self.details_width = self
            .details_width
            .clamp(DETAILS_WIDTH_RANGE.0, DETAILS_WIDTH_RANGE.1);
        self.window_width = self
            .window_width
            .clamp(WINDOW_WIDTH_RANGE.0, WINDOW_WIDTH_RANGE.1);
        self.window_height = self
            .window_height
            .clamp(WINDOW_HEIGHT_RANGE.0, WINDOW_HEIGHT_RANGE.1);
        self
    }
}

// --- the files ----------------------------------------------------------------

/// `settings.toml`: the preferences and the favorites.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    /// Legacy favorites of a version 1 file. The favorites now live in the
    /// GTK bookmarks (see [`super::bookmarks`]); the explorer moves these
    /// there once and saves the file without them, so this is empty in a
    /// file written by this version.
    #[serde(with = "path_list", skip_serializing_if = "Vec::is_empty")]
    pub favorites: Vec<PathBuf>,
    pub preferences: Preferences,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            favorites: Vec::new(),
            preferences: Preferences::default(),
        }
    }
}

/// One open tab of the session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionTab {
    /// The folder, unless the tab shows the Trash.
    #[serde(with = "path_option", skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// The tab shows the virtual Trash.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub trash: bool,
    pub view: ViewMode,
    pub arrangement: ArrangementPrefs,
}

impl SessionTab {
    /// A tab on a folder.
    pub fn directory(path: PathBuf, view: ViewMode, arrangement: ArrangementPrefs) -> Self {
        Self {
            path: Some(path),
            trash: false,
            view,
            arrangement,
        }
    }

    /// A tab on the Trash.
    pub fn trash(view: ViewMode, arrangement: ArrangementPrefs) -> Self {
        Self {
            path: None,
            trash: true,
            view,
            arrangement,
        }
    }
}

/// `session.toml`: the tabs that were open, left to right.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub version: u32,
    /// The active tab's position in `tabs`.
    pub active: usize,
    pub tabs: Vec<SessionTab>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            version: SESSION_VERSION,
            active: 0,
            tabs: Vec::new(),
        }
    }
}

/// A file kind: what loading and saving need to know about it.
trait Stored: Serialize + DeserializeOwned + Default {
    /// How the file is named in notices.
    const LABEL: &'static str;
    /// The newest `version` this build understands.
    const VERSION: u32;

    /// Brings a table written by an older version up to [`Self::VERSION`],
    /// touching only what the older meaning requires. Returns whether a part
    /// of the table was not a choice this version ever wrote and was reset to
    /// its defaults.
    fn migrate(table: &mut toml::Table) -> bool;
}

impl Stored for Settings {
    const LABEL: &'static str = "Settings";
    const VERSION: u32 = SETTINGS_VERSION;

    fn migrate(table: &mut toml::Table) -> bool {
        table
            .get_mut("preferences")
            .and_then(toml::Value::as_table_mut)
            .and_then(|preferences| preferences.get_mut("arrangement"))
            .and_then(toml::Value::as_table_mut)
            .is_some_and(migrate_arrangement)
    }
}

impl Stored for Session {
    const LABEL: &'static str = "Session";
    const VERSION: u32 = SESSION_VERSION;

    fn migrate(table: &mut toml::Table) -> bool {
        let tabs = table.get_mut("tabs").and_then(toml::Value::as_array_mut);
        let mut reset = false;
        for tab in tabs.into_iter().flatten() {
            if let Some(arrangement) = tab
                .as_table_mut()
                .and_then(|tab| tab.get_mut("arrangement"))
                .and_then(toml::Value::as_table_mut)
            {
                reset |= migrate_arrangement(arrangement);
            }
        }
        reset
    }
}

/// Version 1 → 2: the defaults changed from Name ascending with hidden files
/// shown to Modified descending with hidden files hidden. Only a saved value
/// equal to the old default moves; any other saved choice stays, and a key
/// the file omits keeps the meaning it had (the old default) when its
/// partner key was a choice.
///
/// A sort value that is not a string is not a choice this version ever wrote:
/// the sort of that arrangement goes back to the defaults (every other
/// preference is kept) and `true` is returned so the user is told. An
/// unknown string is not repaired here: loading it fails and the file is
/// kept aside like any other invalid one.
fn migrate_arrangement(arrangement: &mut toml::Table) -> bool {
    let sort_value = |key: &str, old_default: &str| match arrangement.get(key) {
        None => Some(old_default.to_owned()),
        Some(value) => value.as_str().map(str::to_owned),
    };
    let (Some(key), Some(dir)) = (
        sort_value("sort_key", "name"),
        sort_value("sort_dir", "ascending"),
    ) else {
        arrangement.remove("sort_key");
        arrangement.remove("sort_dir");
        return true;
    };
    if key == "name" && dir == "ascending" {
        arrangement.insert("sort_key".to_owned(), "modified".into());
        arrangement.insert("sort_dir".to_owned(), "descending".into());
    } else {
        arrangement.insert("sort_key".to_owned(), key.into());
        arrangement.insert("sort_dir".to_owned(), dir.into());
    }
    if arrangement
        .get("hide_hidden")
        .and_then(toml::Value::as_bool)
        == Some(false)
    {
        arrangement.insert("hide_hidden".to_owned(), true.into());
    }
    false
}

// --- loading ------------------------------------------------------------------

/// What reading a file produced.
#[derive(Debug, PartialEq, Eq)]
pub struct Loaded<T> {
    /// The file's content, or the defaults when it could not be used.
    pub value: T,
    /// What to tell the user (the status bar), when something was wrong.
    pub notice: Option<String>,
    /// Whether the app may write the file: `false` when it was written by a
    /// newer version or could not be read, so it is never overwritten.
    pub writable: bool,
    /// The file was written by an older version and was migrated in memory:
    /// the next save should replace it, so the migration happens once.
    pub migrated: bool,
    /// The file was invalid and its copy is safely aside: the next save
    /// should replace it with a valid file, so the notice shows once.
    pub heal: bool,
}

/// Reads `settings.toml` at `path`; `now` stamps the aside copy of a corrupt
/// file.
pub fn load_settings(path: &Path, now: SystemTime) -> Loaded<Settings> {
    load(path, now)
}

/// Reads `session.toml` at `path`; see [`load_settings`].
pub fn load_session(path: &Path, now: SystemTime) -> Loaded<Session> {
    load(path, now)
}

/// How a file's bytes read.
enum Parsed<T> {
    /// The value, whether the file was migrated, whether migration reset a
    /// part that was not a choice this version wrote.
    Value(T, bool, bool),
    /// Written by a newer Fileflux: its `version`.
    Newer(i64),
    /// Not UTF-8, not TOML, or not the expected shape.
    Invalid(String),
}

fn parse<T: Stored>(bytes: &[u8]) -> Parsed<T> {
    let parsed = std::str::from_utf8(bytes)
        .map_err(|error| error.to_string())
        .and_then(|text| {
            text.parse::<toml::Table>()
                .map_err(|error| error.to_string())
        });
    let mut table = match parsed {
        Ok(table) => table,
        Err(reason) => return Parsed::Invalid(reason),
    };
    // A newer version may have changed the shape: do not guess, and keep it.
    if let Some(version) = table.get("version").and_then(toml::Value::as_integer)
        && version > i64::from(T::VERSION)
    {
        return Parsed::Newer(version);
    }
    // A file without a version was written by hand: its values are choices.
    let older = table
        .get("version")
        .and_then(toml::Value::as_integer)
        .is_some_and(|version| version < i64::from(T::VERSION));
    let mut reset = false;
    if older {
        reset = T::migrate(&mut table);
        table.insert("version".to_owned(), i64::from(T::VERSION).into());
    }
    match table.try_into::<T>() {
        Ok(value) => Parsed::Value(value, older, reset),
        Err(error) => Parsed::Invalid(error.to_string()),
    }
}

fn load<T: Stored>(path: &Path, now: SystemTime) -> Loaded<T> {
    let file_name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Loaded {
                value: T::default(),
                notice: None,
                writable: true,
                migrated: false,
                heal: false,
            };
        }
        Err(error) => {
            return Loaded {
                value: T::default(),
                notice: Some(format!(
                    "{} file {file_name} could not be read ({error}); using defaults",
                    T::LABEL
                )),
                writable: false,
                migrated: false,
                heal: false,
            };
        }
    };

    match parse::<T>(&bytes) {
        Parsed::Value(value, migrated, reset) => Loaded {
            value,
            notice: reset.then(|| {
                format!(
                    "{} file was invalid; using defaults for the sort \
                     (the other preferences were kept)",
                    T::LABEL
                )
            }),
            writable: true,
            migrated,
            heal: false,
        },
        Parsed::Newer(version) => Loaded {
            value: T::default(),
            notice: Some(format!(
                "{} file {file_name} was written by a newer Fileflux (version {version}); \
                 using defaults and leaving it untouched",
                T::LABEL
            )),
            writable: false,
            migrated: false,
            heal: false,
        },
        Parsed::Invalid(reason) => corrupt(path, &file_name, now, &reason),
    }
}

/// Defaults for a file that cannot be parsed, after copying it aside. When
/// the copy cannot be made the file is the only copy of what the user wrote:
/// it is left untouched and never written.
fn corrupt<T: Stored>(path: &Path, file_name: &str, now: SystemTime, reason: &str) -> Loaded<T> {
    log::warn!("{} is invalid: {reason}", path.display());
    let (notice, kept) = match keep_aside(path, file_name, now) {
        Ok(kept) => (
            format!(
                "{} file was invalid; using defaults (kept as {kept})",
                T::LABEL
            ),
            true,
        ),
        Err(error) => (
            format!(
                "{} file was invalid; using defaults (could not keep a copy: {error}; \
                 leaving it untouched)",
                T::LABEL
            ),
            false,
        ),
    };
    Loaded {
        value: T::default(),
        notice: Some(notice),
        writable: kept,
        migrated: false,
        heal: kept,
    }
}

/// Copies `path` to `<name>.bad-<UTC timestamp>` beside it and returns the
/// new file's name. The original stays until the next save replaces it.
fn keep_aside(path: &Path, file_name: &str, now: SystemTime) -> io::Result<String> {
    let stamp = DateTime::<Utc>::from(now).format("%Y%m%dT%H%M%SZ");
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    for attempt in 0_u32.. {
        let name = if attempt == 0 {
            format!("{file_name}.bad-{stamp}")
        } else {
            format!("{file_name}.bad-{stamp}-{attempt}")
        };
        let target = dir.join(&name);
        // `create_new` never overwrites an earlier copy.
        let mut out = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&target)
        {
            Ok(out) => out,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        out.write_all(&std::fs::read(path)?)?;
        out.sync_all()?;
        return Ok(name);
    }
    Err(io::Error::other("no free name for the copy"))
}

// --- saving -------------------------------------------------------------------

/// What this process last wrote to a path, behind the lock that serializes
/// its writers (the debounced background save and the save on close).
#[derive(Default)]
struct Written {
    /// The newest generation written: an older one never overwrites it.
    generation: u64,
    /// The settings this process last wrote there (as it held them, before
    /// any merge): what a change by another instance is measured against.
    settings: Option<Settings>,
}

static WRITER: Mutex<BTreeMap<PathBuf, Written>> = Mutex::new(BTreeMap::new());

fn writer() -> MutexGuard<'static, BTreeMap<PathBuf, Written>> {
    WRITER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What an ordered save did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    Written,
    /// A newer generation was already written; the file was not touched.
    Superseded,
}

/// Writes `settings.toml` atomically, creating its directory.
///
/// # Errors
///
/// Any I/O failure; the previous file is left as it was.
pub fn save_settings(path: &Path, settings: &Settings) -> io::Result<()> {
    let mut written = writer();
    write_value(path, settings)?;
    written.entry(path.to_path_buf()).or_default().settings = Some(settings.clone());
    Ok(())
}

/// Writes `session.toml` atomically, creating its directory.
///
/// # Errors
///
/// Any I/O failure; the previous file is left as it was.
pub fn save_session(path: &Path, session: &Session) -> io::Result<()> {
    let _written = writer();
    write_value(path, session)
}

/// [`save_settings`] for a numbered snapshot: under the write lock, a
/// `generation` older than the newest one written to `path` is skipped, so a
/// slow background save never overwrites what the close wrote after it.
///
/// `base` is the settings this process last loaded or wrote when it has not
/// written `path` yet (`None`: overwrite whatever is there, as when healing
/// an invalid file). When the file holds something else than the base, another
/// instance changed it: the favorites are merged ([`merge_settings`]) and the
/// rest is ours.
///
/// # Errors
///
/// Any I/O failure, or the file changed to something invalid or newer, which
/// is left untouched; the previous file is left as it was.
pub fn save_settings_ordered(
    path: &Path,
    settings: &Settings,
    base: Option<&Settings>,
    generation: u64,
) -> io::Result<SaveOutcome> {
    let mut written = writer();
    let entry = written.entry(path.to_path_buf()).or_default();
    if generation < entry.generation {
        return Ok(SaveOutcome::Superseded);
    }
    let merged = match entry.settings.as_ref().or(base) {
        Some(base) => merge_with_disk(path, base, settings)?,
        None => settings.clone(),
    };
    write_value(path, &merged)?;
    entry.generation = generation;
    entry.settings = Some(settings.clone());
    Ok(SaveOutcome::Written)
}

/// [`save_session`] for a numbered snapshot; see [`save_settings_ordered`].
/// The session is not merged: the last writer wins.
///
/// # Errors
///
/// Any I/O failure; the previous file is left as it was.
pub fn save_session_ordered(
    path: &Path,
    session: &Session,
    generation: u64,
) -> io::Result<SaveOutcome> {
    let mut written = writer();
    let entry = written.entry(path.to_path_buf()).or_default();
    if generation < entry.generation {
        return Ok(SaveOutcome::Superseded);
    }
    write_value(path, session)?;
    entry.generation = generation;
    Ok(SaveOutcome::Written)
}

fn write_value<T: Stored>(path: &Path, value: &T) -> io::Result<()> {
    let text = toml::to_string_pretty(value).map_err(io::Error::other)?;
    write_atomic(path, text.as_bytes())
}

/// What is on disk now merged with `ours`; see [`merge_settings`].
fn merge_with_disk(path: &Path, base: &Settings, ours: &Settings) -> io::Result<Settings> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(ours.clone()),
        Err(error) => return Err(error),
    };
    match parse::<Settings>(&bytes) {
        Parsed::Value(disk, ..) => Ok(merge_settings(base, ours, &disk)),
        Parsed::Newer(version) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the file was changed to version {version} by a newer Fileflux; left untouched"
            ),
        )),
        Parsed::Invalid(reason) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the file was changed on disk and is not valid ({reason}); left untouched"),
        )),
    }
}

/// Our settings merged with what another instance left on disk since `base`
/// (what we last loaded or wrote). Favorites are a three-way set merge:
/// what the other instance added stays, what we added is added, what either
/// side removed from `base` stays removed. Every other field is ours (the
/// last writer wins).
#[must_use]
pub fn merge_settings(base: &Settings, ours: &Settings, disk: &Settings) -> Settings {
    let removed_by_us: BTreeSet<&Path> = base
        .favorites
        .iter()
        .filter(|path| !ours.favorites.contains(path))
        .map(PathBuf::as_path)
        .collect();
    let mut favorites: Vec<PathBuf> = Vec::new();
    for path in &disk.favorites {
        if !removed_by_us.contains(path.as_path()) && !favorites.contains(path) {
            favorites.push(path.clone());
        }
    }
    for path in &ours.favorites {
        // In the base and gone from the disk: the other instance removed it.
        if !base.favorites.contains(path) && !favorites.contains(path) {
            favorites.push(path.clone());
        }
    }
    Settings {
        favorites,
        ..ours.clone()
    }
}

/// Counts the temporary files this process created, so names never repeat.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `<name>.<pid>.<counter>.tmp` beside `target`: unique per process and per
/// write, so two instances never share (or truncate) a temporary file.
fn tmp_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_owned();
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    target.with_file_name(name)
}

/// The most symlinks followed before giving up, like the kernel's limit.
const MAX_LINK_HOPS: usize = 40;

/// The file a write to `path` must replace: `path` itself, or what a chain
/// of symlinks (a dotfile manager's) finally points at, which may not exist
/// yet. The symlink itself is never replaced.
pub(crate) fn write_target(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..MAX_LINK_HOPS {
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let link = std::fs::read_link(&current)?;
                current = current.parent().map_or(link.clone(), |dir| dir.join(&link));
            }
            Ok(_) => return Ok(current),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(current),
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::other("too many levels of symbolic links"))
}

/// Unique temporary file in the target's directory (so the rename stays on
/// one filesystem), opened with `create_new` and without following a
/// symlink, synced, then renamed over the target; the directory is synced
/// after, best effort. The temporary file is removed when anything fails.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic_with_mode(path, bytes, 0o600)
}

/// [`write_atomic`] for a file with another permission `mode`, set exactly
/// (the process umask does not narrow it): the GTK bookmarks keep the mode
/// the user's file has.
pub(crate) fn write_atomic_with_mode(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    write_atomic_guarded(path, bytes, mode, || Ok(true)).map(drop)
}

/// [`write_atomic_with_mode`] that asks `still_current` right before the
/// rename and gives the write up (removing the temporary file, returning
/// `false`) when it answers `false`: the caller found the target changed by
/// someone else since it read it.
pub(crate) fn write_atomic_guarded(
    path: &Path,
    bytes: &[u8],
    mode: u32,
    still_current: impl FnOnce() -> io::Result<bool>,
) -> io::Result<bool> {
    let target = write_target(path)?;
    let dir = target
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = tmp_path(&target);

    let written = (|| {
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
            .open(&tmp)?;
        out.set_permissions(std::fs::Permissions::from_mode(mode))?;
        out.write_all(bytes)?;
        out.sync_all()?;
        if !still_current()? {
            return Ok(false);
        }
        std::fs::rename(&tmp, &target).map(|()| true)
    })();
    if !matches!(written, Ok(true)) {
        if let Err(error) = std::fs::remove_file(&tmp) {
            log::debug!("cannot remove {}: {error}", tmp.display());
        }
        return written;
    }
    if let Ok(dir) = std::fs::File::open(dir)
        && let Err(error) = dir.sync_all()
    {
        log::debug!("cannot sync {}: {error}", target.display());
    }
    Ok(true)
}

// --- restoring the session ----------------------------------------------------

/// One tab to reopen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoredTab {
    pub location: Location,
    pub view: ViewMode,
    pub arrangement: ArrangementPrefs,
    /// The saved folder that cannot be opened as written (a relative path),
    /// when `location` is the fallback that replaced it.
    pub fell_back_from: Option<PathBuf>,
}

/// The tabs to reopen and which one is active.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restored {
    pub tabs: Vec<RestoredTab>,
    pub active: usize,
}

/// The tabs a saved session reopens, in order, with the saved folders as
/// they are: nothing here touches the filesystem, so startup never waits on a
/// slow or unmounted folder. The explorer opens each tab optimistically; one
/// whose folder turns out to be gone gets [`restore_fallback`] after its
/// scan fails. Only a relative path, which no scan could resolve, is
/// replaced up front (by `home`, else `/`). The active index is clamped.
pub fn restore_session(session: &Session, home: Option<&Path>) -> Restored {
    let tabs: Vec<RestoredTab> = session
        .tabs
        .iter()
        .map(|tab| {
            let (location, fell_back_from) = match &tab.path {
                _ if tab.trash => (Location::Trash, None),
                Some(path) if path.is_absolute() => (Location::Directory(path.clone()), None),
                Some(path) => (
                    Location::Directory(home.map_or_else(|| PathBuf::from("/"), Path::to_path_buf)),
                    Some(path.clone()),
                ),
                None => (
                    Location::Directory(home.map_or_else(|| PathBuf::from("/"), Path::to_path_buf)),
                    None,
                ),
            };
            RestoredTab {
                location,
                view: tab.view,
                arrangement: tab.arrangement.clone(),
                fell_back_from,
            }
        })
        .collect();
    let active = session.active.min(tabs.len().saturating_sub(1));
    Restored { tabs, active }
}

/// Where a restored tab whose folder `missing` is gone opens instead: its
/// nearest existing ancestor, else `home`, else `/`. `is_dir` answers whether
/// a folder exists; it blocks, so call this off the UI thread.
pub fn restore_fallback(
    missing: &Path,
    home: Option<&Path>,
    is_dir: impl Fn(&Path) -> bool,
) -> PathBuf {
    missing
        .ancestors()
        .skip(1)
        .find(|dir| is_dir(dir))
        .map(Path::to_path_buf)
        .or_else(|| home.map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Whether this launch reopens the saved session and keeps saving it. A
/// path on the command line wins: it opens only that folder, and the saved
/// session is neither restored nor overwritten, so a quick `fileflux ~/x`
/// never replaces the tabs of the last regular run.
pub fn launch_uses_session(cli_start: Option<&Path>) -> bool {
    cli_start.is_none()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn temporary_names_carry_the_pid_and_never_repeat() {
        let target = Path::new("/d/settings.toml");
        let first = tmp_path(target);
        let second = tmp_path(target);
        assert_ne!(first, second);
        let name = first.file_name().expect("has a name").to_string_lossy();
        assert!(
            name.starts_with(&format!("settings.toml.{}.", std::process::id())),
            "{name}"
        );
        assert!(name.ends_with(".tmp"), "{name}");
        assert_eq!(first.parent(), Some(Path::new("/d")));
    }

    #[test]
    fn concurrent_writers_never_tear_the_file() {
        // Two threads write without the in-process lock, as two instances
        // would: unique temporary names keep each write whole.
        let dir = std::env::temp_dir().join(format!("fileflux-tear-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(&dir).expect("dir creates");
        let path = dir.join("settings.toml");
        let payload = |byte: char| byte.to_string().repeat(64 * 1024) + "\n";
        std::thread::scope(|scope| {
            for byte in ['a', 'b'] {
                let path = &path;
                scope.spawn(move || {
                    for _ in 0..200 {
                        write_atomic(path, payload(byte).as_bytes()).expect("write succeeds");
                    }
                });
            }
            scope.spawn(|| {
                for _ in 0..400 {
                    if let Ok(read) = std::fs::read_to_string(&path) {
                        assert!(
                            read == payload('a') || read == payload('b'),
                            "a torn file of {} bytes",
                            read.len()
                        );
                    }
                }
            });
        });
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("dir reads")
            .map(|entry| entry.expect("entry reads").file_name())
            .collect();
        assert_eq!(leftovers, [OsStr::new("settings.toml")]);
        drop(std::fs::remove_dir_all(&dir));
    }
}
