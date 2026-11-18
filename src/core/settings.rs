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
//! overwritten, so the newer app's file survives. Saving writes a temporary
//! file next to the target, syncs it and renames it over, so a crash never
//! leaves half a file.
//!
//! Paths are stored losslessly: a UTF-8 path is a plain TOML string, any
//! other path is `{ bytes = [..] }` holding its raw bytes, so a favorite or
//! tab on a non-UTF-8 name round-trips instead of being lossy or dropped.
//!
//! Both files are written with mode `0600`: they list the user's folders.

use std::ffi::OsStr;
use std::io::{self, Write as _};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::Location;

/// The `version` of the settings file this build reads and writes.
pub const SETTINGS_VERSION: u32 = 1;
/// The `version` of the session file this build reads and writes.
pub const SESSION_VERSION: u32 = 1;

/// The directory both files live in, below the XDG base directory.
const APP_DIR: &str = "fileflux";
const SETTINGS_FILE: &str = "settings.toml";
const SESSION_FILE: &str = "session.toml";

/// The sidebar's width bounds, in CSS pixels, and its default.
pub const SIDEBAR_WIDTH_RANGE: (u32, u32) = (200, 600);
const DEFAULT_SIDEBAR_WIDTH: u32 = 290;
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
    /// Hide names starting with a dot.
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
            sort_key: SortKeyPref::Name,
            sort_dir: SortDirPref::Ascending,
            folders_first: true,
            natural: true,
            hide_hidden: false,
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
            sidebar_collapsed: false,
            details_open: true,
            window_width: DEFAULT_WINDOW.0,
            window_height: DEFAULT_WINDOW.1,
        }
    }
}

impl Preferences {
    /// The preferences with the sidebar and window size clamped to sane
    /// bounds, so a hand-edited file cannot open an unusable window.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        self.sidebar_width = self
            .sidebar_width
            .clamp(SIDEBAR_WIDTH_RANGE.0, SIDEBAR_WIDTH_RANGE.1);
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
    /// The favorite folders, in the order they were added.
    #[serde(with = "path_list")]
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
}

impl Stored for Settings {
    const LABEL: &'static str = "Settings";
    const VERSION: u32 = SETTINGS_VERSION;
}

impl Stored for Session {
    const LABEL: &'static str = "Session";
    const VERSION: u32 = SESSION_VERSION;
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
            };
        }
    };

    let parsed = std::str::from_utf8(&bytes)
        .map_err(|error| error.to_string())
        .and_then(|text| {
            text.parse::<toml::Table>()
                .map_err(|error| error.to_string())
        });
    let table = match parsed {
        Ok(table) => table,
        Err(reason) => return corrupt(path, &file_name, now, &reason),
    };

    // A newer version may have changed the shape: do not guess, and keep it.
    if let Some(version) = table.get("version").and_then(toml::Value::as_integer)
        && version > i64::from(T::VERSION)
    {
        return Loaded {
            value: T::default(),
            notice: Some(format!(
                "{} file {file_name} was written by a newer Fileflux (version {version}); \
                 using defaults and leaving it untouched",
                T::LABEL
            )),
            writable: false,
        };
    }

    match table.try_into::<T>() {
        Ok(value) => Loaded {
            value,
            notice: None,
            writable: true,
        },
        Err(error) => corrupt(path, &file_name, now, &error.to_string()),
    }
}

/// Defaults for a file that cannot be parsed, after copying it aside.
fn corrupt<T: Stored>(path: &Path, file_name: &str, now: SystemTime, reason: &str) -> Loaded<T> {
    log::warn!("{} is invalid: {reason}", path.display());
    let notice = match keep_aside(path, file_name, now) {
        Ok(kept) => format!(
            "{} file was invalid; using defaults (kept as {kept})",
            T::LABEL
        ),
        Err(error) => format!(
            "{} file was invalid; using defaults (could not keep a copy: {error})",
            T::LABEL
        ),
    };
    Loaded {
        value: T::default(),
        notice: Some(notice),
        writable: true,
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

/// Serializes writers, so the debounced background save and the save on
/// close never share a temporary file.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Writes `settings.toml` atomically, creating its directory.
///
/// # Errors
///
/// Any I/O failure; the previous file is left as it was.
pub fn save_settings(path: &Path, settings: &Settings) -> io::Result<()> {
    save(path, settings)
}

/// Writes `session.toml` atomically, creating its directory.
///
/// # Errors
///
/// Any I/O failure; the previous file is left as it was.
pub fn save_session(path: &Path, session: &Session) -> io::Result<()> {
    save(path, session)
}

fn save<T: Stored>(path: &Path, value: &T) -> io::Result<()> {
    let text = toml::to_string_pretty(value).map_err(io::Error::other)?;
    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    write_atomic(path, text.as_bytes())
}

/// Temporary file in the same directory (so the rename stays on one
/// filesystem), synced, then renamed over `path`; the directory is synced
/// after, best effort. The temporary file is removed when anything fails.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);

    let written = (|| {
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        out.write_all(bytes)?;
        out.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        if let Err(error) = std::fs::remove_file(&tmp) {
            log::debug!("cannot remove {}: {error}", tmp.display());
        }
        return written;
    }
    if let Ok(dir) = std::fs::File::open(dir)
        && let Err(error) = dir.sync_all()
    {
        log::debug!("cannot sync {}: {error}", path.display());
    }
    Ok(())
}

// --- restoring the session ----------------------------------------------------

/// One tab to reopen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoredTab {
    pub location: Location,
    pub view: ViewMode,
    pub arrangement: ArrangementPrefs,
    /// The saved folder that no longer exists, when `location` is the
    /// fallback that replaced it.
    pub fell_back_from: Option<PathBuf>,
}

/// The tabs to reopen and which one is active.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restored {
    pub tabs: Vec<RestoredTab>,
    pub active: usize,
}

/// The tabs a saved session reopens. Every saved tab comes back, in order: a
/// folder that is gone is replaced by its nearest existing ancestor (so the
/// tab still opens somewhere sensible), else by `home`, else by `/`.
/// `is_dir` answers whether a folder exists. The active index is clamped.
pub fn restore_session(
    session: &Session,
    home: Option<&Path>,
    is_dir: impl Fn(&Path) -> bool,
) -> Restored {
    let tabs: Vec<RestoredTab> = session
        .tabs
        .iter()
        .map(|tab| {
            let (location, fell_back_from) = match &tab.path {
                _ if tab.trash => (Location::Trash, None),
                Some(path) if is_dir(path) => (Location::Directory(path.clone()), None),
                Some(path) => {
                    let ancestor = path.ancestors().skip(1).find(|dir| is_dir(dir));
                    let fallback = ancestor
                        .map(Path::to_path_buf)
                        .or_else(|| home.map(Path::to_path_buf))
                        .unwrap_or_else(|| PathBuf::from("/"));
                    (Location::Directory(fallback), Some(path.clone()))
                }
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

/// Whether this launch reopens the saved session and keeps saving it. A
/// path on the command line wins: it opens only that folder, and the saved
/// session is neither restored nor overwritten, so a quick `fileflux ~/x`
/// never replaces the tabs of the last regular run.
pub fn launch_uses_session(cli_start: Option<&Path>) -> bool {
    cli_start.is_none()
}
