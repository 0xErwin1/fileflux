//! Reading the freedesktop thumbnail cache (`~/.cache/thumbnails`), read-only.
//!
//! The file manager next door (Nautilus, Thunar, Dolphin) has usually made a
//! small PNG for every image it listed. Reusing it avoids decoding a
//! multi-megabyte photo for a 62×46 tile. Nothing is ever written here.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::BufReader;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use md5::{Digest as _, Md5};

use super::trash::decode_percent;

/// The cache's size directories, smallest first: the grid wants the cheapest
/// thumbnail that exists.
pub const SIZE_DIRECTORIES: [&str; 3] = ["normal", "large", "x-large"];

/// `$XDG_CACHE_HOME/thumbnails` (fallback `$HOME/.cache/thumbnails`). A
/// relative `$XDG_CACHE_HOME` is ignored, as the XDG specification says.
#[must_use]
pub fn cache_root(home: Option<&OsStr>, xdg_cache_home: Option<&OsStr>) -> Option<PathBuf> {
    let base = xdg_cache_home
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .map(Path::to_path_buf)
        .or_else(|| {
            let home = home.filter(|value| !value.is_empty())?;
            Some(Path::new(home).join(".cache"))
        })?;
    Some(base.join("thumbnails"))
}

/// The URI the cache names a file by: what GLib's `g_filename_to_uri` makes,
/// which is what the thumbnailers of the other file managers hash. The
/// reserved characters that may appear in a path (`!$&'()*+,;=:@`) stay as
/// they are; every other byte is escaped.
#[must_use]
pub fn thumbnail_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/!$&'()*+,;=:@".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// `<md5 of the URI, lowercase hex>.png`.
#[must_use]
pub fn thumbnail_file_name(uri: &str) -> String {
    let digest = Md5::digest(uri.as_bytes());
    let mut name = String::with_capacity(36);
    for byte in digest {
        name.push_str(&format!("{byte:02x}"));
    }
    name.push_str(".png");
    name
}

/// The `Thumb::URI` and `Thumb::MTime` texts of a PNG, from its header
/// chunks alone: the decoder stops before the image data, so no pixel is
/// ever decoded.
fn thumbnail_tags(file: &Path) -> Option<(String, String)> {
    let reader = png::Decoder::new(BufReader::new(File::open(file).ok()?))
        .read_info()
        .ok()?;
    let text = &reader.info().uncompressed_latin1_text;
    let tag = |key: &str| {
        text.iter()
            .find(|chunk| chunk.keyword == key)
            .map(|chunk| chunk.text.clone())
    };
    Some((tag("Thumb::URI")?, tag("Thumb::MTime")?))
}

/// The seconds since the epoch `Thumb::MTime` records.
fn mtime_seconds(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|since| since.as_secs())
}

/// Whether two URIs name the same bytes, whichever characters each escaped.
fn same_uri(a: &str, b: &str) -> bool {
    a == b
        || matches!((decode_percent(a.as_bytes()), decode_percent(b.as_bytes())), (Some(a), Some(b)) if a == b)
}

/// What names `image` in the cache, read from the file as it is now.
struct Source {
    /// The file with its symlinks resolved: what the cache is keyed by.
    canonical: PathBuf,
    uri: String,
    /// The file's own modification time, in seconds.
    modified: u64,
}

impl Source {
    /// `None` when the file cannot be reached.
    fn of(image: &Path) -> Option<Self> {
        let canonical = fs::canonicalize(image).ok()?;
        let modified = mtime_seconds(fs::metadata(&canonical).ok()?.modified().ok()?)?;
        let uri = thumbnail_uri(&canonical);
        Some(Self {
            canonical,
            uri,
            modified,
        })
    }
}

/// The cached thumbnail of `image` under `cache_root`, smallest size first,
/// when one exists and still describes the file as it is now: its
/// `Thumb::URI` is the file's canonical URI and its `Thumb::MTime` the
/// file's modification time. Anything else (no thumbnail, an unreadable or
/// untagged one, a stale one) is `None`, and the caller loads the image
/// itself.
///
/// This stats, canonicalizes and reads files: call it off the UI thread.
#[must_use]
pub fn find(cache_root: &Path, image: &Path) -> Option<PathBuf> {
    find_for(cache_root, image, &Source::of(image)?)
}

fn find_for(cache_root: &Path, image: &Path, source: &Source) -> Option<PathBuf> {
    let name = thumbnail_file_name(&source.uri);
    SIZE_DIRECTORIES.iter().find_map(|size| {
        let candidate = cache_root.join(size).join(&name);
        let (tagged_uri, tagged_mtime) = thumbnail_tags(&candidate)?;
        let fresh = same_uri(&tagged_uri, &source.uri)
            && tagged_mtime.trim().parse() == Ok(source.modified);
        if !fresh {
            log::debug!(
                "thumbnail cache entry {} is stale for {}",
                candidate.display(),
                image.display()
            );
        }
        fresh.then_some(candidate)
    })
}

/// How many answers a [`Memo`] remembers before it starts over.
const MEMO_LIMIT: usize = 4096;

/// A cached thumbnail that was found, with what its file looked like then.
#[derive(Debug)]
struct Hit {
    thumbnail: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

impl Hit {
    fn of(thumbnail: PathBuf) -> Option<Self> {
        let meta = fs::metadata(&thumbnail).ok()?;
        Some(Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            thumbnail,
        })
    }

    /// Whether the cached file is still there as it was found: a thumbnail
    /// another file manager deleted or rewrote must not reach the image
    /// loader.
    fn is_current(&self) -> bool {
        fs::metadata(&self.thumbnail)
            .is_ok_and(|meta| meta.len() == self.len && meta.modified().ok() == self.modified)
    }
}

/// The answers of [`find`], remembered per file and its **own** modification
/// time (the file's, never a "last used" time a listing may show instead) so
/// a repaint or a refresh does not read the cache again. A remembered answer
/// is checked against its cached file before it is handed out; only hits are
/// remembered, so a thumbnail another program makes later is found.
#[derive(Debug, Default)]
pub struct Memo {
    hits: Mutex<HashMap<(PathBuf, u64), Hit>>,
}

impl Memo {
    /// [`find`] through the memory. Blocking, like [`find`].
    #[must_use]
    pub fn lookup(&self, cache_root: &Path, image: &Path) -> Option<PathBuf> {
        let source = Source::of(image)?;
        let key = (source.canonical.clone(), source.modified);
        let remembered = self
            .hits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .map(|hit| (hit.is_current(), hit.thumbnail.clone()));
        match remembered {
            Some((true, thumbnail)) => return Some(thumbnail),
            Some((false, _)) => {
                // Deleted or rewritten since: ask the cache again, and fall
                // back to the image itself when it has nothing current.
                self.hits
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&key);
            }
            None => {}
        }
        let found = find_for(cache_root, image, &source)?;
        let hit = Hit::of(found.clone())?;
        let mut hits = self.hits.lock().unwrap_or_else(PoisonError::into_inner);
        if hits.len() >= MEMO_LIMIT {
            hits.clear();
        }
        hits.insert(key, hit);
        Some(found)
    }
}

/// [`Memo::lookup`] against the process environment's cache and one shared
/// memory. Blocking: the explorer calls it on the background executor, never
/// while rendering.
#[must_use]
pub fn lookup(image: &Path) -> Option<PathBuf> {
    static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
    static MEMO: LazyLock<Memo> = LazyLock::new(Memo::default);
    let root = ROOT
        .get_or_init(|| {
            cache_root(
                std::env::var_os("HOME").as_deref(),
                std::env::var_os("XDG_CACHE_HOME").as_deref(),
            )
        })
        .as_deref()?;
    let found = MEMO.lookup(root, image);
    match &found {
        Some(thumbnail) => log::debug!(
            "using cached thumbnail {} for {}",
            thumbnail.display(),
            image.display()
        ),
        None => log::debug!("no usable cached thumbnail for {}", image.display()),
    }
    found
}
