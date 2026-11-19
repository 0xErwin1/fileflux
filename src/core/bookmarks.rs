//! The favorites, kept where the desktop keeps them: the GTK bookmarks file
//! `$XDG_CONFIG_HOME/gtk-3.0/bookmarks` (fallback `~/.config`) that Nautilus,
//! Thunar, Nemo and every GTK file chooser share.
//!
//! One bookmark per line: `file:///percent-encoded/path Optional Label`.
//! Local entries are the favorites (the label when present, else the folder
//! name). Every other line (`sftp://`, `smb://`, comments, blanks, anything
//! unknown) is kept byte for byte: this module only ever appends a line or
//! drops the lines of the folder being removed, so another program's
//! bookmarks, order and labels survive a change made here.
//!
//! Every change reads the file again, edits those bytes and writes them
//! atomically (see [`super::settings`]: a temporary file beside the target,
//! renamed over it, through a symlink, keeping the file's mode), so an edit
//! made by another program meanwhile is never overwritten. Changes of this
//! process are serialized by one lock.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use super::settings::{write_atomic_with_mode, write_target};

const GTK_DIR: &str = "gtk-3.0";
const BOOKMARKS_FILE: &str = "bookmarks";
/// The mode of a bookmarks file this module creates, GTK's own.
const NEW_FILE_MODE: u32 = 0o644;

/// Serializes the read-edit-write of this process.
static WRITER: Mutex<()> = Mutex::new(());

/// Where the bookmarks file lives; `None` when neither the absolute XDG
/// variable nor `$HOME` is usable (the favorites are then not persisted).
pub fn bookmarks_path(home: Option<&OsStr>, xdg_config_home: Option<&OsStr>) -> Option<PathBuf> {
    let base = xdg_config_home
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .map(Path::to_path_buf)
        .or_else(|| {
            let home = home.filter(|value| !value.is_empty())?;
            Some(Path::new(home).join(".config"))
        })?;
    Some(base.join(GTK_DIR).join(BOOKMARKS_FILE))
}

/// A local bookmark: a favorite folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bookmark {
    pub path: PathBuf,
    /// The text after the URI, when there is one.
    pub label: Option<String>,
}

impl Bookmark {
    /// What the sidebar shows: the label, else the folder's name.
    #[must_use]
    pub fn display_name(&self) -> String {
        self.label.clone().unwrap_or_else(|| {
            self.path.file_name().map_or_else(
                || self.path.to_string_lossy().into_owned(),
                |name| name.to_string_lossy().into_owned(),
            )
        })
    }
}

/// One line of the file with its terminator, as read.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Line {
    raw: Vec<u8>,
    /// Set for a `file://` entry.
    bookmark: Option<Bookmark>,
}

/// A parsed bookmarks file: every line, understood or not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BookmarkFile {
    lines: Vec<Line>,
}

/// The line text without its `\n` or `\r\n`.
fn content(raw: &[u8]) -> &[u8] {
    let raw = raw.strip_suffix(b"\n").unwrap_or(raw);
    raw.strip_suffix(b"\r").unwrap_or(raw)
}

fn hex(byte: u8) -> Option<u8> {
    char::from(byte)
        .to_digit(16)
        .and_then(|digit| u8::try_from(digit).ok())
}

/// The bytes a URI path encodes; a malformed escape stays as written.
fn percent_decode(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut index = 0;
    while let Some(&byte) = text.get(index) {
        let escaped = (byte == b'%')
            .then(|| {
                let high = hex(*text.get(index + 1)?)?;
                let low = hex(*text.get(index + 2)?)?;
                Some(high << 4 | low)
            })
            .flatten();
        match escaped {
            Some(decoded) => {
                out.push(decoded);
                index += 3;
            }
            None => {
                out.push(byte);
                index += 1;
            }
        }
    }
    out
}

/// The local absolute path of a `file://` URI; `None` for any other URI, a
/// remote host or a relative path.
fn local_path(uri: &[u8]) -> Option<PathBuf> {
    let rest = uri.strip_prefix(b"file://")?;
    let path = rest.strip_prefix(b"localhost").unwrap_or(rest);
    if path.first() != Some(&b'/') {
        return None;
    }
    let decoded = percent_decode(path);
    (!decoded.contains(&0)).then(|| PathBuf::from(OsString::from_vec(decoded)))
}

fn parse_line(raw: &[u8]) -> Line {
    let text = content(raw);
    let mut parts = text.splitn(2, |&byte| byte == b' ');
    let uri = parts.next().unwrap_or_default();
    let label = parts.next().unwrap_or_default();
    let label = String::from_utf8_lossy(label).trim().to_owned();
    Line {
        raw: raw.to_vec(),
        bookmark: local_path(uri).map(|path| Bookmark {
            path,
            label: (!label.is_empty()).then_some(label),
        }),
    }
}

/// Parses the file's bytes. Never fails: a line that is not a local bookmark
/// is simply not a favorite.
#[must_use]
pub fn parse(bytes: &[u8]) -> BookmarkFile {
    BookmarkFile {
        lines: bytes
            .split_inclusive(|&byte| byte == b'\n')
            .map(parse_line)
            .collect(),
    }
}

impl BookmarkFile {
    /// The favorites in file order, without repeats of one folder.
    #[must_use]
    pub fn favorites(&self) -> Vec<Bookmark> {
        let mut seen: Vec<Bookmark> = Vec::new();
        for bookmark in self.lines.iter().filter_map(|line| line.bookmark.as_ref()) {
            if !seen.iter().any(|known| known.path == bookmark.path) {
                seen.push(bookmark.clone());
            }
        }
        seen
    }

    fn contains(&self, folder: &Path) -> bool {
        self.lines
            .iter()
            .filter_map(|line| line.bookmark.as_ref())
            .any(|bookmark| bookmark.path == folder)
    }

    /// The line ending the file uses: its last terminated line's.
    fn eol(&self) -> &'static [u8] {
        let crlf = self
            .lines
            .iter()
            .rev()
            .find(|line| line.raw.ends_with(b"\n"))
            .is_some_and(|line| line.raw.ends_with(b"\r\n"));
        if crlf { b"\r\n" } else { b"\n" }
    }

    /// Appends `folder` unless it is a bookmark already; whether it did.
    fn append(&mut self, folder: &Path) -> bool {
        if self.contains(folder) {
            return false;
        }
        let eol = self.eol();
        if let Some(last) = self.lines.last_mut()
            && !last.raw.ends_with(b"\n")
        {
            last.raw.extend_from_slice(eol);
        }
        let mut raw = file_uri(folder).into_bytes();
        raw.extend_from_slice(eol);
        self.lines.push(Line {
            raw,
            bookmark: Some(Bookmark {
                path: folder.to_path_buf(),
                label: None,
            }),
        });
        true
    }

    /// Drops every line of `folder`; whether any went.
    fn drop_folder(&mut self, folder: &Path) -> bool {
        let before = self.lines.len();
        self.lines.retain(|line| {
            line.bookmark
                .as_ref()
                .is_none_or(|bookmark| bookmark.path != folder)
        });
        self.lines.len() != before
    }

    fn to_bytes(&self) -> Vec<u8> {
        self.lines
            .iter()
            .flat_map(|line| line.raw.iter().copied())
            .collect()
    }
}

/// The `file://` URI of `path`: everything but unreserved characters and `/`
/// is percent-escaped byte by byte, so any path (non-UTF-8 included) round
/// trips.
#[must_use]
pub fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

fn read_bytes(path: &Path) -> io::Result<Vec<u8>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

/// The favorites the file at `path` holds; a missing file holds none.
///
/// # Errors
///
/// Any other I/O failure.
pub fn read(path: &Path) -> io::Result<Vec<Bookmark>> {
    Ok(parse(&read_bytes(path)?).favorites())
}

/// Reads the file again, applies `edit` and writes the result when `edit`
/// changed something; returns whether it did.
fn update(path: &Path, edit: impl FnOnce(&mut BookmarkFile) -> bool) -> io::Result<bool> {
    let _lock = WRITER.lock().unwrap_or_else(PoisonError::into_inner);
    let mut file = parse(&read_bytes(path)?);
    if !edit(&mut file) {
        return Ok(false);
    }
    let mode = std::fs::metadata(write_target(path)?)
        .map_or(NEW_FILE_MODE, |meta| meta.permissions().mode() & 0o7777);
    write_atomic_with_mode(path, &file.to_bytes(), mode)?;
    Ok(true)
}

/// Adds `folder` as the last bookmark; `false` when it already is one.
///
/// # Errors
///
/// Any I/O failure; the file is left as it was.
pub fn add(path: &Path, folder: &Path) -> io::Result<bool> {
    update(path, |file| file.append(folder))
}

/// Removes every bookmark of `folder`; `false` when there was none (and
/// nothing is written, nothing created).
///
/// # Errors
///
/// Any I/O failure; the file is left as it was.
pub fn remove(path: &Path, folder: &Path) -> io::Result<bool> {
    update(path, |file| file.drop_folder(folder))
}

/// Appends the `folders` that are not bookmarks yet, in order; returns how
/// many it added. This is the one-time move of the favorites that
/// `settings.toml` used to hold.
///
/// # Errors
///
/// Any I/O failure; the file is left as it was.
pub fn merge(path: &Path, folders: &[PathBuf]) -> io::Result<usize> {
    let mut added = 0;
    update(path, |file| {
        for folder in folders {
            added += usize::from(file.append(folder));
        }
        added > 0
    })?;
    Ok(added)
}
