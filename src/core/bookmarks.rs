//! The favorites, kept where the desktop keeps them: the GTK bookmarks file
//! `$XDG_CONFIG_HOME/gtk-3.0/bookmarks` (fallback `~/.config`) that Nautilus,
//! Thunar, Nemo and every GTK file chooser share.
//!
//! One bookmark per line: `file:///percent-encoded/path Optional Label`.
//! Local entries are the favorites (the label when present, else the folder
//! name). The `sftp://`, `smb://`, `ftp://` and `dav://` entries are the
//! network bookmarks (see [`super::network`]); one that carries a password is
//! not listed. Every other line (comments, blanks, anything unknown) is kept
//! byte for byte: this module only ever appends a line or
//! drops the lines of the folder being removed, rewrites the label of one
//! line or moves one line, so another program's
//! bookmarks, order and labels survive a change made here.
//!
//! Every change reads the file again, edits those bytes and writes them
//! atomically (see [`super::settings`]: a temporary file beside the target,
//! renamed over it, through a symlink, keeping the file's mode). Right
//! before the rename the file is read once more: an edit another program made
//! meanwhile is re-applied (the change is made again on its content). The
//! window that remains between that last re-read and the rename is narrowed,
//! not closed: a write landing inside it is still lost. Changes of this
//! process are serialized by one lock.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use super::network::ServerUri;
use super::settings::{write_atomic_guarded, write_target};

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

/// A network bookmark: a server address and the label after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkBookmark {
    pub uri: ServerUri,
    /// The text after the address, when there is one.
    pub title: Option<String>,
}

impl NetworkBookmark {
    /// What the sidebar shows: the title, else the host and path.
    #[must_use]
    pub fn display_name(&self) -> String {
        self.uri.label(self.title.as_deref())
    }
}

/// One line of the file with its terminator, as read.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Line {
    raw: Vec<u8>,
    /// Set for a `file://` entry.
    bookmark: Option<Bookmark>,
    /// Set for a network entry.
    network: Option<NetworkBookmark>,
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
    let bookmark = local_path(uri);
    let network = match bookmark {
        Some(_) => None,
        None => std::str::from_utf8(uri)
            .ok()
            .and_then(|uri| ServerUri::parse(uri).ok())
            .map(|uri| NetworkBookmark {
                uri,
                title: (!label.is_empty()).then(|| label.clone()),
            }),
    };
    Line {
        raw: raw.to_vec(),
        bookmark: bookmark.map(|path| Bookmark {
            path,
            label: (!label.is_empty()).then_some(label),
        }),
        network,
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

    /// The network bookmarks in file order, without repeats of one address.
    #[must_use]
    pub fn network(&self) -> Vec<NetworkBookmark> {
        let mut seen: Vec<NetworkBookmark> = Vec::new();
        for bookmark in self.lines.iter().filter_map(|line| line.network.as_ref()) {
            if !seen.iter().any(|known| known.uri == bookmark.uri) {
                seen.push(bookmark.clone());
            }
        }
        seen
    }

    /// Drops every line of the network bookmark `uri`; whether any went.
    fn drop_network(&mut self, uri: &ServerUri) -> bool {
        let before = self.lines.len();
        self.lines.retain(|line| {
            line.network
                .as_ref()
                .is_none_or(|bookmark| bookmark.uri != *uri)
        });
        self.lines.len() != before
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
            network: None,
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

    /// Sets the label of the first line of `folder` (cleared when `label` is
    /// empty); whether the line changed. The address stays as written and so
    /// does the line's ending; no other line is touched.
    fn relabel(&mut self, folder: &Path, label: &str) -> bool {
        let label = (!label.is_empty()).then(|| label.to_owned());
        let Some(line) = self
            .lines
            .iter_mut()
            .find(|line| line.bookmark.as_ref().is_some_and(|b| b.path == folder))
        else {
            return false;
        };
        let Some(bookmark) = line.bookmark.as_mut() else {
            return false;
        };
        if bookmark.label == label {
            return false;
        }
        let text = content(&line.raw);
        let uri = text.split(|&byte| byte == b' ').next().unwrap_or_default();
        let ending = line.raw.get(text.len()..).unwrap_or_default().to_vec();
        let mut raw = uri.to_vec();
        if let Some(label) = &label {
            raw.push(b' ');
            raw.extend_from_slice(label.as_bytes());
        }
        raw.extend_from_slice(&ending);
        line.raw = raw;
        bookmark.label = label;
        true
    }

    /// Moves the first line of `folder` just before the first line of
    /// `before`, or after the last favorite's line when `before` is `None`;
    /// whether the order of the favorites changed. Every line keeps its bytes, except that the
    /// one that was last and no longer is gets its line ending.
    fn move_folder(&mut self, folder: &Path, before: Option<&Path>) -> bool {
        let position = |wanted: &Path| {
            self.lines
                .iter()
                .position(|line| line.bookmark.as_ref().is_some_and(|b| b.path == wanted))
        };
        let Some(from) = position(folder) else {
            return false;
        };
        let to = match before {
            Some(anchor) if anchor == folder => return false,
            Some(anchor) => match position(anchor) {
                Some(index) => index,
                None => return false,
            },
            None => match self.lines.iter().rposition(|line| line.bookmark.is_some()) {
                Some(last) => last + 1,
                None => return false,
            },
        };
        // Only favorites count: stepping over a comment or a network line
        // would change the file, not the order of the favorites.
        let (low, high) = if to > from {
            (from + 1, to)
        } else {
            (to, from)
        };
        let crosses_favorite = self
            .lines
            .get(low..high)
            .is_some_and(|lines| lines.iter().any(|line| line.bookmark.is_some()));
        if !crosses_favorite {
            return false;
        }
        let eol = self.eol();
        let moved = self.lines.remove(from);
        self.lines
            .insert(if to > from { to - 1 } else { to }, moved);
        let last = self.lines.len().saturating_sub(1);
        for line in self.lines.iter_mut().take(last) {
            if !line.raw.ends_with(b"\n") {
                line.raw.extend_from_slice(eol);
            }
        }
        true
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

/// The network bookmarks the file at `path` holds; a missing file holds none.
///
/// # Errors
///
/// Any other I/O failure.
pub fn read_network(path: &Path) -> io::Result<Vec<NetworkBookmark>> {
    Ok(parse(&read_bytes(path)?).network())
}

/// The favorites and the network bookmarks of the file at `path`, from one
/// read.
///
/// # Errors
///
/// Any I/O failure other than a missing file.
pub fn read_all(path: &Path) -> io::Result<(Vec<Bookmark>, Vec<NetworkBookmark>)> {
    let file = parse(&read_bytes(path)?);
    Ok((file.favorites(), file.network()))
}

/// How many times a change starts over on fresh content before giving up.
const MAX_ATTEMPTS: usize = 5;

/// Reads the file again, applies `edit` and writes the result when `edit`
/// changed something; returns whether it did.
///
/// Right before the rename the file is read once more: when another program
/// changed it since this attempt read it, the attempt is dropped and `edit`
/// is applied to the fresh content (so `edit` may run more than once and must
/// start from nothing each time), up to [`MAX_ATTEMPTS`] times.
/// `before_rename` runs just before that last check.
fn update(
    path: &Path,
    before_rename: &mut dyn FnMut(),
    mut edit: impl FnMut(&mut BookmarkFile) -> bool,
) -> io::Result<bool> {
    let _lock = WRITER.lock().unwrap_or_else(PoisonError::into_inner);
    for _ in 0..MAX_ATTEMPTS {
        let read = read_bytes(path)?;
        let mut file = parse(&read);
        if !edit(&mut file) {
            return Ok(false);
        }
        let mode = std::fs::metadata(write_target(path)?)
            .map_or(NEW_FILE_MODE, |meta| meta.permissions().mode() & 0o7777);
        let written = write_atomic_guarded(path, &file.to_bytes(), mode, || {
            before_rename();
            Ok(read_bytes(path)? == read)
        })?;
        if written {
            return Ok(true);
        }
    }
    Err(io::Error::other(
        "the bookmarks file keeps changing under another program's writes",
    ))
}

fn require_absolute(folder: &Path) -> io::Result<()> {
    if folder.is_absolute() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "bookmark folder {} is not an absolute path",
                folder.display()
            ),
        ))
    }
}

/// Adds `folder` as the last bookmark; `false` when it already is one.
///
/// # Errors
///
/// A relative `folder` (it would be a line no program reads), or any I/O
/// failure; the file is left as it was.
pub fn add(path: &Path, folder: &Path) -> io::Result<bool> {
    add_observed(path, folder, &mut || {})
}

/// [`add`] that calls `before_rename` after the new content is ready and
/// just before the file is checked and replaced: tests use it to play the
/// other program's write.
#[doc(hidden)]
pub fn add_observed(
    path: &Path,
    folder: &Path,
    before_rename: &mut dyn FnMut(),
) -> io::Result<bool> {
    require_absolute(folder)?;
    update(path, before_rename, |file| file.append(folder))
}

/// Removes every bookmark of `folder`; `false` when there was none (and
/// nothing is written, nothing created).
///
/// # Errors
///
/// Any I/O failure; the file is left as it was.
pub fn remove(path: &Path, folder: &Path) -> io::Result<bool> {
    update(path, &mut || {}, |file| file.drop_folder(folder))
}

/// Removes every line of the network bookmark `uri`; `false` when there was
/// none (and nothing is written, nothing created). Other lines stay byte for
/// byte.
///
/// # Errors
///
/// Any I/O failure; the file is left as it was.
pub fn remove_network(path: &Path, uri: &ServerUri) -> io::Result<bool> {
    update(path, &mut || {}, |file| file.drop_network(uri))
}

/// Appends the `folders` that are not bookmarks yet, in order; returns how
/// many it added. This is the one-time move of the favorites that
/// `settings.toml` used to hold.
///
/// # Errors
///
/// A relative folder (nothing is written) or any I/O failure; the file is
/// left as it was.
pub fn merge(path: &Path, folders: &[PathBuf]) -> io::Result<usize> {
    for folder in folders {
        require_absolute(folder)?;
    }
    let mut added = 0;
    update(path, &mut || {}, |file| {
        added = 0;
        for folder in folders {
            added += usize::from(file.append(folder));
        }
        added > 0
    })?;
    Ok(added)
}

/// Sets the label of `folder`'s bookmark (its first line when repeated), or
/// clears it when `label` is empty, which shows the folder's name again;
/// `false` when `folder` is no bookmark or already has that label (nothing
/// is written, nothing created). The label is trimmed. Other lines stay byte
/// for byte.
///
/// # Errors
///
/// A label with a control character (it would break the line), or any I/O
/// failure; the file is left as it was.
pub fn rename(path: &Path, folder: &Path, label: &str) -> io::Result<bool> {
    let label = label.trim();
    if label.chars().any(char::is_control) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a bookmark label cannot hold line breaks or control characters",
        ));
    }
    update(path, &mut || {}, |file| file.relabel(folder, label))
}

/// Moves `folder`'s bookmark (its first line when repeated) just before the
/// bookmark of `before`, or after the last favorite when `before` is `None`;
/// `false` when either is no bookmark or the order would not change (nothing
/// is written). Lines that are not favorites keep their places and bytes.
///
/// # Errors
///
/// Any I/O failure; the file is left as it was.
pub fn reorder(path: &Path, folder: &Path, before: Option<&Path>) -> io::Result<bool> {
    update(path, &mut || {}, |file| file.move_folder(folder, before))
}
