//! Read-only access to the freedesktop home Trash. Reads only
//! `<root>/files` plus matching `<root>/info/<name>.trashinfo` metadata;
//! never creates directories, writes configuration, restores, deletes or
//! follows the recorded original paths.

use super::ops::{fit_name, rename_noreplace, unique_name, validate_name};
use super::{Entry, EntryKind, Listing};
use chrono::NaiveDateTime;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// Parses freedesktop `.trashinfo` content. Only the `[Trash Info]` header
/// is recognized; the percent-encoded `Path` key is decoded to bytes and
/// stays display-only.
pub fn parse_trashinfo(content: &str) -> Result<PathBuf, String> {
    let mut lines = content
        .trim()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let Some(header) = lines.next() else {
        return Err("empty trash info".to_owned());
    };
    if header != "[Trash Info]" {
        return Err(format!(
            "unrecognized trash info header {header:?}; only [Trash Info] is supported"
        ));
    }
    let mut origin = None;
    for line in lines {
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("malformed trash info entry {line:?}"));
        };
        if key.trim() == "Path" {
            let decoded = decode_percent(value.as_bytes())
                .ok_or_else(|| "invalid percent escape in trash info Path".to_owned())?;
            origin = Some(PathBuf::from(OsString::from_vec(decoded)));
        }
        // Other keys such as DeletionDate are accepted and ignored.
    }
    origin.ok_or_else(|| "trash info has no Path".to_owned())
}

/// Decodes percent-encoded bytes; `None` on a malformed escape.
pub fn decode_percent(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len());
    let mut rest = input;
    while let Some(first) = rest.first().copied() {
        if first == b'%' {
            let high = hex_digit(*rest.get(1)?)?;
            let low = hex_digit(*rest.get(2)?)?;
            out.push(high * 16 + low);
            rest = rest.get(3..)?;
        } else {
            out.push(first);
            rest = rest.get(1..).unwrap_or_default();
        }
    }
    Some(out)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Reads the Trash at `root` (typically the home trash): every direct item
/// of `<root>/files` becomes an `Entry`, enriched by the matching
/// `<root>/info/<name>.trashinfo` when readable. A missing `files` directory
/// is an empty listing and nothing is ever created or written. Recorded
/// original paths stay display-only in `Entry.origin`; `Entry.path` always
/// remains the actual trash content path, and unreadable or malformed
/// metadata yields an explicit warning while the item itself is kept.
/// Entries sort by their actual trash file names.
pub fn read_trash(root: &Path) -> io::Result<Listing> {
    let files_dir = root.join("files");
    match fs::metadata(&files_dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Listing::default());
        }
        Err(error) => return Err(error),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "trash files location {} is not a directory",
                    files_dir.to_string_lossy()
                ),
            ));
        }
        Ok(_) => {}
    }
    let info_dir = root.join("info");
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    for found in fs::read_dir(&files_dir)? {
        let item = match found {
            Ok(item) => item,
            Err(error) => {
                warnings.push(format!("trash entry could not be read: {error}"));
                continue;
            }
        };
        let name = item.file_name();
        let item_path = files_dir.join(&name);
        let file_type = match item.file_type() {
            Ok(file_type) => Some(file_type),
            Err(error) => {
                warnings.push(format!(
                    "type of trash entry {} is unavailable: {error}",
                    name.to_string_lossy()
                ));
                None
            }
        };
        let metadata = match item.metadata() {
            Ok(metadata) => Some(metadata),
            Err(error) => {
                warnings.push(format!(
                    "metadata of trash entry {} is unavailable: {error}",
                    name.to_string_lossy()
                ));
                None
            }
        };
        let (kind, is_symlink) = match file_type {
            Some(file_type) if file_type.is_symlink() => (EntryKind::Symlink, true),
            Some(file_type) if file_type.is_dir() => (EntryKind::Directory, false),
            Some(file_type) if file_type.is_file() => (EntryKind::File, false),
            _ => (EntryKind::Other, false),
        };
        // Directory entries carry no meaningful file size, matching the
        // shared scanner contract for the kind.
        let size = if kind == EntryKind::Directory {
            None
        } else {
            metadata.as_ref().map(|metadata| metadata.len())
        };
        let modified = metadata.and_then(|metadata| metadata.modified().ok());

        // The info file name keeps its full name and appends the extension.
        let mut info_name_bytes = name.as_bytes().to_vec();
        info_name_bytes.extend_from_slice(b".trashinfo");
        let info_path = info_dir.join(OsStr::from_bytes(&info_name_bytes));
        let mut origin = None;
        let mut recorded_name = None;
        match fs::read(&info_path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                warnings.push(format!("missing trash info for {}", name.to_string_lossy()));
            }
            Err(error) => {
                warnings.push(format!(
                    "trash info for {} could not be read: {error}",
                    name.to_string_lossy()
                ));
            }
            Ok(bytes) => match parse_trashinfo(&String::from_utf8_lossy(&bytes)) {
                Err(reason) => warnings.push(format!(
                    "unreadable trash info for {}: {reason}",
                    name.to_string_lossy()
                )),
                Ok(recorded_path) => {
                    if let Some(base) = recorded_path.file_name().filter(|base| !base.is_empty()) {
                        recorded_name = Some(base.to_owned());
                    }
                    origin = Some(recorded_path);
                }
            },
        }

        let entry_name = recorded_name.unwrap_or_else(|| name.clone());
        entries.push((
            name,
            Entry {
                name: entry_name,
                path: item_path,
                kind,
                is_symlink,
                size,
                modified,
                origin,
            },
        ));
    }
    entries.sort_by(|(left, _), (right, _)| left.as_bytes().cmp(right.as_bytes()));
    warnings.sort();
    Ok(Listing {
        entries: entries.into_iter().map(|(_, entry)| entry).collect(),
        warnings,
    })
}

/// Where trashed files go and how their original paths are recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrashKind {
    /// `$XDG_DATA_HOME/Trash`: absolute original paths.
    Home,
    /// `$topdir/.Trash-$uid` on another mount: original paths are recorded
    /// relative to this topdir.
    Topdir(PathBuf),
}

/// A trash directory (`files/` + `info/`) chosen for a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashTarget {
    pub root: PathBuf,
    pub kind: TrashKind,
}

impl TrashTarget {
    pub fn home(root: PathBuf) -> TrashTarget {
        TrashTarget {
            root,
            kind: TrashKind::Home,
        }
    }
}

/// Why a trash operation failed.
#[derive(Debug)]
pub enum TrashError {
    /// No usable trash for the file: the UI may offer permanent deletion,
    /// but only behind an explicit confirmation. Nothing was deleted.
    NoTrash(String),
    /// Malformed or hostile trash metadata, or a bad argument.
    Invalid(String),
    Io(io::Error),
}

impl TrashError {
    /// True when restoring found the original path taken.
    pub fn is_already_exists(&self) -> bool {
        matches!(self, TrashError::Io(error) if error.kind() == io::ErrorKind::AlreadyExists)
    }
}

impl fmt::Display for TrashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrashError::NoTrash(reason) => write!(f, "no usable trash: {reason}"),
            TrashError::Invalid(reason) => write!(f, "invalid trash data: {reason}"),
            TrashError::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for TrashError {}

impl From<io::Error> for TrashError {
    fn from(error: io::Error) -> Self {
        TrashError::Io(error)
    }
}

/// What undo, restore and delete need to know about one trashed item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashedItem {
    pub root: PathBuf,
    /// The name under `files/` (and `info/<name>.trashinfo`).
    pub name: OsString,
    /// Absolute original path.
    pub original: PathBuf,
}

impl TrashedItem {
    pub fn files_path(&self) -> PathBuf {
        self.root.join("files").join(&self.name)
    }

    pub fn info_path(&self) -> PathBuf {
        info_path_for(&self.root, &self.name)
    }

    /// Reads the item `name` of `target` from its `.trashinfo`, resolving
    /// topdir-relative paths. Relative paths with `..` are rejected.
    pub fn load(target: &TrashTarget, name: &OsStr) -> Result<TrashedItem, TrashError> {
        validate_item_name(name)?;
        let bytes = fs::read(info_path_for(&target.root, name))?;
        let recorded =
            parse_trashinfo(&String::from_utf8_lossy(&bytes)).map_err(TrashError::Invalid)?;
        let original = if recorded.is_absolute() {
            recorded
        } else {
            match &target.kind {
                TrashKind::Topdir(topdir)
                    if recorded
                        .components()
                        .all(|part| matches!(part, std::path::Component::Normal(_))) =>
                {
                    topdir.join(recorded)
                }
                _ => {
                    return Err(TrashError::Invalid(format!(
                        "unusable relative original path {}",
                        recorded.display()
                    )));
                }
            }
        };
        Ok(TrashedItem {
            root: target.root.clone(),
            name: name.to_owned(),
            original,
        })
    }
}

/// How `restore` treats an original path that is taken again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreConflict {
    /// Restore under a unique "name (2)" next to the original.
    KeepBoth,
    /// Fail with `AlreadyExists`, changing nothing.
    Fail,
}

/// Result of emptying a trash: failed items stay in the trash.
#[derive(Debug, Default)]
pub struct EmptyReport {
    pub removed: usize,
    pub failures: Vec<(PathBuf, io::Error)>,
}

/// Percent-encodes bytes for a `.trashinfo` `Path`: RFC 3986 unreserved
/// characters and `/` stay, everything else becomes `%XX`.
pub fn encode_percent(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len());
    for &byte in input {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Longest `files/` name whose `<name>.trashinfo` still fits 255 bytes.
const MAX_TRASH_NAME_BYTES: usize = 255 - ".trashinfo".len();

fn info_path_for(root: &Path, name: &OsStr) -> PathBuf {
    let mut file = name.as_bytes().to_vec();
    file.extend_from_slice(b".trashinfo");
    root.join("info").join(OsStr::from_bytes(&file))
}

/// An item name must be one plain path component: restore and delete act
/// on `files/<name>` and must never be steered outside the trash.
fn validate_item_name(name: &OsStr) -> Result<(), TrashError> {
    validate_name(name).map_err(|error| TrashError::Invalid(format!("item name: {error}")))
}

fn create_private_dir_all(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Chooses the trash for `path` (freedesktop Trash spec 1.0): the home
/// trash when `path` lives on `home_dev`, else `$topdir/.Trash-$uid` for the
/// mount `mount_topdir_of` names, created with mode 0700. The shared
/// `$topdir/.Trash` sticky-directory variant is not used. When no trash is
/// usable the result is `TrashError::NoTrash`: the caller may then offer
/// permanent deletion behind an explicit confirmation; nothing is deleted
/// here.
pub fn trash_dir_for(
    path: &Path,
    home_trash: &Path,
    home_dev: u64,
    uid: u32,
    mount_topdir_of: impl Fn(&Path) -> Option<PathBuf>,
) -> Result<TrashTarget, TrashError> {
    // The link itself lives where it is, whatever it points to.
    let device = fs::symlink_metadata(path)?.dev();
    if device == home_dev {
        return Ok(TrashTarget::home(home_trash.to_path_buf()));
    }
    let topdir = mount_topdir_of(path).ok_or_else(|| {
        TrashError::NoTrash(format!("no mount point found for {}", path.display()))
    })?;
    if !path.starts_with(&topdir) {
        return Err(TrashError::NoTrash(format!(
            "{} is not under its mount point {}",
            path.display(),
            topdir.display()
        )));
    }
    let top_meta = fs::metadata(&topdir)
        .map_err(|error| TrashError::NoTrash(format!("{}: {error}", topdir.display())))?;
    if top_meta.dev() != device {
        return Err(TrashError::NoTrash(format!(
            "{} is not the mount point of {}",
            topdir.display(),
            path.display()
        )));
    }
    let root = topdir.join(format!(".Trash-{uid}"));
    match fs::symlink_metadata(&root) {
        // A symlink reports as non-directory here, so it is refused: a
        // trash that redirects elsewhere is not trusted.
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(TrashError::NoTrash(format!(
                "{} exists and is not a directory",
                root.display()
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&root)
                .map_err(|error| {
                    TrashError::NoTrash(format!("cannot create {}: {error}", root.display()))
                })?;
        }
        Err(error) => {
            return Err(TrashError::NoTrash(format!("{}: {error}", root.display())));
        }
    }
    Ok(TrashTarget {
        root,
        kind: TrashKind::Topdir(topdir),
    })
}

/// Moves `path` (a link is moved as a link) into `target`, recording where
/// it came from and when (`now` is local time). The `.trashinfo` is created
/// first with `O_EXCL`, under a unique name on collision, and is removed
/// again when the move into `files/` fails. `path` must be absolute.
pub fn put(
    path: &Path,
    target: &TrashTarget,
    now: NaiveDateTime,
) -> Result<TrashedItem, TrashError> {
    if !path.is_absolute() {
        return Err(TrashError::Invalid(format!(
            "{} is not an absolute path",
            path.display()
        )));
    }
    let base = path
        .file_name()
        .ok_or_else(|| TrashError::Invalid(format!("{} has no file name", path.display())))?;
    let recorded = match &target.kind {
        TrashKind::Home => path,
        TrashKind::Topdir(topdir) => path.strip_prefix(topdir).map_err(|_| {
            TrashError::Invalid(format!(
                "{} is not under {}",
                path.display(),
                topdir.display()
            ))
        })?,
    };
    let content = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        encode_percent(recorded.as_os_str().as_bytes()),
        now.format("%Y-%m-%dT%H:%M:%S")
    );
    let files_dir = target.root.join("files");
    create_private_dir_all(&files_dir)?;
    create_private_dir_all(&target.root.join("info"))?;

    let is_dir = fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir());
    for attempt in 1..=10_000u32 {
        // The `.trashinfo` name must fit too, so the trash name may be a
        // shortened form of the original (`Path=` stays exact).
        let name = if attempt == 1 {
            fit_name(base, "", is_dir, MAX_TRASH_NAME_BYTES)
        } else {
            fit_name(base, &format!(" ({attempt})"), is_dir, MAX_TRASH_NAME_BYTES)
        };
        let info_path = info_path_for(&target.root, &name);
        let mut info = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&info_path)
        {
            Ok(info) => info,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };
        let written = io::Write::write_all(&mut info, content.as_bytes());
        drop(info);
        let moved = written.and_then(|()| rename_noreplace(path, &files_dir.join(&name)));
        match moved {
            Ok(()) => {
                return Ok(TrashedItem {
                    root: target.root.clone(),
                    name,
                    original: path.to_path_buf(),
                });
            }
            Err(error) => {
                fs::remove_file(&info_path).ok();
                if error.kind() == io::ErrorKind::AlreadyExists {
                    // `files/<name>` is taken (e.g. by an item without
                    // info): try the next name.
                    continue;
                }
                return Err(error.into());
            }
        }
    }
    Err(TrashError::Io(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free name in the trash",
    )))
}

/// Moves a trashed item back to its original path, creating missing parent
/// directories (left in place if the move then fails), and removes its
/// `.trashinfo` only after the move succeeded. If the original path is
/// taken, `KeepBoth` restores under a unique name and `Fail` returns
/// `AlreadyExists`; nothing is ever replaced. The move is a rename, so a
/// restore across devices fails with `EXDEV` and changes nothing.
pub fn restore(item: &TrashedItem, conflict: RestoreConflict) -> Result<PathBuf, TrashError> {
    validate_item_name(&item.name)?;
    let name = item
        .original
        .file_name()
        .ok_or_else(|| TrashError::Invalid("original path has no file name".to_owned()))?;
    let parent = item
        .original
        .parent()
        .ok_or_else(|| TrashError::Invalid("original path has no parent".to_owned()))?;
    fs::create_dir_all(parent)?;
    let destination = match conflict {
        RestoreConflict::Fail => item.original.clone(),
        RestoreConflict::KeepBoth => parent.join(unique_name(parent, name)?),
    };
    // NOREPLACE keeps `Fail` honest even if the path appears meanwhile.
    rename_noreplace(&item.files_path(), &destination)?;
    // The item is back; a leftover info file is only stale metadata.
    fs::remove_file(item.info_path()).ok();
    Ok(destination)
}

/// Removes one trashed item for good: its `files/` entry (directories
/// recursively, symlinks never followed) and then its `.trashinfo`. When the
/// content cannot be removed the info is kept so the item stays listed.
pub fn delete_permanently(item: &TrashedItem) -> Result<(), TrashError> {
    validate_item_name(&item.name)?;
    remove_item(&item.root, &item.name)?;
    Ok(())
}

fn remove_item(root: &Path, name: &OsStr) -> io::Result<()> {
    let content = root.join("files").join(name);
    match fs::symlink_metadata(&content) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(&content)?,
        Ok(_) => fs::remove_file(&content)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    match fs::remove_file(info_path_for(root, name)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Permanently removes everything in the trash at `trash_root`, item by
/// item. Items that cannot be removed are reported and stay (with their
/// info); orphan `.trashinfo` files without content are removed too.
pub fn empty(trash_root: &Path) -> Result<EmptyReport, TrashError> {
    let files_dir = trash_root.join("files");
    let mut report = EmptyReport::default();
    match fs::symlink_metadata(&files_dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(report),
        Err(error) => return Err(error.into()),
        Ok(meta) if !meta.is_dir() => {
            return Err(TrashError::Invalid(format!(
                "{} is not a directory",
                files_dir.display()
            )));
        }
        Ok(_) => {}
    }
    for found in fs::read_dir(&files_dir)? {
        let name = match found {
            Ok(found) => found.file_name(),
            Err(error) => {
                report.failures.push((files_dir.clone(), error));
                continue;
            }
        };
        match remove_item(trash_root, &name) {
            Ok(()) => report.removed += 1,
            Err(error) => report.failures.push((files_dir.join(&name), error)),
        }
    }
    // Info files whose content is gone.
    if let Ok(infos) = fs::read_dir(trash_root.join("info")) {
        for info in infos.flatten() {
            let file_name = info.file_name();
            let Some(stem) = file_name.as_bytes().strip_suffix(b".trashinfo") else {
                continue;
            };
            let content = files_dir.join(OsStr::from_bytes(stem));
            if fs::symlink_metadata(&content).is_err_and(|e| e.kind() == io::ErrorKind::NotFound) {
                fs::remove_file(info.path()).ok();
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs as unix_fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

    /// Unique scratch directory outside the repository, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(prefix: &str) -> io::Result<Scratch> {
            let tick = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
                + SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed) as u128;
            let dir = std::env::temp_dir().join(format!(
                "fileflux-trash-{prefix}-{}-{tick}",
                std::process::id()
            ));
            fs::create_dir_all(&dir)?;
            Ok(Scratch(dir))
        }

        fn trash(&self) -> PathBuf {
            self.0.join("Trash")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    /// The entry at that position, for assertions.
    fn entry_at(listing: &Listing, position: usize) -> Result<&Entry, Box<dyn std::error::Error>> {
        listing
            .entries
            .get(position)
            .ok_or_else(|| Box::<dyn std::error::Error>::from(format!("no entry at {position}")))
    }

    /// A missing home Trash is an empty listing, and nothing is created.
    #[test]
    fn missing_trash_is_empty_without_writes() -> TestResult {
        let home = Scratch::new("missing")?;
        let trash = home.trash();

        let listing = read_trash(&trash)?;

        assert!(listing.entries.is_empty());
        assert!(listing.warnings.is_empty());
        assert!(!trash.exists(), "no trash directories were created");
        Ok(())
    }

    /// A mixed fixture: sorted entries, real metadata, the percent-encoded
    /// original path surfaced as display-only origin and name, symlink and
    /// directory kinds (directories carry no size), plus a warning for an
    /// item without trash info.
    #[test]
    fn mixed_fixture_reads_entries_metadata_and_origin() -> TestResult {
        let home = Scratch::new("mixed")?;
        let trash = home.trash();
        fs::create_dir_all(trash.join("files"))?;
        fs::create_dir_all(trash.join("info"))?;
        fs::write(trash.join("files/note.txt"), b"hello")?;
        fs::create_dir_all(trash.join("files/photos"))?;
        unix_fs::symlink("note.txt", trash.join("files/link"))?;
        fs::write(
            trash.join("info/note.txt.trashinfo"),
            "[Trash Info]\nPath=/home/fixture/Documents%2FOld%20notes.txt\nDeletionDate=20240102T030405\n",
        )?;
        fs::write(
            trash.join("info/photos.trashinfo"),
            "[Trash Info]\nPath=/home/fixture/Pictures\n",
        )?;

        let listing = read_trash(&trash)?;

        assert_eq!(listing.entries.len(), 3);
        assert_eq!(listing.warnings.len(), 1, "only link lacks info");
        let warning = listing.warnings.first().ok_or("expected a link warning")?;
        assert!(warning.contains("link"));

        let link = entry_at(&listing, 0)?;
        assert_eq!(link.name, "link");
        assert_eq!(link.path, trash.join("files/link"));
        assert_eq!(link.kind, EntryKind::Symlink);
        assert!(link.is_symlink);
        assert_eq!(link.origin, None);

        let note = entry_at(&listing, 1)?;
        assert_eq!(note.kind, EntryKind::File);
        assert!(!note.is_symlink);
        assert_eq!(note.size, Some(5));
        assert!(note.modified.is_some());
        assert_eq!(
            note.origin,
            Some(PathBuf::from("/home/fixture/Documents/Old notes.txt"))
        );
        // The original file name is shown when the metadata provides one.
        assert_eq!(note.name, "Old notes.txt");
        // The navigation path is always the actual trash content path.
        assert_eq!(note.path, trash.join("files/note.txt"));

        let photos = entry_at(&listing, 2)?;
        assert_eq!(photos.kind, EntryKind::Directory);
        assert!(!photos.is_symlink);
        assert_eq!(photos.size, None, "directory entries carry no size");
        assert!(photos.modified.is_some());
        assert_eq!(photos.origin, Some(PathBuf::from("/home/fixture/Pictures")));
        assert_eq!(photos.name, "Pictures");
        assert_eq!(photos.path, trash.join("files/photos"));
        Ok(())
    }

    /// Missing, wrong-header and Path-less trash info each warn explicitly
    /// while the actual items are kept, and reading never writes anything.
    #[test]
    fn missing_or_malformed_info_warns_and_retains_items() -> TestResult {
        let home = Scratch::new("malformed")?;
        let trash = home.trash();
        fs::create_dir_all(trash.join("files"))?;
        fs::create_dir_all(trash.join("info"))?;
        fs::write(trash.join("files/orphan.bin"), b"x")?;
        fs::write(trash.join("files/wrong.txt"), b"x")?;
        fs::write(trash.join("files/nopath.log"), b"x")?;
        fs::write(
            trash.join("info/wrong.txt.trashinfo"),
            "[Other Info]\nPath=/somewhere\n",
        )?;
        fs::write(
            trash.join("info/nopath.log.trashinfo"),
            "[Trash Info]\nDeletionDate=20240102T030405\n",
        )?;
        let files_before = fs::read_dir(trash.join("files"))?.count();

        let listing = read_trash(&trash)?;

        assert_eq!(listing.entries.len(), 3);
        assert_eq!(listing.warnings.len(), 3);
        assert!(listing.warnings.iter().any(|w| w.contains("orphan.bin")));
        assert!(listing.warnings.iter().any(|w| w.contains("wrong.txt")));
        assert!(listing.warnings.iter().any(|w| w.contains("nopath.log")));
        for entry in &listing.entries {
            assert_eq!(entry.origin, None);
            assert_eq!(
                entry.path,
                trash.join("files").join(&entry.name),
                "path must stay the trash content path"
            );
        }
        assert_eq!(
            fs::read_dir(trash.join("files"))?.count(),
            files_before,
            "reading wrote nothing"
        );
        Ok(())
    }

    /// A hostile recorded path is never opened or used for navigation.
    #[test]
    fn entry_path_is_never_the_original_location() -> TestResult {
        let home = Scratch::new("hostile")?;
        let trash = home.trash();
        fs::create_dir_all(trash.join("files"))?;
        fs::create_dir_all(trash.join("info"))?;
        fs::write(trash.join("files/victim.dat"), b"x")?;
        fs::write(
            trash.join("info/victim.dat.trashinfo"),
            "[Trash Info]\nPath=../../../etc/passwd\n",
        )?;

        let listing = read_trash(&trash)?;

        assert_eq!(listing.warnings.len(), 0);
        assert_eq!(listing.entries.len(), 1);
        let victim = entry_at(&listing, 0)?;
        assert_eq!(victim.path, trash.join("files/victim.dat"));
        // Display-only origin is kept unopened, name comes from it.
        assert_eq!(victim.origin, Some(PathBuf::from("../../../etc/passwd")));
        assert_eq!(victim.name, "passwd");
        Ok(())
    }

    /// Non-UTF8 names keep their exact bytes, the info file name is found by
    /// appending `.trashinfo` (not by replacing the last suffix), and the
    /// percent-decoded origin can itself be non-UTF8.
    #[test]
    fn non_utf8_names_preserved_and_info_appends_extension() -> TestResult {
        let home = Scratch::new("nonutf8")?;
        let trash = home.trash();
        fs::create_dir_all(trash.join("files"))?;
        fs::create_dir_all(trash.join("info"))?;
        let raw_name: &[u8] = b"caf\xe9-report.tar.gz";
        fs::write(
            trash
                .join("files")
                .join(Path::new(OsStr::from_bytes(raw_name))),
            b"x",
        )?;
        fs::write(
            trash.join("info").join(Path::new(OsStr::from_bytes(
                b"caf\xe9-report.tar.gz.trashinfo",
            ))),
            // %E9 decodes to the raw \xe9 byte; UTF-8 names would use %C3%A9.
            "[Trash Info]\nPath=/home/fixture/caf%E9-report.tar.gz\n",
        )?;

        let listing = read_trash(&trash)?;

        assert!(listing.warnings.is_empty());
        assert_eq!(listing.entries.len(), 1);
        let entry = entry_at(&listing, 0)?;
        assert_eq!(entry.name.as_encoded_bytes(), raw_name);
        let expected_path = trash
            .join("files")
            .join(Path::new(OsStr::from_bytes(raw_name)));
        assert_eq!(
            entry.path.as_os_str().as_bytes(),
            expected_path.as_os_str().as_bytes()
        );
        assert_eq!(
            entry.origin,
            Some(PathBuf::from(OsString::from_vec(
                b"/home/fixture/caf\xe9-report.tar.gz".to_vec()
            )))
        );
        Ok(())
    }
}

#[cfg(test)]
mod write_tests {
    use super::*;
    use chrono::NaiveDate;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// Scratch directory holding a fake home, trash and mount; removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(prefix: &str) -> io::Result<Scratch> {
            let tick = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            let dir = std::env::temp_dir().join(format!(
                "fileflux-trashw-{prefix}-{}-{tick}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir)?;
            Ok(Scratch(dir))
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        fn target(&self) -> TrashTarget {
            TrashTarget::home(self.path("Trash"))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn when() -> Result<NaiveDateTime, Box<dyn std::error::Error>> {
        NaiveDate::from_ymd_opt(2024, 3, 9)
            .and_then(|date| date.and_hms_opt(7, 5, 3))
            .ok_or_else(|| "bad fixture date".into())
    }

    fn names(dir: &Path) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let mut found = Vec::new();
        for entry in fs::read_dir(dir)? {
            found.push(entry?.file_name().to_string_lossy().into_owned());
        }
        found.sort();
        Ok(found)
    }

    fn trash_file(
        scratch: &Scratch,
        rel: &str,
        body: &[u8],
    ) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let path = scratch.path(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, body)?;
        Ok(path)
    }

    #[test]
    fn encode_percent_round_trips_and_keeps_slashes() {
        let raw: &[u8] = b"/home/u/My Docs/caf\xc3\xa9 100%#.txt";
        let encoded = encode_percent(raw);
        assert_eq!(encoded, "/home/u/My%20Docs/caf%C3%A9%20100%25%23.txt");
        assert_eq!(decode_percent(encoded.as_bytes()).as_deref(), Some(raw));
        let odd = b"a\xe9b";
        assert_eq!(
            decode_percent(encode_percent(odd).as_bytes()).as_deref(),
            Some(odd.as_slice())
        );
        assert_eq!(encode_percent(b"safe-_.~/Name9"), "safe-_.~/Name9");
    }

    #[test]
    fn put_writes_files_and_info_with_spec_content() -> TestResult {
        let scratch = Scratch::new("put")?;
        let file = trash_file(&scratch, "home/My Docs/report v1.txt", b"data")?;
        let target = scratch.target();

        let item = put(&file, &target, when()?)?;

        assert!(!file.exists(), "the file left its place");
        assert_eq!(item.name, "report v1.txt");
        assert_eq!(item.original, file);
        assert_eq!(item.files_path(), target.root.join("files/report v1.txt"));
        assert_eq!(fs::read(item.files_path())?, b"data");
        let info = fs::read_to_string(target.root.join("info/report v1.txt.trashinfo"))?;
        let expected = format!(
            "[Trash Info]\nPath={}\nDeletionDate=2024-03-09T07:05:03\n",
            file.to_string_lossy().replace(' ', "%20")
        );
        assert_eq!(info, expected);
        assert_eq!(
            item.info_path(),
            target.root.join("info/report v1.txt.trashinfo")
        );
        // The existing read side sees it, with the original path.
        let listing = read_trash(&target.root)?;
        assert!(listing.warnings.is_empty());
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(
            listing.entries.first().and_then(|e| e.origin.clone()),
            Some(file)
        );
        Ok(())
    }

    #[test]
    fn put_shortens_names_whose_info_file_would_exceed_255_bytes() -> TestResult {
        let scratch = Scratch::new("put-long")?;
        for len in [246usize, 253, 255] {
            let name = format!("{}.txt", "T".repeat(len - 4));
            let file = trash_file(&scratch, &format!("home/{len}/{name}"), b"data")?;
            let target = scratch.target();

            let item = put(&file, &target, when()?)?;

            assert!(!file.exists(), "{len}: trashed");
            assert_eq!(item.original, file, "{len}: Path stays exact");
            assert!(item.name.len() + ".trashinfo".len() <= 255, "{len}");
            assert_eq!(fs::read(item.files_path())?, b"data");
            let info = fs::read_to_string(item.info_path())?;
            assert!(info.contains(&format!("Path={}", file.to_string_lossy())));
            let restored = restore(&item, RestoreConflict::Fail)?;
            assert_eq!(restored, file, "{len}");
            assert_eq!(fs::read(&file)?, b"data");
        }
        Ok(())
    }

    #[test]
    fn restore_keep_both_works_for_a_253_byte_name() -> TestResult {
        let scratch = Scratch::new("restore-long")?;
        let name = "R".repeat(253);
        let file = trash_file(&scratch, &format!("home/{name}"), b"old")?;
        let item = put(&file, &scratch.target(), when()?)?;
        fs::write(&file, b"newer")?;

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            tx.send(restore(&item, RestoreConflict::KeepBoth)).ok();
        });
        let restored = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| "restore hung")??;

        assert!(restored.file_name().is_some_and(|n| n.len() <= 255));
        assert_ne!(restored, file);
        assert_eq!(fs::read(&restored)?, b"old");
        assert_eq!(fs::read(&file)?, b"newer");
        Ok(())
    }

    #[test]
    fn put_creates_private_trash_directories() -> TestResult {
        let scratch = Scratch::new("put-modes")?;
        let file = trash_file(&scratch, "home/a.txt", b"x")?;
        let target = scratch.target();
        put(&file, &target, when()?)?;
        for dir in ["files", "info"] {
            let mode = fs::metadata(target.root.join(dir))?.mode() & 0o777;
            assert_eq!(mode, 0o700, "{dir}");
        }
        Ok(())
    }

    #[test]
    fn put_same_name_twice_keeps_both() -> TestResult {
        let scratch = Scratch::new("put-collide")?;
        let first = trash_file(&scratch, "home/a/note.txt", b"one")?;
        let second = trash_file(&scratch, "home/b/note.txt", b"two")?;
        let target = scratch.target();

        let one = put(&first, &target, when()?)?;
        let two = put(&second, &target, when()?)?;

        assert_ne!(one.name, two.name);
        assert_eq!(two.name, "note (2).txt");
        assert_eq!(fs::read(one.files_path())?, b"one");
        assert_eq!(fs::read(two.files_path())?, b"two");
        let info = fs::read_to_string(two.info_path())?;
        assert!(info.contains(&second.to_string_lossy().into_owned()));
        assert_eq!(names(&target.root.join("info"))?.len(), 2);
        Ok(())
    }

    #[test]
    fn put_skips_a_name_already_taken_in_files() -> TestResult {
        let scratch = Scratch::new("put-orphan")?;
        let target = scratch.target();
        fs::create_dir_all(target.root.join("files"))?;
        fs::write(target.root.join("files/a.txt"), b"orphan")?;
        let file = trash_file(&scratch, "home/a.txt", b"new")?;

        let item = put(&file, &target, when()?)?;

        assert_eq!(item.name, "a (2).txt");
        assert_eq!(fs::read(target.root.join("files/a.txt"))?, b"orphan");
        assert_eq!(names(&target.root.join("info"))?, ["a (2).txt.trashinfo"]);
        Ok(())
    }

    #[test]
    fn put_failure_removes_the_info_file() -> TestResult {
        let scratch = Scratch::new("put-fail")?;
        let target = scratch.target();
        let missing = scratch.path("home/ghost.txt");
        fs::create_dir_all(scratch.path("home"))?;

        let result = put(&missing, &target, when()?);

        assert!(result.is_err());
        assert!(names(&target.root.join("info"))?.is_empty());
        assert!(names(&target.root.join("files"))?.is_empty());
        Ok(())
    }

    #[test]
    fn put_rejects_relative_paths() -> TestResult {
        let scratch = Scratch::new("put-relative")?;
        let result = put(Path::new("relative.txt"), &scratch.target(), when()?);
        assert!(matches!(result, Err(TrashError::Invalid(_))));
        assert!(!scratch.path("Trash").exists());
        Ok(())
    }

    #[test]
    fn put_on_a_symlink_trashes_the_link_not_the_target() -> TestResult {
        let scratch = Scratch::new("put-link")?;
        let real = trash_file(&scratch, "home/real.txt", b"real")?;
        let dir = scratch.path("home/realdir");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("inner"), b"inner")?;
        let link = scratch.path("home/link.txt");
        let dirlink = scratch.path("home/dirlink");
        symlink(&real, &link)?;
        symlink(&dir, &dirlink)?;
        let target = scratch.target();

        let one = put(&link, &target, when()?)?;
        let two = put(&dirlink, &target, when()?)?;

        assert!(fs::symlink_metadata(&link).is_err());
        assert_eq!(fs::read(&real)?, b"real");
        assert_eq!(fs::read(dir.join("inner"))?, b"inner");
        assert!(fs::symlink_metadata(one.files_path())?.is_symlink());
        assert!(fs::symlink_metadata(two.files_path())?.is_symlink());
        assert_eq!(fs::read_link(one.files_path())?, real);
        Ok(())
    }

    #[test]
    fn put_on_a_directory_moves_the_whole_tree() -> TestResult {
        let scratch = Scratch::new("put-dir")?;
        let file = trash_file(&scratch, "home/Photos/a.jpg", b"jpg")?;
        let dir = scratch.path("home/Photos");
        let item = put(&dir, &scratch.target(), when()?)?;
        assert!(!dir.exists());
        assert_eq!(fs::read(item.files_path().join("a.jpg"))?, b"jpg");
        assert!(!file.exists());
        Ok(())
    }

    #[test]
    fn trash_dir_for_picks_home_on_the_same_device() -> TestResult {
        let scratch = Scratch::new("dir-home")?;
        let file = trash_file(&scratch, "home/a.txt", b"x")?;
        let home_dev = fs::symlink_metadata(&file)?.dev();
        let home_trash = scratch.path("Trash");

        let target = trash_dir_for(&file, &home_trash, home_dev, 1000, |_| None)?;

        assert_eq!(target, TrashTarget::home(home_trash));
        Ok(())
    }

    #[test]
    fn trash_dir_for_uses_topdir_trash_on_another_device() -> TestResult {
        let scratch = Scratch::new("dir-top")?;
        let topdir = scratch.path("mnt");
        let file = trash_file(&scratch, "mnt/stuff/a.txt", b"x")?;
        let other_dev = fs::symlink_metadata(&file)?.dev().wrapping_add(1);
        let seen = std::cell::RefCell::new(None);

        let target = trash_dir_for(&file, &scratch.path("Trash"), other_dev, 1234, |path| {
            *seen.borrow_mut() = Some(path.to_path_buf());
            Some(topdir.clone())
        })?;

        assert_eq!(seen.take(), Some(file.clone()));
        assert_eq!(target.root, topdir.join(".Trash-1234"));
        assert_eq!(target.kind, TrashKind::Topdir(topdir.clone()));
        let meta = fs::symlink_metadata(&target.root)?;
        assert!(meta.is_dir());
        assert_eq!(meta.mode() & 0o777, 0o700);
        assert!(!scratch.path("Trash").exists(), "home trash untouched");

        // Reusing the existing directory works too.
        let again = trash_dir_for(&file, &scratch.path("Trash"), other_dev, 1234, |_| {
            Some(topdir.clone())
        })?;
        assert_eq!(again, target);
        Ok(())
    }

    #[test]
    fn topdir_trash_records_topdir_relative_paths_and_round_trips() -> TestResult {
        let scratch = Scratch::new("top-roundtrip")?;
        let topdir = scratch.path("mnt");
        let file = trash_file(&scratch, "mnt/stuff/My File.txt", b"usb")?;
        let other_dev = fs::symlink_metadata(&file)?.dev().wrapping_add(1);
        let target = trash_dir_for(&file, &scratch.path("Trash"), other_dev, 77, |_| {
            Some(topdir.clone())
        })?;

        let item = put(&file, &target, when()?)?;

        let info = fs::read_to_string(item.info_path())?;
        assert!(info.contains("Path=stuff/My%20File.txt\n"), "{info}");
        assert_eq!(item.original, file, "item carries the absolute path");
        let loaded = TrashedItem::load(&target, &item.name)?;
        assert_eq!(loaded, item);
        assert_eq!(restore(&loaded, RestoreConflict::Fail)?, file);
        assert_eq!(fs::read(&file)?, b"usb");
        Ok(())
    }

    #[test]
    fn trash_dir_for_without_a_usable_trash_is_an_error_not_a_delete() -> TestResult {
        let scratch = Scratch::new("dir-none")?;
        let file = trash_file(&scratch, "mnt/a.txt", b"x")?;
        let other_dev = fs::symlink_metadata(&file)?.dev().wrapping_add(1);

        let none = trash_dir_for(&file, &scratch.path("Trash"), other_dev, 1, |_| None);
        assert!(matches!(none, Err(TrashError::NoTrash(_))));

        // A topdir trash path occupied by a file or a symlink is unusable.
        fs::write(scratch.path("mnt/.Trash-1"), b"squatter")?;
        let squatted = trash_dir_for(&file, &scratch.path("Trash"), other_dev, 1, |_| {
            Some(scratch.path("mnt"))
        });
        assert!(matches!(squatted, Err(TrashError::NoTrash(_))));
        assert!(file.exists());
        Ok(())
    }

    #[test]
    fn load_rejects_hostile_relative_paths() -> TestResult {
        let scratch = Scratch::new("load-hostile")?;
        let target = TrashTarget {
            root: scratch.path("mnt/.Trash-1"),
            kind: TrashKind::Topdir(scratch.path("mnt")),
        };
        fs::create_dir_all(target.root.join("info"))?;
        fs::write(
            target.root.join("info/x.trashinfo"),
            "[Trash Info]\nPath=../../etc/passwd\n",
        )?;
        assert!(matches!(
            TrashedItem::load(&target, OsStr::new("x")),
            Err(TrashError::Invalid(_))
        ));
        let home = scratch.target();
        fs::create_dir_all(home.root.join("info"))?;
        fs::write(
            home.root.join("info/y.trashinfo"),
            "[Trash Info]\nPath=rel/y\n",
        )?;
        assert!(matches!(
            TrashedItem::load(&home, OsStr::new("y")),
            Err(TrashError::Invalid(_))
        ));
        assert!(matches!(
            TrashedItem::load(&home, OsStr::new("../y")),
            Err(TrashError::Invalid(_))
        ));
        Ok(())
    }

    #[test]
    fn restore_round_trip_returns_the_file_and_drops_the_info() -> TestResult {
        let scratch = Scratch::new("restore")?;
        let file = trash_file(&scratch, "home/a b.txt", b"data")?;
        let target = scratch.target();
        let item = put(&file, &target, when()?)?;

        let restored = restore(&item, RestoreConflict::Fail)?;

        assert_eq!(restored, file);
        assert_eq!(fs::read(&file)?, b"data");
        assert!(names(&target.root.join("files"))?.is_empty());
        assert!(names(&target.root.join("info"))?.is_empty());
        Ok(())
    }

    #[test]
    fn restore_recreates_missing_parent_directories() -> TestResult {
        let scratch = Scratch::new("restore-parents")?;
        let file = trash_file(&scratch, "home/deep/er/a.txt", b"data")?;
        let item = put(&file, &scratch.target(), when()?)?;
        fs::remove_dir_all(scratch.path("home/deep"))?;

        let restored = restore(&item, RestoreConflict::Fail)?;

        assert_eq!(fs::read(restored)?, b"data");
        Ok(())
    }

    #[test]
    fn restore_conflict_fail_changes_nothing() -> TestResult {
        let scratch = Scratch::new("restore-fail")?;
        let file = trash_file(&scratch, "home/a.txt", b"old")?;
        let target = scratch.target();
        let item = put(&file, &target, when()?)?;
        fs::write(&file, b"newer")?;

        let error = restore(&item, RestoreConflict::Fail)
            .err()
            .ok_or("restored over")?;

        assert!(error.is_already_exists());
        assert_eq!(fs::read(&file)?, b"newer");
        assert_eq!(fs::read(item.files_path())?, b"old");
        assert!(item.info_path().exists(), "info kept until success");
        Ok(())
    }

    #[test]
    fn restore_conflict_keep_both_uses_a_unique_name() -> TestResult {
        let scratch = Scratch::new("restore-keep")?;
        let file = trash_file(&scratch, "home/a.txt", b"old")?;
        let target = scratch.target();
        let item = put(&file, &target, when()?)?;
        fs::write(&file, b"newer")?;

        let restored = restore(&item, RestoreConflict::KeepBoth)?;

        assert_eq!(restored, scratch.path("home/a (2).txt"));
        assert_eq!(fs::read(&restored)?, b"old");
        assert_eq!(fs::read(&file)?, b"newer");
        assert!(!item.info_path().exists());
        assert!(!item.files_path().exists());
        Ok(())
    }

    #[test]
    fn restore_a_trashed_symlink_restores_the_link() -> TestResult {
        let scratch = Scratch::new("restore-link")?;
        let real = trash_file(&scratch, "home/real.txt", b"real")?;
        let link = scratch.path("home/link");
        symlink(&real, &link)?;
        let item = put(&link, &scratch.target(), when()?)?;

        restore(&item, RestoreConflict::Fail)?;

        assert_eq!(fs::read_link(&link)?, real);
        assert_eq!(fs::read(&real)?, b"real");
        Ok(())
    }

    #[test]
    fn restore_refuses_a_forged_item_name() -> TestResult {
        let scratch = Scratch::new("restore-forged")?;
        let victim = trash_file(&scratch, "victim.txt", b"keep")?;
        let item = TrashedItem {
            root: scratch.path("Trash"),
            name: OsString::from("../../victim.txt"),
            original: scratch.path("elsewhere.txt"),
        };
        assert!(restore(&item, RestoreConflict::Fail).is_err());
        assert!(delete_permanently(&item).is_err());
        assert_eq!(fs::read(&victim)?, b"keep");
        Ok(())
    }

    #[test]
    fn delete_permanently_removes_content_and_info_only_for_that_item() -> TestResult {
        let scratch = Scratch::new("delete")?;
        let outside = trash_file(&scratch, "outside.txt", b"outside")?;
        let dir = scratch.path("home/tree");
        fs::create_dir_all(dir.join("sub"))?;
        fs::write(dir.join("sub/f"), b"f")?;
        symlink(&outside, dir.join("sub/link"))?;
        symlink(scratch.path("home"), dir.join("sub/dirlink"))?;
        let keep = trash_file(&scratch, "home/keep.txt", b"keep")?;
        let target = scratch.target();
        let gone = put(&dir, &target, when()?)?;
        let kept = put(&keep, &target, when()?)?;

        delete_permanently(&gone)?;

        assert!(!gone.files_path().exists());
        assert!(!gone.info_path().exists());
        assert_eq!(fs::read(kept.files_path())?, b"keep");
        assert!(kept.info_path().exists());
        assert_eq!(fs::read(&outside)?, b"outside", "links were not followed");
        assert!(scratch.path("home").exists());
        Ok(())
    }

    #[test]
    fn delete_permanently_of_a_symlink_removes_only_the_link() -> TestResult {
        let scratch = Scratch::new("delete-link")?;
        let real = trash_file(&scratch, "home/real.txt", b"real")?;
        let link = scratch.path("home/link");
        symlink(&real, &link)?;
        let item = put(&link, &scratch.target(), when()?)?;
        delete_permanently(&item)?;
        assert!(fs::symlink_metadata(item.files_path()).is_err());
        assert_eq!(fs::read(&real)?, b"real");
        Ok(())
    }

    #[test]
    fn empty_removes_everything_and_reports_per_item() -> TestResult {
        let scratch = Scratch::new("empty")?;
        let outside = trash_file(&scratch, "outside.txt", b"outside")?;
        let target = scratch.target();
        let a = trash_file(&scratch, "home/a.txt", b"a")?;
        let dir = scratch.path("home/tree");
        fs::create_dir_all(&dir)?;
        symlink(&outside, dir.join("link"))?;
        let link = scratch.path("home/link");
        symlink(&outside, &link)?;
        put(&a, &target, when()?)?;
        put(&dir, &target, when()?)?;
        put(&link, &target, when()?)?;
        // An orphan info file with no content is cleaned up too.
        fs::write(
            target.root.join("info/orphan.trashinfo"),
            "[Trash Info]\nPath=/x\n",
        )?;

        let report = empty(&target.root)?;

        assert_eq!(report.removed, 3);
        assert!(report.failures.is_empty());
        assert!(names(&target.root.join("files"))?.is_empty());
        assert!(names(&target.root.join("info"))?.is_empty());
        assert_eq!(fs::read(&outside)?, b"outside");
        Ok(())
    }

    #[test]
    fn empty_reports_items_it_could_not_remove_and_keeps_their_info() -> TestResult {
        let scratch = Scratch::new("empty-fail")?;
        let target = scratch.target();
        let ok = trash_file(&scratch, "home/ok.txt", b"ok")?;
        let stuck_dir = scratch.path("home/stuck");
        fs::create_dir_all(stuck_dir.join("locked"))?;
        fs::write(stuck_dir.join("locked/f"), b"f")?;
        put(&ok, &target, when()?)?;
        let stuck = put(&stuck_dir, &target, when()?)?;
        let locked = stuck.files_path().join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500))?;

        let report = empty(&target.root)?;

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700))?;
        // Running as root bypasses permission bits; only assert when blocked.
        if report.failures.is_empty() {
            return Ok(());
        }
        assert_eq!(report.removed, 1);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(
            report.failures.first().map(|f| f.0.clone()),
            Some(stuck.files_path())
        );
        assert!(stuck.info_path().exists(), "info stays with its item");
        assert!(names(&target.root.join("files"))? == ["stuck"]);
        Ok(())
    }

    #[test]
    fn empty_of_a_missing_trash_is_a_noop() -> TestResult {
        let scratch = Scratch::new("empty-missing")?;
        let report = empty(&scratch.path("Trash"))?;
        assert_eq!(report.removed, 0);
        assert!(!scratch.path("Trash").exists());
        Ok(())
    }
}
