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

/// The longest edge, in pixels, of a `normal` thumbnail.
pub const NORMAL_EDGE: f32 = 128.0;

/// The smallest freedesktop size directory a tile accepts: the thumbnail
/// must be at least as large as the box it fills, or it draws blurred.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Bucket {
    /// `normal`, 128 px: enough for a box up to that size.
    Normal,
    /// `large` (256 px) or above: for a bigger box.
    Large,
}

impl Bucket {
    /// The bucket for a thumbnail box whose longest edge is `edge` logical
    /// pixels: `normal` up to its own 128 px, `large` above.
    #[must_use]
    pub fn for_display(edge: f32) -> Self {
        if edge <= NORMAL_EDGE {
            Self::Normal
        } else {
            Self::Large
        }
    }

    /// The size directories that satisfy this bucket, smallest first.
    fn directories(self) -> &'static [&'static str] {
        match self {
            Self::Normal => &SIZE_DIRECTORIES,
            Self::Large => &["large", "x-large"],
        }
    }

    /// The bucket a cached thumbnail belongs to, from its size directory;
    /// `None` for a path outside the cache's size directories.
    #[must_use]
    pub fn of_thumbnail(thumbnail: &Path) -> Option<Self> {
        match thumbnail.parent()?.file_name()?.to_str()? {
            "normal" => Some(Self::Normal),
            "large" | "x-large" => Some(Self::Large),
            _ => None,
        }
    }
}

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
    find_at(cache_root, image, Bucket::Normal)
}

/// [`find`] for a tile that needs at least the `bucket` size: a smaller
/// cached thumbnail is not an answer.
#[must_use]
pub fn find_at(cache_root: &Path, image: &Path, bucket: Bucket) -> Option<PathBuf> {
    find_for(cache_root, image, &Source::of(image)?, bucket)
}

fn find_for(cache_root: &Path, image: &Path, source: &Source, bucket: Bucket) -> Option<PathBuf> {
    let name = thumbnail_file_name(&source.uri);
    bucket.directories().iter().find_map(|size| {
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
    hits: Mutex<HashMap<(PathBuf, u64, Bucket), Hit>>,
}

impl Memo {
    /// [`find`] through the memory. Blocking, like [`find`].
    #[must_use]
    pub fn lookup(&self, cache_root: &Path, image: &Path) -> Option<PathBuf> {
        self.lookup_at(cache_root, image, Bucket::Normal)
    }

    /// [`Self::lookup`] for at least the `bucket` size; each bucket has its
    /// own remembered answer.
    #[must_use]
    pub fn lookup_at(&self, cache_root: &Path, image: &Path, bucket: Bucket) -> Option<PathBuf> {
        let source = Source::of(image)?;
        let key = (source.canonical.clone(), source.modified, bucket);
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
        let found = find_for(cache_root, image, &source, bucket)?;
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
    lookup_at(image, Bucket::Normal)
}

/// [`lookup`] for a tile that needs at least the `bucket` size.
#[must_use]
pub fn lookup_at(image: &Path, bucket: Bucket) -> Option<PathBuf> {
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
    let found = MEMO.lookup_at(root, image, bucket);
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
    /// No installed thumbnailer makes a preview of this file, or the one that
    /// claims it failed. Nothing is recorded, so a thumbnailer installed
    /// later is used.
    NoPreview,
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
    /// Not a format this program decodes and no thumbnailer claimed it.
    Unsupported,
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
        self.ensure_at(image, Bucket::Normal)
    }

    /// [`Self::ensure`] for a tile that needs at least the `bucket` size: a
    /// cached `normal` thumbnail does not satisfy `Large`, so a `large` one
    /// is made beside it.
    #[must_use]
    pub fn ensure_at(&self, image: &Path, bucket: Bucket) -> Outcome {
        let Some(source) = Source::of(image) else {
            return Outcome::Skipped(Skip::Unreachable);
        };
        if let Some(skip) = self.skip_reason(&source.canonical) {
            return Outcome::Skipped(skip);
        }
        if let Some(found) = find_for(&self.cache_root, image, &source, bucket) {
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
            Err(Failure::Unsupported) => Outcome::Skipped(Skip::NoPreview),
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
        let mut tags = Self::tags(source);
        tags.push(("Thumb::Size".to_owned(), size.to_string()));
        let pixels = match self.decode(&source.canonical) {
            Ok((pixels, (width, height))) => {
                tags.push(("Thumb::Image::Width".to_owned(), width.to_string()));
                tags.push(("Thumb::Image::Height".to_owned(), height.to_string()));
                pixels
            }
            // Not a format this program decodes: a video, a PDF, an office
            // document or a vector image. The system's thumbnailers make its
            // preview, the same ones pcmanfm and Nautilus use.
            Err(Failure::Undecodable(reason)) => {
                match self.thumbnailed_by_system(&source.canonical) {
                    Some(pixels) => pixels,
                    None if is_decodable_image(&source.canonical) => {
                        // A broken image: the failure marker keeps it from being
                        // decoded again until it changes.
                        log::info!(
                            "cannot decode {} for a thumbnail: {reason}",
                            source.canonical.display()
                        );
                        return Err(Failure::Undecodable(reason));
                    }
                    None => {
                        log::debug!(
                            "no thumbnailer makes a preview of {} ({reason})",
                            source.canonical.display()
                        );
                        return Err(Failure::Unsupported);
                    }
                }
            }
            Err(other) => return Err(other),
        };
        tags.push(("Software".to_owned(), "Fileflux".to_owned()));
        let dir = self.cache_root.join("large");
        let name = thumbnail_file_name(&source.uri);
        write_png(
            &dir,
            OsStr::new(&name),
            pixels.dimensions(),
            png::ColorType::Rgba,
            pixels.as_raw(),
            &tags,
        )?;
        Ok(dir.join(name))
    }

    /// The source decoded, made upright and shrunk to a `large` thumbnail,
    /// with the pixel size it had.
    fn decode(&self, path: &Path) -> Result<(image::RgbaImage, (u32, u32)), Failure> {
        let reader = || -> io::Result<_> { image::ImageReader::open(path)?.with_guessed_format() };
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
        };
        Ok((small.into_rgba8(), (width, height)))
    }

    /// The preview an installed freedesktop thumbnailer makes of `path`, if
    /// one claims its MIME type. Blocking: it spawns a process.
    fn thumbnailed_by_system(&self, path: &Path) -> Option<image::RgbaImage> {
        let mime = super::mimes::of_path(path)?;
        let thumbnailer = super::thumbnailers::for_mime(mime)?;
        log::debug!(
            "asking the thumbnailer {} for a preview of {}",
            thumbnailer.path.display(),
            path.display()
        );
        super::thumbnailers::run(thumbnailer, path, LARGE_EDGE)
    }
}

/// Whether `path` names a raster image this program decodes: the formats the
/// `image` feature set carries. A broken one of these is remembered as failed
/// instead of being retried on every listing, and the details panel shows it
/// whole instead of waiting for a thumbnail of it.
#[must_use]
pub fn is_decodable_image(path: &Path) -> bool {
    super::mimes::of_path(path).is_some_and(|mime| {
        matches!(
            mime,
            "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/bmp"
        )
    })
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
    generate_at(image, Bucket::Normal)
}

/// [`generate`] for a tile that needs at least the `bucket` size.
#[must_use]
pub fn generate_at(image: &Path, bucket: Bucket) -> Outcome {
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
            generator.ensure_at(image, bucket)
        })
}

// --- Scheduling ---------------------------------------------------------------

/// One image to find or make a thumbnail for: its path and the modification
/// time it had when it was listed.
pub type Job = (PathBuf, Option<SystemTime>);

/// The thumbnail work of one listing, shared between the worker threads that
/// do it and the view that shows it: the workers take the next job, finish it
/// into a result, and the view collects every result that arrived since it
/// last looked, so a burst of thumbnails costs one redraw, not one each.
/// What the view is showing decides what comes next ([`Queue::reprioritize`]),
/// and closing the queue — leaving the folder — stops the workers after the
/// jobs they are running.
pub struct Queue<R> {
    state: Mutex<QueueState<R>>,
}

impl<R> std::fmt::Debug for Queue<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        formatter
            .debug_struct("Queue")
            .field("pending", &state.pending.len())
            .field("running", &state.running)
            .field("done", &state.done.len())
            .field("closed", &state.closed)
            .finish()
    }
}

struct QueueState<R> {
    pending: std::collections::VecDeque<Job>,
    running: usize,
    done: Vec<(Job, R)>,
    closed: bool,
}

impl<R> Queue<R> {
    /// A queue that serves `jobs` in the order given.
    #[must_use]
    pub fn new(jobs: Vec<Job>) -> Self {
        Self {
            state: Mutex::new(QueueState {
                pending: jobs.into(),
                running: 0,
                done: Vec::new(),
                closed: false,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState<R>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The next job to work on, or `None` when none is left or the queue was
    /// closed. The job counts as running until [`Queue::finish`].
    pub fn next(&self) -> Option<Job> {
        let mut state = self.lock();
        if state.closed {
            return None;
        }
        let job = state.pending.pop_front()?;
        state.running += 1;
        Some(job)
    }

    /// Whether a job is waiting to be served (the queue is open).
    pub fn has_work(&self) -> bool {
        let state = self.lock();
        !state.closed && !state.pending.is_empty()
    }

    /// Serves the next job: runs `work` on it and records its result;
    /// `false` when there was no job to serve. A job whose `work` panics
    /// counts as finished without a result (its tile keeps the glyph), so a
    /// decoder that panics can never leave the queue counted as running.
    pub fn work_next(&self, work: impl FnOnce(&Job) -> R) -> bool {
        let Some(job) = self.next() else {
            return false;
        };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&job))) {
            Ok(result) => self.finish(job, result),
            Err(_) => {
                log::warn!(
                    "thumbnail of {} panicked; it keeps its icon",
                    job.0.display()
                );
                let mut state = self.lock();
                state.running = state.running.saturating_sub(1);
            }
        }
        true
    }

    /// Records `result` as the outcome of `job`, a job [`Queue::next`] served.
    pub fn finish(&self, job: Job, result: R) {
        let mut state = self.lock();
        state.running = state.running.saturating_sub(1);
        state.done.push((job, result));
    }

    /// Every result finished since the last call, in the order they finished.
    pub fn drain(&self) -> Vec<(Job, R)> {
        std::mem::take(&mut self.lock().done)
    }

    /// Stops serving: the pending jobs are dropped.
    pub fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        state.pending.clear();
    }

    /// Whether nothing is pending, nothing is running and every result was
    /// collected.
    pub fn is_idle(&self) -> bool {
        let state = self.lock();
        state.pending.is_empty() && state.running == 0 && state.done.is_empty()
    }

    /// Puts the jobs the view shows first. `position` is a job's place in the
    /// view (`None` for one the view hides) and `viewport` the places on
    /// screen: those are served first in view order, then the ones after the
    /// screen (the way a reader scrolls), then the ones before it, and last
    /// the ones the view does not show. Jobs of equal rank keep their order.
    pub fn reprioritize(
        &self,
        position: impl Fn(&Path) -> Option<usize>,
        viewport: std::ops::Range<usize>,
    ) {
        let mut state = self.lock();
        state
            .pending
            .make_contiguous()
            .sort_by_cached_key(|(path, _)| match position(path) {
                Some(place) if viewport.contains(&place) => (0_u8, place),
                Some(place) if place >= viewport.end => (1, place),
                Some(place) => (2, place),
                None => (3, 0),
            });
    }
}

/// The most thumbnail jobs running at once across the whole app, however many
/// tabs and panes list images: each worker holds a [`Permit`] while it works
/// on a job. `reserve` permits are kept for the foreground (the tab the user
/// looks at), so tabs behind it cannot take every one.
#[derive(Debug)]
pub struct Limiter {
    cap: usize,
    reserve: usize,
    in_use: std::sync::atomic::AtomicUsize,
}

/// One running job of a [`Limiter`]; released on drop.
#[derive(Debug)]
pub struct Permit<'a>(&'a Limiter);

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.0
            .in_use
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

impl Limiter {
    /// A limiter of `cap` permits (at least one) that keeps `reserve` of them
    /// for the foreground; a background holder always has at least one.
    #[must_use]
    pub const fn new(cap: usize, reserve: usize) -> Self {
        Self {
            cap: if cap == 0 { 1 } else { cap },
            reserve,
            in_use: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// A permit, or `None` when the limit is reached: the foreground may use
    /// all of them, the background all but the reserve.
    pub fn try_acquire(&self, foreground: bool) -> Option<Permit<'_>> {
        let limit = if foreground {
            self.cap
        } else {
            self.cap.saturating_sub(self.reserve).max(1)
        };
        self.in_use
            .try_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |in_use| (in_use < limit).then_some(in_use + 1),
            )
            .ok()
            .map(|_| Permit(self))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fileflux-bucket-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        drop(fs::remove_dir_all(&dir));
        fs::create_dir_all(&dir).expect("scratch directory is created");
        dir
    }

    /// A tiny real PNG image the generator can decode.
    fn picture(dir: &Path) -> PathBuf {
        let path = dir.join("picture.png");
        image::RgbaImage::from_pixel(8, 8, image::Rgba([200, 30, 30, 255]))
            .save(&path)
            .expect("fixture image writes");
        fs::canonicalize(&path).expect("fixture image resolves")
    }

    /// Caches a thumbnail of `image` in the `size` directory, tagged as the
    /// standard requires.
    fn cache_entry(cache: &Path, size: &str, image: &Path) -> PathBuf {
        let source = Source::of(image).expect("fixture image is reachable");
        let dir = cache.join(size);
        let name = thumbnail_file_name(&source.uri);
        write_png(
            &dir,
            OsStr::new(&name),
            (1, 1),
            png::ColorType::Grayscale,
            &[0],
            &Generator::tags(&source),
        )
        .expect("fixture thumbnail writes");
        dir.join(name)
    }

    #[test]
    fn the_bucket_follows_the_displayed_edge_at_the_normal_size() {
        for edge in [1.0, 46.0, 62.0, 110.0, 128.0] {
            assert_eq!(Bucket::for_display(edge), Bucket::Normal, "{edge}");
        }
        for edge in [128.5, 174.0, 256.0, 512.0] {
            assert_eq!(Bucket::for_display(edge), Bucket::Large, "{edge}");
        }
    }

    #[test]
    fn a_thumbnail_path_names_its_bucket_by_its_size_directory() {
        let at = |path: &str| Bucket::of_thumbnail(Path::new(path));
        assert_eq!(at("/c/thumbnails/normal/a.png"), Some(Bucket::Normal));
        assert_eq!(at("/c/thumbnails/large/a.png"), Some(Bucket::Large));
        assert_eq!(at("/c/thumbnails/x-large/a.png"), Some(Bucket::Large));
        assert_eq!(at("/pictures/a.png"), None);
        assert!(Bucket::Normal < Bucket::Large);
    }

    #[test]
    fn a_large_request_skips_the_normal_thumbnail_and_a_normal_one_prefers_it() {
        let dir = scratch("find");
        let cache = dir.join("thumbnails");
        let image = picture(&dir);
        let normal = cache_entry(&cache, "normal", &image);
        assert_eq!(
            find_at(&cache, &image, Bucket::Normal),
            Some(normal.clone())
        );
        assert_eq!(find_at(&cache, &image, Bucket::Large), None);
        assert_eq!(find(&cache, &image), Some(normal.clone()), "find is Normal");

        let large = cache_entry(&cache, "large", &image);
        assert_eq!(find_at(&cache, &image, Bucket::Normal), Some(normal));
        assert_eq!(find_at(&cache, &image, Bucket::Large), Some(large));
        drop(fs::remove_dir_all(&dir));
    }

    #[test]
    fn the_memo_keeps_one_answer_per_bucket() {
        let dir = scratch("memo");
        let cache = dir.join("thumbnails");
        let image = picture(&dir);
        let normal = cache_entry(&cache, "normal", &image);
        let memo = Memo::default();
        assert_eq!(memo.lookup(&cache, &image), Some(normal.clone()));
        assert_eq!(
            memo.lookup_at(&cache, &image, Bucket::Large),
            None,
            "the remembered normal hit does not answer a large request"
        );
        let large = cache_entry(&cache, "large", &image);
        assert_eq!(memo.lookup_at(&cache, &image, Bucket::Large), Some(large));
        assert_eq!(memo.lookup(&cache, &image), Some(normal));
        drop(fs::remove_dir_all(&dir));
    }

    #[test]
    fn ensuring_a_large_thumbnail_generates_one_beside_a_cached_normal() {
        let dir = scratch("ensure");
        let cache = dir.join("thumbnails");
        let image = picture(&dir);
        let normal = cache_entry(&cache, "normal", &image);
        let generator = Generator::new(cache.clone(), None);
        assert_eq!(generator.ensure(&image), Outcome::Ready(normal));
        let made = generator.ensure_at(&image, Bucket::Large);
        let Outcome::Ready(large) = made else {
            unreachable!("a large thumbnail is generated, got {made:?}");
        };
        assert_eq!(Bucket::of_thumbnail(&large), Some(Bucket::Large));
        assert_eq!(
            generator.ensure_at(&image, Bucket::Large),
            Outcome::Ready(large),
            "the second request reuses it"
        );
        drop(fs::remove_dir_all(&dir));
    }

    fn job(index: usize) -> Job {
        (PathBuf::from(format!("/p/{index}.jpg")), None)
    }

    fn position(path: &Path) -> Option<usize> {
        path.file_stem()?.to_str()?.parse().ok()
    }

    fn served(queue: &Queue<u8>) -> Vec<usize> {
        std::iter::from_fn(|| queue.next())
            .filter_map(|(path, _)| position(&path))
            .collect()
    }

    #[test]
    fn a_queue_serves_its_jobs_in_the_given_order() {
        let queue = Queue::<u8>::new((0..5).map(job).collect());
        assert_eq!(served(&queue), [0, 1, 2, 3, 4]);
        assert_eq!(queue.next(), None, "an empty queue serves nothing");
    }

    #[test]
    fn the_viewport_is_served_first_then_what_follows_then_what_precedes() {
        let queue = Queue::<u8>::new((0..10).map(job).collect());
        queue.reprioritize(position, 6..8);
        assert_eq!(served(&queue), [6, 7, 8, 9, 0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_job_the_view_does_not_show_waits_behind_every_shown_one() {
        let mut jobs: Vec<Job> = (0..3).map(job).collect();
        jobs.insert(0, (PathBuf::from("/p/hidden.jpg"), None));
        let queue = Queue::<u8>::new(jobs);
        queue.reprioritize(position, 1..2);
        let order: Vec<Option<usize>> = std::iter::from_fn(|| queue.next())
            .map(|(path, _)| position(&path))
            .collect();
        assert_eq!(order, [Some(1), Some(2), Some(0), None]);
    }

    #[test]
    fn a_closed_queue_serves_nothing_more() {
        let queue = Queue::<u8>::new((0..3).map(job).collect());
        assert!(queue.next().is_some());
        queue.close();
        assert_eq!(queue.next(), None);
    }

    #[test]
    fn results_come_out_in_one_batch_and_the_queue_is_idle_once_all_are_in() {
        let queue = Queue::new((0..2).map(job).collect());
        assert!(!queue.is_idle());
        let first = queue.next().expect("a job");
        let second = queue.next().expect("a second job");
        assert!(!queue.is_idle(), "two jobs are running");
        queue.finish(first, 1u8);
        assert!(!queue.is_idle(), "one job is still running");
        queue.finish(second, 2u8);
        assert!(!queue.is_idle(), "the results were not collected yet");
        let batch = queue.drain();
        assert_eq!(
            batch.iter().map(|(_, result)| *result).collect::<Vec<_>>(),
            [1, 2]
        );
        assert!(queue.drain().is_empty(), "a result is handed out once");
        assert!(queue.is_idle());
    }

    // --- Queue and limiter ------------------------------------------------

    fn jobs(n: usize) -> Vec<Job> {
        (0..n)
            .map(|i| (PathBuf::from(format!("/p/{i}.jpg")), None))
            .collect()
    }

    fn place(path: &Path) -> Option<usize> {
        path.file_stem()?.to_str()?.parse().ok()
    }

    /// Many folders opened and left while workers run and the view scrolls:
    /// every worker exits, no result is lost or duplicated for a queue that
    /// ran to the end, and a closed queue stops serving.
    #[test]
    fn open_close_cycles_never_hang_or_duplicate() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for cycle in 0..300 {
            let n = 50 + cycle % 200;
            let queue = std::sync::Arc::new(Queue::<usize>::new(jobs(n)));
            let served = std::sync::Arc::new(AtomicUsize::new(0));
            let workers: Vec<_> = (0..4)
                .map(|_| {
                    let (queue, served) = (queue.clone(), served.clone());
                    std::thread::spawn(move || {
                        while let Some(job) = queue.next() {
                            served.fetch_add(1, Ordering::Relaxed);
                            let value = place(&job.0).unwrap_or(usize::MAX);
                            queue.finish(job, value);
                        }
                    })
                })
                .collect();
            let mut drained = Vec::new();
            for step in 0..5 {
                queue.reprioritize(place, (step * 10)..(step * 10 + 20));
                drained.extend(queue.drain());
            }
            let leave = cycle % 3 == 0;
            if leave {
                queue.close();
                assert!(queue.next().is_none(), "a closed queue serves nothing");
            }
            for worker in workers {
                worker.join().expect("worker did not panic");
            }
            drained.extend(queue.drain());
            assert!(queue.is_idle(), "cycle {cycle}: idle once the workers exit");
            let mut seen: Vec<usize> = drained.iter().map(|(_, value)| *value).collect();
            seen.sort_unstable();
            let before = seen.len();
            seen.dedup();
            assert_eq!(before, seen.len(), "cycle {cycle}: published twice");
            assert_eq!(
                seen.len(),
                served.load(Ordering::Relaxed),
                "cycle {cycle}: a result was lost"
            );
            if !leave {
                assert_eq!(
                    seen,
                    (0..n).collect::<Vec<_>>(),
                    "cycle {cycle}: all done once"
                );
            }
        }
    }

    /// Re-prioritizing never grows the queue nor duplicates a job.
    #[test]
    fn reprioritize_neither_grows_nor_duplicates() {
        let queue = Queue::<u8>::new(jobs(1000));
        for start in (0..1000).step_by(7) {
            queue.reprioritize(place, start..start + 40);
        }
        let mut served: Vec<usize> = std::iter::from_fn(|| queue.next())
            .filter_map(|(path, _)| place(&path))
            .collect();
        assert_eq!(served.len(), 1000);
        served.sort_unstable();
        served.dedup();
        assert_eq!(served.len(), 1000);
    }

    /// A job that panics counts as finished (no result, so its tile keeps the
    /// glyph): the queue still goes idle and the other jobs are served.
    #[test]
    fn a_panicking_job_still_counts_as_finished() {
        let queue = Queue::<usize>::new(jobs(3));
        let mut served = 0;
        while queue.work_next(|job| {
            served += 1;
            assert!(place(&job.0) != Some(1), "decoder panic");
            served
        }) {}
        assert_eq!(served, 3, "the jobs after the panic are served");
        let answers = queue.drain();
        assert_eq!(answers.len(), 2, "the panicked job has no result");
        assert!(queue.is_idle(), "nothing stays counted as running");
    }

    #[test]
    fn has_work_follows_pending_and_close() {
        let queue = Queue::<u8>::new(jobs(1));
        assert!(queue.has_work());
        assert!(queue.next().is_some());
        assert!(!queue.has_work(), "nothing pending");
        let queue = Queue::<u8>::new(jobs(2));
        queue.close();
        assert!(!queue.has_work(), "closed");
    }

    /// The cap is shared by every holder, and a background tab leaves the
    /// reserved permits to the foreground one.
    #[test]
    fn the_limiter_caps_all_holders_and_reserves_for_the_foreground() {
        let limiter = Limiter::new(3, 1);
        let first = limiter
            .try_acquire(false)
            .expect("background gets a permit");
        let second = limiter
            .try_acquire(false)
            .expect("background gets a second");
        assert!(
            limiter.try_acquire(false).is_none(),
            "the last permit is reserved for the foreground"
        );
        let third = limiter
            .try_acquire(true)
            .expect("foreground takes the reserve");
        assert!(limiter.try_acquire(true).is_none(), "the cap is absolute");
        drop(second);
        let again = limiter
            .try_acquire(true)
            .expect("a released permit is reusable");
        drop((first, third, again));
        assert!(limiter.try_acquire(false).is_some());
    }

    #[test]
    fn the_limiter_never_lets_a_background_holder_starve_at_a_cap_of_one() {
        let limiter = Limiter::new(1, 1);
        assert!(
            limiter.try_acquire(false).is_some(),
            "a reserve never takes the only permit from everyone"
        );
    }
}
