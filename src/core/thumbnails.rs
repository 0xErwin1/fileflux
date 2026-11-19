//! The freedesktop thumbnail cache (`~/.cache/thumbnails`), following the
//! Thumbnail Managing Standard.
//!
//! The file manager next door (Nautilus, Thunar, Dolphin) has usually made a
//! small PNG for every image it listed. Reusing it avoids decoding a
//! multi-megabyte photo for a 62×46 tile. When there is none, a
//! [`Generator`] makes one in the `large` (256 px) directory, tagged as the
//! standard requires so the other managers reuse it, and records a decode
//! failure under `fail/fileflux-<version>` so a broken image is not decoded
//! again until it changes.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, BufReader, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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

// --- Generating ---------------------------------------------------------------

/// The longest edge of a `large` thumbnail, in pixels.
pub const LARGE_EDGE: u32 = 256;
/// Images with more pixels than this are never decoded for a thumbnail.
pub const MAX_PIXELS: u64 = 50_000_000;
/// Images bigger than this many bytes are never decoded for a thumbnail.
pub const MAX_BYTES: u64 = 100 * 1024 * 1024;
/// The spec's failure directory for this program and version.
const FAIL_DIRECTORY: &str = concat!("fail/fileflux-", env!("CARGO_PKG_VERSION"));

/// `statfs` types of FUSE and network filesystems: reading a whole image over
/// them just to make a thumbnail is slow and may hang.
const REMOTE_FILESYSTEMS: [u64; 5] = [
    0x6573_5546, // FUSE
    0x6969,      // NFS
    0x517B,      // SMB
    0xFE53_4D42, // SMB2
    0xFF53_4D42, // CIFS
];

/// Why an image got no thumbnail and no failure marker either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// The file cannot be reached.
    Unreachable,
    /// The file is inside the thumbnail cache: it is a thumbnail already.
    InsideCache,
    /// The file is on gvfs, FUSE or a network filesystem.
    Remote,
    /// Over [`MAX_BYTES`] or [`MAX_PIXELS`].
    TooLarge,
    /// The cache could not be written.
    CacheUnwritable,
}

/// What [`Generator::ensure`] came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The thumbnail, found in the cache or just made.
    Ready(PathBuf),
    /// The image does not decode; a marker keeps it from being tried again
    /// until its modification time changes.
    Failed,
    Skipped(Skip),
}

enum Failure {
    TooLarge,
    Undecodable(String),
    Io(io::Error),
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Makes the missing thumbnails of a cache. Blocking: call it off the UI
/// thread.
#[derive(Debug, Clone)]
pub struct Generator {
    cache_root: PathBuf,
    gvfs_root: Option<PathBuf>,
}

impl Generator {
    /// `gvfs_root` is where gvfs mounts live ([`super::places::gvfs_root`]).
    #[must_use]
    pub fn new(cache_root: PathBuf, gvfs_root: Option<PathBuf>) -> Self {
        Self {
            cache_root,
            gvfs_root,
        }
    }

    /// The current thumbnail of `image`: the cached one, else a new `large`
    /// one. Never writes for a file inside the cache, on gvfs/FUSE, or over
    /// the size caps.
    #[must_use]
    pub fn ensure(&self, image: &Path) -> Outcome {
        let Some(source) = Source::of(image) else {
            return Outcome::Skipped(Skip::Unreachable);
        };
        if let Some(skip) = self.skip_reason(&source.canonical) {
            return Outcome::Skipped(skip);
        }
        if let Some(found) = find_for(&self.cache_root, image, &source) {
            return Outcome::Ready(found);
        }
        if self.failure_is_recorded(&source) {
            log::debug!("not retrying {}: marked as failed", image.display());
            return Outcome::Failed;
        }
        match self.create(&source) {
            Ok(thumbnail) => {
                log::info!(
                    "generated thumbnail {} for {}",
                    thumbnail.display(),
                    image.display()
                );
                Outcome::Ready(thumbnail)
            }
            Err(Failure::TooLarge) => Outcome::Skipped(Skip::TooLarge),
            Err(Failure::Undecodable(reason)) => {
                log::info!(
                    "cannot decode {} for a thumbnail: {reason}",
                    image.display()
                );
                if let Err(error) = self.record_failure(&source) {
                    log::warn!("cannot record the thumbnail failure: {error}");
                }
                Outcome::Failed
            }
            Err(Failure::Io(error)) => {
                log::warn!("cannot write the thumbnail of {}: {error}", image.display());
                Outcome::Skipped(Skip::CacheUnwritable)
            }
        }
    }

    fn skip_reason(&self, canonical: &Path) -> Option<Skip> {
        let cache = fs::canonicalize(&self.cache_root).unwrap_or_else(|_| self.cache_root.clone());
        if canonical.starts_with(&cache) || canonical.starts_with(&self.cache_root) {
            return Some(Skip::InsideCache);
        }
        let on_gvfs = self.gvfs_root.as_deref().is_some_and(|root| {
            let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
            canonical.starts_with(root)
        });
        let remote = on_gvfs
            || rustix::fs::statfs(canonical)
                .is_ok_and(|stat| matches!(u64::try_from(stat.f_type), Ok(kind) if REMOTE_FILESYSTEMS.contains(&kind)));
        remote.then_some(Skip::Remote)
    }

    fn tags(source: &Source) -> Vec<(String, String)> {
        vec![
            ("Thumb::URI".to_owned(), source.uri.clone()),
            ("Thumb::MTime".to_owned(), source.modified.to_string()),
        ]
    }

    fn failure_path(&self, source: &Source) -> PathBuf {
        self.cache_root
            .join(FAIL_DIRECTORY)
            .join(thumbnail_file_name(&source.uri))
    }

    fn failure_is_recorded(&self, source: &Source) -> bool {
        thumbnail_tags(&self.failure_path(source)).is_some_and(|(uri, mtime)| {
            same_uri(&uri, &source.uri) && mtime.trim().parse() == Ok(source.modified)
        })
    }

    fn record_failure(&self, source: &Source) -> io::Result<()> {
        let marker = self.failure_path(source);
        let (Some(dir), Some(name)) = (marker.parent(), marker.file_name()) else {
            return Err(io::Error::other("the failure marker has no directory"));
        };
        write_png(
            dir,
            name,
            (1, 1),
            png::ColorType::Grayscale,
            &[0],
            &Self::tags(source),
        )
    }

    fn create(&self, source: &Source) -> Result<PathBuf, Failure> {
        let size = fs::metadata(&source.canonical)?.len();
        if size > MAX_BYTES {
            return Err(Failure::TooLarge);
        }
        let reader = || -> io::Result<_> {
            image::ImageReader::open(&source.canonical)?.with_guessed_format()
        };
        let (width, height) = reader()?
            .into_dimensions()
            .map_err(|error| Failure::Undecodable(error.to_string()))?;
        if u64::from(width) * u64::from(height) > MAX_PIXELS {
            return Err(Failure::TooLarge);
        }
        // Camera photos carry their rotation in EXIF; the thumbnail must
        // be upright like the full image the viewer shows.
        let undecodable = |error: image::ImageError| Failure::Undecodable(error.to_string());
        let mut decoder = reader()?.into_decoder().map_err(undecodable)?;
        let orientation = image::ImageDecoder::orientation(&mut decoder)
            .unwrap_or(image::metadata::Orientation::NoTransforms);
        let mut decoded = image::DynamicImage::from_decoder(decoder).map_err(undecodable)?;
        decoded.apply_orientation(orientation);
        let small = if width > LARGE_EDGE || height > LARGE_EDGE {
            decoded.thumbnail(LARGE_EDGE, LARGE_EDGE)
        } else {
            decoded
        }
        .into_rgba8();
        let mut tags = Self::tags(source);
        tags.extend([
            ("Thumb::Size".to_owned(), size.to_string()),
            ("Thumb::Image::Width".to_owned(), width.to_string()),
            ("Thumb::Image::Height".to_owned(), height.to_string()),
            ("Software".to_owned(), "Fileflux".to_owned()),
        ]);
        let dir = self.cache_root.join("large");
        let name = thumbnail_file_name(&source.uri);
        write_png(
            &dir,
            OsStr::new(&name),
            small.dimensions(),
            png::ColorType::Rgba,
            small.as_raw(),
            &tags,
        )?;
        Ok(dir.join(name))
    }
}

/// Writes a PNG with `tEXt` chunks as `dir/name`: the directories are made
/// 0700, the file is built 0600 beside its destination and renamed into
/// place, so a reader never sees half a thumbnail and no temp file survives.
fn write_png(
    dir: &Path,
    name: &OsStr,
    (width, height): (u32, u32),
    color: png::ColorType,
    pixels: &[u8],
    tags: &[(String, String)],
) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        for (key, value) in tags {
            encoder.add_text_chunk(key.clone(), value.clone())?;
        }
        encoder.write_header()?.write_image_data(pixels)?;
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let temp = dir.join(format!(
        ".fileflux-{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temp, dir.join(name))
    })();
    if written.is_err() {
        drop(fs::remove_file(&temp));
    }
    written
}

/// [`Generator::ensure`] against the process environment's cache and gvfs
/// directory. Blocking: the explorer calls it on the background executor.
#[must_use]
pub fn generate(image: &Path) -> Outcome {
    static GENERATOR: LazyLock<Option<Generator>> = LazyLock::new(|| {
        let root = cache_root(
            std::env::var_os("HOME").as_deref(),
            std::env::var_os("XDG_CACHE_HOME").as_deref(),
        )?;
        Some(Generator::new(root, super::places::gvfs_root_from_env()))
    });
    GENERATOR
        .as_ref()
        .map_or(Outcome::Skipped(Skip::CacheUnwritable), |generator| {
            generator.ensure(image)
        })
}
