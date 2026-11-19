//! The freedesktop thumbnail cache reader, against fixture caches in a fresh
//! scratch directory. Nothing here touches the real `~/.cache/thumbnails`.

use std::error::Error;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fileflux::core::thumbnails::{Memo, cache_root, find, thumbnail_file_name, thumbnail_uri};

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
