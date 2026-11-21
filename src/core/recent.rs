//! The recently used files, read-only, from the freedesktop
//! `recently-used.xbel` (`$XDG_DATA_HOME/recently-used.xbel`, fallback
//! `~/.local/share`) that GTK and Qt applications and file choosers share.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read as _};
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

use super::trash::decode_percent;
use super::{Entry, EntryKind, Listing};

/// The most files the Recent view lists.
pub const MAX_RECENT: usize = 200;

/// Where the file lives; `None` without a usable `$XDG_DATA_HOME` or `$HOME`.
#[must_use]
pub fn recent_path(home: Option<&OsStr>, xdg_data_home: Option<&OsStr>) -> Option<PathBuf> {
    let base = xdg_data_home
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .map(Path::to_path_buf)
        .or_else(|| {
            let home = home.filter(|value| !value.is_empty())?;
            Some(Path::new(home).join(".local").join("share"))
        })?;
    Some(base.join(FILE_NAME))
}

const FILE_NAME: &str = "recently-used.xbel";

/// A larger file is read only up to this many bytes (GTK trims it to a few
/// hundred entries; this is a guard against a runaway file).
const READ_LIMIT: u64 = 16 * 1024 * 1024;

/// One bookmark of the file: a local path and when it was last used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Used {
    pub path: PathBuf,
    pub when: Option<SystemTime>,
}

/// What a file's bytes hold.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    /// Newest first, one per path, local `file://` entries only.
    pub used: Vec<Used>,
    /// Why the rest of the file was not read, when it was malformed.
    pub problem: Option<String>,
}

/// Parses the bytes of a `recently-used.xbel`.
#[must_use]
pub fn parse(xbel: &[u8]) -> Parsed {
    let mut reader = Reader::from_reader(xbel);
    // A stray end tag must not stop the list: only the bookmarks matter.
    reader.config_mut().check_end_names = false;
    let mut buffer = Vec::new();
    let mut found: Vec<Used> = Vec::new();
    let mut problem = None;
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(start) | Event::Empty(start))
                if start.local_name().as_ref() == b"bookmark" =>
            {
                if let Some(used) = bookmark(&start, &reader) {
                    found.push(used);
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => {
                problem = Some(format!(
                    "{FILE_NAME} is malformed after {} entries: {error}",
                    found.len()
                ));
                break;
            }
        }
        buffer.clear();
    }
    // The file appends new entries: of two equally recent, the later wins.
    found.reverse();
    found.sort_by_key(|used| std::cmp::Reverse(used.when));
    let mut seen = HashSet::new();
    found.retain(|used| seen.insert(used.path.clone()));
    Parsed {
        used: found,
        problem,
    }
}

/// The local file one `<bookmark>` names and its latest timestamp.
fn bookmark(start: &BytesStart<'_>, reader: &Reader<&[u8]>) -> Option<Used> {
    let mut path = None;
    let mut when: Option<SystemTime> = None;
    for attribute in start.attributes().flatten() {
        let Ok(value) =
            attribute.decoded_and_normalized_value(XmlVersion::Explicit1_0, reader.decoder())
        else {
            continue;
        };
        match attribute.key.as_ref() {
            b"href" => path = local_path(&value),
            b"added" | b"modified" | b"visited" => {
                let stamp = chrono::DateTime::parse_from_rfc3339(value.trim())
                    .ok()
                    .map(SystemTime::from);
                when = when.max(stamp);
            }
            _ => {}
        }
    }
    Some(Used { path: path?, when })
}

/// The absolute local path of a `file://` URI; `None` for any other URI, a
/// remote host, a relative path or a malformed escape.
fn local_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    if !path.starts_with('/') {
        return None;
    }
    let decoded = decode_percent(path.as_bytes())?;
    (!decoded.contains(&0)).then(|| PathBuf::from(OsString::from_vec(decoded)))
}

/// The Recent listing of the file at `file`: [`MAX_RECENT`] existing files.
///
/// # Errors
///
/// An I/O failure other than the file not existing (which lists nothing).
pub fn read_recent(file: &Path) -> io::Result<Listing> {
    read_recent_capped(file, MAX_RECENT)
}

/// [`read_recent`] with another cap.
///
/// # Errors
///
/// See [`read_recent`].
pub fn read_recent_capped(file: &Path, cap: usize) -> io::Result<Listing> {
    let mut bytes = Vec::new();
    match fs::File::open(file) {
        Ok(opened) => {
            opened.take(READ_LIMIT).read_to_end(&mut bytes)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Listing::default()),
        Err(error) => return Err(error),
    }
    let parsed = parse(&bytes);
    let entries = parsed
        .used
        .into_iter()
        .filter_map(|used| entry(used.path, used.when))
        .take(cap)
        .collect();
    Ok(Listing {
        entries,
        warnings: parsed.problem.into_iter().collect(),
        ..Listing::default()
    })
}

/// The entry of a file that still exists: its kind and size as the folder
/// scan reports them, its folder as the origin, and the time it was last
/// used as the modification time, so sorting by date orders the view by use.
fn entry(path: PathBuf, used: Option<SystemTime>) -> Option<Entry> {
    let meta = fs::metadata(&path).ok()?;
    let is_symlink = fs::symlink_metadata(&path).is_ok_and(|link| link.is_symlink());
    let kind = if meta.is_dir() {
        EntryKind::Directory
    } else if meta.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    Some(Entry {
        name: path.file_name()?.to_os_string(),
        origin: path.parent().map(Path::to_path_buf),
        unix: None,
        size: (kind != EntryKind::Directory).then_some(meta.len()),
        modified: used.or_else(|| meta.modified().ok()),
        path,
        kind,
        is_symlink,
    })
}
