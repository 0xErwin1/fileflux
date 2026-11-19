//! The recently-used reader, against fixture files in a fresh scratch
//! directory. Nothing here touches the real `recently-used.xbel`.

use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use fileflux::core::EntryKind;
use fileflux::core::recent::{parse, read_recent, read_recent_capped, recent_path};

type TestResult = Result<(), Box<dyn Error>>;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Result<Self, Box<dyn Error>> {
        let dir =
            std::env::temp_dir().join(format!("fileflux-recent-{name}-{}", std::process::id()));
        drop(fs::remove_dir_all(&dir));
        fs::create_dir_all(&dir)?;
        Ok(Self(fs::canonicalize(dir)?))
    }

    fn file(&self, name: &str) -> Result<PathBuf, Box<dyn Error>> {
        let path = self.0.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, b"x")?;
        Ok(path)
    }

    fn xbel(&self, body: &str) -> Result<PathBuf, Box<dyn Error>> {
        let path = self.0.join("recently-used.xbel");
        fs::write(
            &path,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<xbel version=\"1.0\" \
                 xmlns:bookmark=\"http://www.freedesktop.org/standards/desktop-bookmarks\" \
                 xmlns:mime=\"http://www.freedesktop.org/standards/shared-mime-info\">\n{body}</xbel>\n"
            ),
        )?;
        Ok(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.0));
    }
}

fn bookmark(href: &str, modified: &str) -> String {
    format!(
        "  <bookmark href=\"{href}\" added=\"{modified}\" modified=\"{modified}\" visited=\"{modified}\">\n    \
         <info><metadata owner=\"http://freedesktop.org\"><mime:mime-type type=\"text/plain\"/></metadata></info>\n  </bookmark>\n"
    )
}

fn uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

#[test]
fn the_file_lives_under_xdg_data_home_else_home() {
    assert_eq!(
        recent_path(Some(OsStr::new("/h")), Some(OsStr::new("/d"))),
        Some(PathBuf::from("/d/recently-used.xbel"))
    );
    assert_eq!(
        recent_path(Some(OsStr::new("/h")), None),
        Some(PathBuf::from("/h/.local/share/recently-used.xbel"))
    );
    assert_eq!(
        recent_path(Some(OsStr::new("/h")), Some(OsStr::new("rel"))),
        Some(PathBuf::from("/h/.local/share/recently-used.xbel"))
    );
    assert_eq!(recent_path(None, None), None);
}

#[test]
fn files_come_newest_first_by_their_latest_timestamp() -> TestResult {
    let scratch = Scratch::new("order")?;
    let old = scratch.file("old.txt")?;
    let new = scratch.file("new.txt")?;
    let mid = scratch.file("mid.txt")?;
    let file = scratch.xbel(&format!(
        "{}{}{}",
        bookmark(&uri(&old), "2024-03-01T10:00:00Z"),
        bookmark(&uri(&new), "2024-05-01T10:00:00Z"),
        bookmark(&uri(&mid), "2024-04-01T10:00:00Z"),
    ))?;
    let listing = read_recent(&file)?;
    let names: Vec<_> = listing
        .entries
        .iter()
        .map(|e| e.display_name().into_owned())
        .collect();
    assert_eq!(names, ["new.txt", "mid.txt", "old.txt"]);
    assert!(listing.warnings.is_empty());
    Ok(())
}

#[test]
fn visited_counts_when_it_is_later_than_modified() {
    let xbel = b"<xbel><bookmark href=\"file:///a\" modified=\"2024-01-01T00:00:00Z\" visited=\"2024-06-01T00:00:00Z\"/>\
                 <bookmark href=\"file:///b\" modified=\"2024-03-01T00:00:00Z\"/></xbel>";
    let paths: Vec<_> = parse(xbel).used.into_iter().map(|used| used.path).collect();
    assert_eq!(paths, [PathBuf::from("/a"), PathBuf::from("/b")]);
}

#[test]
fn an_entry_shows_its_folder_and_when_it_was_used() -> TestResult {
    let scratch = Scratch::new("entry")?;
    let doc = scratch.file("docs/a.txt")?;
    let file = scratch.xbel(&bookmark(&uri(&doc), "2024-03-01T10:00:00Z"))?;
    let listing = read_recent(&file)?;
    let entry = listing.entries.first().ok_or("one entry")?;
    assert_eq!(entry.path, doc);
    assert_eq!(entry.kind, EntryKind::File);
    assert_eq!(entry.size, Some(1));
    assert_eq!(entry.origin.as_deref(), doc.parent());
    assert_eq!(
        entry.modified,
        Some(UNIX_EPOCH + Duration::from_secs(1_709_287_200))
    );
    Ok(())
}

#[test]
fn percent_encoded_hrefs_and_entities_decode() -> TestResult {
    let scratch = Scratch::new("encoding")?;
    let odd = scratch.file("my docs/caf\u{e9} & #1.txt")?;
    let encoded = format!(
        "file://{}/my%20docs/caf%C3%A9%20&amp;%20%231.txt",
        scratch.0.display()
    );
    let file = scratch.xbel(&bookmark(&encoded, "2024-03-01T10:00:00Z"))?;
    let listing = read_recent(&file)?;
    assert_eq!(
        listing
            .entries
            .iter()
            .map(|e| e.path.clone())
            .collect::<Vec<_>>(),
        [odd]
    );
    Ok(())
}

#[test]
fn missing_files_and_foreign_hrefs_are_skipped() -> TestResult {
    let scratch = Scratch::new("skipped")?;
    let kept = scratch.file("kept.txt")?;
    let file = scratch.xbel(&format!(
        "{}{}{}{}{}{}",
        bookmark(&uri(&scratch.0.join("gone.txt")), "2024-09-01T10:00:00Z"),
        bookmark("sftp://host/file.txt", "2024-09-01T10:00:00Z"),
        bookmark("https://example.org/a.txt", "2024-09-01T10:00:00Z"),
        bookmark("file://otherhost/etc/passwd", "2024-09-01T10:00:00Z"),
        bookmark("file:relative.txt", "2024-09-01T10:00:00Z"),
        bookmark(&uri(&kept), "2024-03-01T10:00:00Z"),
    ))?;
    let listing = read_recent(&file)?;
    assert_eq!(
        listing
            .entries
            .iter()
            .map(|e| e.path.clone())
            .collect::<Vec<_>>(),
        [kept]
    );
    Ok(())
}

#[test]
fn a_file_listed_twice_shows_once_at_its_latest_use() -> TestResult {
    let scratch = Scratch::new("dedupe")?;
    let a = scratch.file("a.txt")?;
    let b = scratch.file("b.txt")?;
    let file = scratch.xbel(&format!(
        "{}{}{}",
        bookmark(&uri(&a), "2024-01-01T10:00:00Z"),
        bookmark(&uri(&b), "2024-02-01T10:00:00Z"),
        bookmark(&uri(&a), "2024-03-01T10:00:00Z"),
    ))?;
    let listing = read_recent(&file)?;
    assert_eq!(
        listing
            .entries
            .iter()
            .map(|e| e.path.clone())
            .collect::<Vec<_>>(),
        [a, b]
    );
    Ok(())
}

#[test]
fn the_list_is_capped_after_dropping_missing_files() -> TestResult {
    let scratch = Scratch::new("cap")?;
    let mut body = String::new();
    // The newest ones are gone: the cap counts what exists.
    for day in 20..=25 {
        body.push_str(&bookmark(
            &uri(&scratch.0.join(format!("gone{day}"))),
            &format!("2024-03-{day}T10:00:00Z"),
        ));
    }
    for day in 1..=10 {
        let path = scratch.file(&format!("f{day:02}.txt"))?;
        body.push_str(&bookmark(
            &uri(&path),
            &format!("2024-03-{day:02}T10:00:00Z"),
        ));
    }
    let file = scratch.xbel(&body)?;
    let listing = read_recent_capped(&file, 3)?;
    let names: Vec<_> = listing
        .entries
        .iter()
        .map(|e| e.display_name().into_owned())
        .collect();
    assert_eq!(names, ["f10.txt", "f09.txt", "f08.txt"]);
    assert_eq!(read_recent(&file)?.entries.len(), 10);
    Ok(())
}

#[test]
fn a_missing_file_lists_nothing() -> TestResult {
    let scratch = Scratch::new("absent")?;
    let listing = read_recent(&scratch.0.join("recently-used.xbel"))?;
    assert!(listing.entries.is_empty());
    assert!(listing.warnings.is_empty());
    Ok(())
}

#[test]
fn malformed_xml_keeps_what_was_read_and_says_so() -> TestResult {
    let scratch = Scratch::new("malformed")?;
    let first = scratch.file("first.txt")?;
    let second = scratch.file("second.txt")?;
    let path = scratch.0.join("recently-used.xbel");
    fs::write(
        &path,
        format!(
            "<xbel>{}<bookmark href=\"{}\" modified=\"oops\"/><bookmark href=\"{}",
            bookmark(&uri(&first), "2024-03-01T10:00:00Z"),
            uri(&second),
            uri(&second)
        ),
    )?;
    let listing = read_recent(&path)?;
    let names: Vec<_> = listing
        .entries
        .iter()
        .map(|e| e.display_name().into_owned())
        .collect();
    assert_eq!(
        names,
        ["first.txt", "second.txt"],
        "the undated one sorts last"
    );
    assert_eq!(listing.warnings.len(), 1);

    fs::write(&path, b"\xff\xfe not xml at all <<<")?;
    let listing = read_recent(&path)?;
    assert!(listing.entries.is_empty());
    assert!(parse(b"").used.is_empty());
    assert!(parse(b"").problem.is_none());
    Ok(())
}
