//! Read-only access to the freedesktop home Trash. Reads only
//! `<root>/files` plus matching `<root>/info/<name>.trashinfo` metadata;
//! never creates directories, writes configuration, restores, deletes or
//! follows the recorded original paths.

use super::{Entry, EntryKind, Listing};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
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
