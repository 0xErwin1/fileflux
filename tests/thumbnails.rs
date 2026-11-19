//! The freedesktop thumbnail cache reader, against fixture caches in a fresh
//! scratch directory. Nothing here touches the real `~/.cache/thumbnails`.

use std::error::Error;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fileflux::core::thumbnails::{
    Generator, Memo, Outcome, Skip, cache_root, find, thumbnail_file_name, thumbnail_uri,
};

type TestResult = Result<(), Box<dyn Error>>;

/// A PNG with the given `tEXt` chunks (before the image data, as the
/// thumbnail spec has them).
fn png_with_text(texts: &[(&str, &str)]) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        for (key, value) in texts {
            encoder.add_text_chunk((*key).to_owned(), (*value).to_owned())?;
        }
        encoder.write_header()?.write_image_data(&[0])?;
    }
    Ok(bytes)
}

struct Fixture {
    scratch: PathBuf,
    cache: PathBuf,
    image: PathBuf,
}

const MTIME: u64 = 1_700_000_000;

impl Fixture {
    fn new(name: &str) -> Result<Self, Box<dyn Error>> {
        let scratch =
            std::env::temp_dir().join(format!("fileflux-thumbs-{name}-{}", std::process::id()));
        drop(fs::remove_dir_all(&scratch));
        let pictures = scratch.join("pictures");
        let cache = scratch.join("cache").join("thumbnails");
        fs::create_dir_all(&pictures)?;
        fs::create_dir_all(&cache)?;
        let image = pictures.join("photo one.jpg");
        let file = File::create(&image)?;
        file.set_modified(UNIX_EPOCH + Duration::from_secs(MTIME))?;
        Ok(Self {
            scratch,
            cache,
            image: fs::canonicalize(&image)?,
        })
    }

    /// Writes the cached thumbnail of the fixture image in `size`.
    fn cache_entry(&self, size: &str, texts: &[(&str, &str)]) -> Result<PathBuf, Box<dyn Error>> {
        let dir = self.cache.join(size);
        fs::create_dir_all(&dir)?;
        let file = dir.join(thumbnail_file_name(&thumbnail_uri(&self.image)));
        fs::write(&file, png_with_text(texts)?)?;
        Ok(file)
    }

    fn uri(&self) -> String {
        thumbnail_uri(&self.image)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.scratch));
    }
}

fn mtime_text() -> String {
    MTIME.to_string()
}

#[test]
fn the_cache_name_is_the_md5_of_the_uri() {
    // `printf 'file:///home/jens/photo.png' | md5sum`
    assert_eq!(
        thumbnail_file_name("file:///home/jens/photo.png"),
        "3a295334ea4f1f388246178ae903e902.png"
    );
}

#[test]
fn the_uri_escapes_like_glib() {
    assert_eq!(
        thumbnail_uri(Path::new("/p/a b#c%d é(1)@x.png")),
        "file:///p/a%20b%23c%25d%20%C3%A9(1)@x.png"
    );
}

#[test]
fn the_cache_lives_under_xdg_cache_home_else_home() {
    assert_eq!(
        cache_root(Some(OsStr::new("/h")), Some(OsStr::new("/c"))),
        Some(PathBuf::from("/c/thumbnails"))
    );
    assert_eq!(
        cache_root(Some(OsStr::new("/h")), None),
        Some(PathBuf::from("/h/.cache/thumbnails"))
    );
    assert_eq!(
        cache_root(Some(OsStr::new("/h")), Some(OsStr::new("relative"))),
        Some(PathBuf::from("/h/.cache/thumbnails"))
    );
    assert_eq!(cache_root(None, None), None);
}

#[test]
fn a_matching_thumbnail_is_found() -> TestResult {
    let fixture = Fixture::new("valid")?;
    let uri = fixture.uri();
    let entry = fixture.cache_entry(
        "normal",
        &[("Thumb::URI", &uri), ("Thumb::MTime", &mtime_text())],
    )?;
    assert_eq!(find(&fixture.cache, &fixture.image), Some(entry));
    Ok(())
}

#[test]
fn a_larger_size_is_used_when_the_smaller_is_absent_or_stale() -> TestResult {
    let fixture = Fixture::new("sizes")?;
    let uri = fixture.uri();
    fixture.cache_entry("normal", &[("Thumb::URI", &uri), ("Thumb::MTime", "1")])?;
    let large = fixture.cache_entry(
        "large",
        &[("Thumb::URI", &uri), ("Thumb::MTime", &mtime_text())],
    )?;
    assert_eq!(find(&fixture.cache, &fixture.image), Some(large));
    Ok(())
}

#[test]
fn a_stale_modification_time_is_ignored() -> TestResult {
    let fixture = Fixture::new("stale")?;
    let uri = fixture.uri();
    fixture.cache_entry(
        "normal",
        &[
            ("Thumb::URI", &uri),
            ("Thumb::MTime", &(MTIME - 1).to_string()),
        ],
    )?;
    assert_eq!(find(&fixture.cache, &fixture.image), None);
    Ok(())
}

#[test]
fn a_thumbnail_naming_another_uri_is_ignored() -> TestResult {
    let fixture = Fixture::new("uri")?;
    fixture.cache_entry(
        "normal",
        &[
            ("Thumb::URI", "file:///elsewhere/photo.jpg"),
            ("Thumb::MTime", &mtime_text()),
        ],
    )?;
    assert_eq!(find(&fixture.cache, &fixture.image), None);
    Ok(())
}

#[test]
fn a_thumbnail_without_its_text_chunks_is_ignored() -> TestResult {
    let fixture = Fixture::new("untagged")?;
    let uri = fixture.uri();
    fixture.cache_entry("normal", &[])?;
    assert_eq!(find(&fixture.cache, &fixture.image), None);
    fixture.cache_entry("normal", &[("Thumb::URI", &uri)])?;
    assert_eq!(find(&fixture.cache, &fixture.image), None, "no MTime");
    fixture.cache_entry("normal", &[("Thumb::MTime", &mtime_text())])?;
    assert_eq!(find(&fixture.cache, &fixture.image), None, "no URI");
    Ok(())
}

#[test]
fn a_missing_or_unreadable_thumbnail_is_ignored() -> TestResult {
    let fixture = Fixture::new("missing")?;
    assert_eq!(find(&fixture.cache, &fixture.image), None, "empty cache");
    assert_eq!(
        find(&fixture.scratch.join("no-such-cache"), &fixture.image),
        None
    );
    assert_eq!(
        find(&fixture.cache, &fixture.scratch.join("gone.jpg")),
        None
    );
    let dir = fixture.cache.join("normal");
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join(thumbnail_file_name(&fixture.uri())),
        b"this is not a PNG",
    )?;
    assert_eq!(find(&fixture.cache, &fixture.image), None, "garbage");
    Ok(())
}

#[test]
fn only_the_text_chunks_are_read_not_the_pixels() -> TestResult {
    let fixture = Fixture::new("header-only")?;
    let uri = fixture.uri();
    let entry = fixture.cache_entry(
        "normal",
        &[("Thumb::URI", &uri), ("Thumb::MTime", &mtime_text())],
    )?;
    let bytes = fs::read(&entry)?;
    let idat = bytes
        .windows(4)
        .position(|window| window == b"IDAT")
        .ok_or("no IDAT chunk")?;
    // Cut right after the image data chunk's type: no pixel could be decoded.
    fs::write(&entry, bytes.get(..idat + 4).ok_or("short png")?)?;
    assert_eq!(find(&fixture.cache, &fixture.image), Some(entry));
    Ok(())
}

#[test]
fn a_symlinked_path_is_looked_up_by_its_canonical_uri() -> TestResult {
    let fixture = Fixture::new("symlink")?;
    let uri = fixture.uri();
    let entry = fixture.cache_entry(
        "normal",
        &[("Thumb::URI", &uri), ("Thumb::MTime", &mtime_text())],
    )?;
    let link = fixture.scratch.join("link.jpg");
    std::os::unix::fs::symlink(&fixture.image, &link)?;
    assert_eq!(find(&fixture.cache, &link), Some(entry));
    Ok(())
}

#[test]
fn a_changed_file_invalidates_its_thumbnail() -> TestResult {
    let fixture = Fixture::new("changed")?;
    let uri = fixture.uri();
    fixture.cache_entry(
        "normal",
        &[("Thumb::URI", &uri), ("Thumb::MTime", &mtime_text())],
    )?;
    File::options()
        .write(true)
        .open(&fixture.image)?
        .set_modified(SystemTime::now())?;
    assert_eq!(find(&fixture.cache, &fixture.image), None);
    Ok(())
}

fn tagged(fixture: &Fixture, size: &str) -> Result<PathBuf, Box<dyn Error>> {
    let uri = fixture.uri();
    fixture.cache_entry(
        size,
        &[("Thumb::URI", &uri), ("Thumb::MTime", &mtime_text())],
    )
}

#[test]
fn the_memory_answers_a_repeat_with_the_same_thumbnail() -> TestResult {
    let fixture = Fixture::new("memo-hit")?;
    let entry = tagged(&fixture, "normal")?;
    let memo = Memo::default();
    assert_eq!(
        memo.lookup(&fixture.cache, &fixture.image),
        Some(entry.clone())
    );
    assert_eq!(memo.lookup(&fixture.cache, &fixture.image), Some(entry));
    Ok(())
}

#[test]
fn a_remembered_thumbnail_that_was_deleted_falls_back_to_the_image() -> TestResult {
    let fixture = Fixture::new("memo-deleted")?;
    let entry = tagged(&fixture, "normal")?;
    let memo = Memo::default();
    assert_eq!(
        memo.lookup(&fixture.cache, &fixture.image),
        Some(entry.clone())
    );
    fs::remove_file(&entry)?;
    assert_eq!(
        memo.lookup(&fixture.cache, &fixture.image),
        None,
        "the loader must get the full image, not a path that is gone"
    );
    // The same thumbnail written again is used again.
    let again = tagged(&fixture, "normal")?;
    assert_eq!(memo.lookup(&fixture.cache, &fixture.image), Some(again));
    Ok(())
}

#[test]
fn a_remembered_thumbnail_that_was_rewritten_is_read_again() -> TestResult {
    let fixture = Fixture::new("memo-rewritten")?;
    let entry = tagged(&fixture, "normal")?;
    let memo = Memo::default();
    assert_eq!(
        memo.lookup(&fixture.cache, &fixture.image),
        Some(entry.clone())
    );
    // Another program replaces it with a thumbnail of an older state of the
    // image (a different file: its length differs).
    fixture.cache_entry(
        "normal",
        &[
            ("Thumb::URI", &fixture.uri()),
            ("Thumb::MTime", &(MTIME - 5).to_string()),
            ("Thumb::Padding", "xxxxxxxxxxxxxxxxxxxx"),
        ],
    )?;
    assert_eq!(memo.lookup(&fixture.cache, &fixture.image), None);
    Ok(())
}

#[test]
fn the_memory_follows_the_images_own_modification_time() -> TestResult {
    let fixture = Fixture::new("memo-mtime")?;
    let entry = tagged(&fixture, "normal")?;
    let memo = Memo::default();
    assert_eq!(memo.lookup(&fixture.cache, &fixture.image), Some(entry));
    // The image changes: the thumbnail describes the old state.
    File::options()
        .write(true)
        .open(&fixture.image)?
        .set_modified(UNIX_EPOCH + Duration::from_secs(MTIME + 60))?;
    assert_eq!(memo.lookup(&fixture.cache, &fixture.image), None);
    // Back to the time the thumbnail records: found again.
    File::options()
        .write(true)
        .open(&fixture.image)?
        .set_modified(UNIX_EPOCH + Duration::from_secs(MTIME))?;
    assert!(memo.lookup(&fixture.cache, &fixture.image).is_some());
    Ok(())
}

#[test]
fn a_thumbnail_made_after_a_miss_is_found() -> TestResult {
    let fixture = Fixture::new("memo-late")?;
    let memo = Memo::default();
    assert_eq!(memo.lookup(&fixture.cache, &fixture.image), None);
    let entry = tagged(&fixture, "normal")?;
    assert_eq!(memo.lookup(&fixture.cache, &fixture.image), Some(entry));
    Ok(())
}

// --- Generating ---------------------------------------------------------------

/// A scratch tree with a pictures directory and an empty cache.
struct Bench {
    scratch: PathBuf,
    pictures: PathBuf,
    cache: PathBuf,
}

impl Bench {
    fn new(name: &str) -> Result<Self, Box<dyn Error>> {
        let scratch =
            std::env::temp_dir().join(format!("fileflux-gen-{name}-{}", std::process::id()));
        drop(fs::remove_dir_all(&scratch));
        let pictures = scratch.join("pictures");
        fs::create_dir_all(&pictures)?;
        Ok(Self {
            pictures: fs::canonicalize(&pictures)?,
            cache: fs::canonicalize(&scratch)?.join("cache").join("thumbnails"),
            scratch,
        })
    }

    fn generator(&self) -> Generator {
        Generator::new(self.cache.clone(), None)
    }

    /// An image of `width`×`height` in the pictures directory, with `mtime`.
    fn image(
        &self,
        name: &str,
        (width, height): (u32, u32),
        mtime: u64,
    ) -> Result<PathBuf, Box<dyn Error>> {
        let path = self.pictures.join(name);
        image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 251) as u8, (y % 251) as u8, 7])
        })
        .save(&path)?;
        File::options()
            .write(true)
            .open(&path)?
            .set_modified(UNIX_EPOCH + Duration::from_secs(mtime))?;
        Ok(path)
    }

    fn marker(&self, image: &Path) -> PathBuf {
        self.cache
            .join(concat!("fail/fileflux-", env!("CARGO_PKG_VERSION")))
            .join(thumbnail_file_name(&thumbnail_uri(image)))
    }

    /// Every file under the cache.
    fn cache_files(&self) -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push(path);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.cache, &mut out);
        out.sort();
        out
    }
}

impl Drop for Bench {
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.scratch));
    }
}

fn texts_of(png_file: &Path) -> Result<Vec<(String, String)>, Box<dyn Error>> {
    let reader = png::Decoder::new(std::io::BufReader::new(File::open(png_file)?)).read_info()?;
    Ok(reader
        .info()
        .uncompressed_latin1_text
        .iter()
        .map(|chunk| (chunk.keyword.clone(), chunk.text.clone()))
        .collect())
}

fn text<'a>(texts: &'a [(String, String)], key: &str) -> Option<&'a str> {
    texts
        .iter()
        .find(|(keyword, _)| keyword == key)
        .map(|(_, value)| value.as_str())
}

#[test]
fn a_missing_thumbnail_is_generated_large_and_tagged() -> TestResult {
    let bench = Bench::new("tags")?;
    let image = bench.image("wide.jpg", (600, 300), MTIME)?;
    let Outcome::Ready(thumbnail) = bench.generator().ensure(&image) else {
        return Err("no thumbnail".into());
    };
    let expected = bench
        .cache
        .join("large")
        .join(thumbnail_file_name(&thumbnail_uri(&image)));
    assert_eq!(thumbnail, expected);
    let decoder = png::Decoder::new(std::io::BufReader::new(File::open(&thumbnail)?));
    let reader = decoder.read_info()?;
    assert_eq!((reader.info().width, reader.info().height), (256, 128));
    let texts = texts_of(&thumbnail)?;
    assert_eq!(
        text(&texts, "Thumb::URI"),
        Some(thumbnail_uri(&image).as_str())
    );
    assert_eq!(text(&texts, "Thumb::MTime"), Some("1700000000"));
    let size = fs::metadata(&image)?.len().to_string();
    assert_eq!(text(&texts, "Thumb::Size"), Some(size.as_str()));
    assert_eq!(text(&texts, "Thumb::Image::Width"), Some("600"));
    assert_eq!(text(&texts, "Thumb::Image::Height"), Some("300"));
    assert_eq!(text(&texts, "Software"), Some("Fileflux"));
    // Another reader of the cache accepts it as current.
    assert_eq!(find(&bench.cache, &image), Some(thumbnail));
    Ok(())
}

#[test]
fn a_small_image_is_not_enlarged_and_portrait_keeps_its_aspect() -> TestResult {
    let bench = Bench::new("sizes")?;
    let small = bench.image("small.png", (40, 20), MTIME)?;
    let tall = bench.image("tall.png", (300, 900), MTIME)?;
    let dims = |image: &Path| -> Result<(u32, u32), Box<dyn Error>> {
        let Outcome::Ready(thumbnail) = bench.generator().ensure(image) else {
            return Err("no thumbnail".into());
        };
        let reader =
            png::Decoder::new(std::io::BufReader::new(File::open(thumbnail)?)).read_info()?;
        Ok((reader.info().width, reader.info().height))
    };
    assert_eq!(dims(&small)?, (40, 20));
    assert_eq!(dims(&tall)?, (85, 256));
    Ok(())
}

#[test]
fn the_cache_is_private_and_the_write_leaves_no_temp_file() -> TestResult {
    use std::os::unix::fs::PermissionsExt as _;
    let bench = Bench::new("modes")?;
    let image = bench.image("a.png", (30, 30), MTIME)?;
    let Outcome::Ready(thumbnail) = bench.generator().ensure(&image) else {
        return Err("no thumbnail".into());
    };
    let mode = |path: &Path| fs::metadata(path).map(|meta| meta.permissions().mode() & 0o777);
    assert_eq!(mode(&thumbnail)?, 0o600);
    assert_eq!(mode(&bench.cache.join("large"))?, 0o700);
    assert_eq!(mode(&bench.cache)?, 0o700);
    assert_eq!(bench.cache_files(), vec![thumbnail]);
    Ok(())
}

#[test]
fn a_changed_modification_time_makes_a_new_thumbnail() -> TestResult {
    let bench = Bench::new("mtime")?;
    let image = bench.image("a.png", (500, 500), MTIME)?;
    let Outcome::Ready(first) = bench.generator().ensure(&image) else {
        return Err("no thumbnail".into());
    };
    assert_eq!(text(&texts_of(&first)?, "Thumb::MTime"), Some("1700000000"));
    File::options()
        .write(true)
        .open(&image)?
        .set_modified(UNIX_EPOCH + Duration::from_secs(MTIME + 60))?;
    assert_eq!(find(&bench.cache, &image), None);
    let Outcome::Ready(second) = bench.generator().ensure(&image) else {
        return Err("no thumbnail".into());
    };
    assert_eq!(second, first);
    assert_eq!(
        text(&texts_of(&second)?, "Thumb::MTime"),
        Some("1700000060")
    );
    Ok(())
}

#[test]
fn a_fresh_cached_thumbnail_is_reused_not_rewritten() -> TestResult {
    let bench = Bench::new("reuse")?;
    let image = bench.image("a.png", (500, 500), MTIME)?;
    let Outcome::Ready(first) = bench.generator().ensure(&image) else {
        return Err("no thumbnail".into());
    };
    let before = fs::metadata(&first)?.modified()?;
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(
        bench.generator().ensure(&image),
        Outcome::Ready(first.clone())
    );
    assert_eq!(fs::metadata(&first)?.modified()?, before);
    Ok(())
}

#[test]
fn a_broken_image_leaves_a_marker_and_is_not_retried() -> TestResult {
    let bench = Bench::new("fail")?;
    let image = bench.pictures.join("broken.png");
    fs::write(&image, b"this is not a png")?;
    File::options()
        .write(true)
        .open(&image)?
        .set_modified(UNIX_EPOCH + Duration::from_secs(MTIME))?;
    assert_eq!(bench.generator().ensure(&image), Outcome::Failed);
    let marker = bench.marker(&image);
    let texts = texts_of(&marker)?;
    assert_eq!(
        text(&texts, "Thumb::URI"),
        Some(thumbnail_uri(&image).as_str())
    );
    assert_eq!(text(&texts, "Thumb::MTime"), Some("1700000000"));
    assert_eq!(bench.cache_files(), vec![marker.clone()]);
    // Repaired in place with the same mtime: still not retried.
    image::RgbImage::new(20, 20).save(&image)?;
    File::options()
        .write(true)
        .open(&image)?
        .set_modified(UNIX_EPOCH + Duration::from_secs(MTIME))?;
    assert_eq!(bench.generator().ensure(&image), Outcome::Failed);
    assert!(!bench.cache.join("large").exists());
    // A new mtime retries and succeeds.
    File::options()
        .write(true)
        .open(&image)?
        .set_modified(UNIX_EPOCH + Duration::from_secs(MTIME + 1))?;
    assert!(matches!(
        bench.generator().ensure(&image),
        Outcome::Ready(_)
    ));
    Ok(())
}

#[test]
fn nothing_is_written_for_files_inside_the_cache_or_on_gvfs_or_too_big() -> TestResult {
    let bench = Bench::new("skips")?;
    // Inside the cache: an image under any of its directories.
    fs::create_dir_all(bench.cache.join("normal"))?;
    let inside = bench.cache.join("normal").join("0123.png");
    image::RgbImage::new(300, 300).save(&inside)?;
    assert_eq!(
        bench.generator().ensure(&inside),
        Outcome::Skipped(Skip::InsideCache)
    );
    fs::remove_file(&inside)?;
    assert_eq!(bench.cache_files(), Vec::<PathBuf>::new());

    // gvfs: a mount directory under the gvfs root.
    let gvfs = bench.scratch.join("run").join("gvfs");
    let mount = gvfs.join("mtp:host=phone");
    fs::create_dir_all(&mount)?;
    let phone = mount.join("p.png");
    image::RgbImage::new(300, 300).save(&phone)?;
    let generator = Generator::new(bench.cache.clone(), Some(gvfs));
    assert_eq!(generator.ensure(&phone), Outcome::Skipped(Skip::Remote));

    // Over the pixel cap: a header that claims 60 000 × 1 000 pixels.
    let huge = bench.pictures.join("huge.png");
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 60_000, 1_000);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::One);
        let mut writer = encoder.write_header()?;
        let row = vec![0u8; 60_000 / 8];
        let mut stream = writer.stream_writer()?;
        for _ in 0..1_000 {
            std::io::Write::write_all(&mut stream, &row)?;
        }
        stream.finish()?;
    }
    fs::write(&huge, bytes)?;
    assert_eq!(
        bench.generator().ensure(&huge),
        Outcome::Skipped(Skip::TooLarge)
    );
    assert_eq!(bench.cache_files(), Vec::<PathBuf>::new());
    Ok(())
}

/// A JPEG carrying an EXIF APP1 segment with `orientation`.
fn jpeg_with_orientation(
    path: &Path,
    (width, height): (u32, u32),
    orientation: u16,
) -> Result<(), Box<dyn Error>> {
    let mut jpeg = Vec::new();
    image::RgbImage::from_pixel(width, height, image::Rgb([200, 30, 30])).write_to(
        &mut std::io::Cursor::new(&mut jpeg),
        image::ImageFormat::Jpeg,
    )?;
    // Little-endian TIFF with one IFD entry: 0x0112 Orientation, SHORT, 1.
    let mut tiff = b"II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
    tiff.extend_from_slice(&orientation.to_le_bytes());
    tiff.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let mut app1 = b"Exif\0\0".to_vec();
    app1.extend_from_slice(&tiff);
    let length = u16::try_from(app1.len() + 2)?;
    let mut out = jpeg.get(..2).ok_or("short jpeg")?.to_vec();
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(&app1);
    out.extend_from_slice(jpeg.get(2..).ok_or("short jpeg")?);
    fs::write(path, out)?;
    Ok(())
}

#[test]
fn a_generated_thumbnail_follows_the_exif_orientation() -> TestResult {
    let bench = Bench::new("exif")?;
    let image = bench.pictures.join("rotated.jpg");
    // Stored 400x200, EXIF 6 = rotate 90° clockwise: shown 200x400.
    jpeg_with_orientation(&image, (400, 200), 6)?;
    let Outcome::Ready(thumbnail) = bench.generator().ensure(&image) else {
        return Err("no thumbnail was generated".into());
    };
    let (width, height) = image::image_dimensions(&thumbnail)?;
    assert!(height > width, "the thumbnail is upright: {width}x{height}");
    Ok(())
}
