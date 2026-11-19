//! Read-only directory scanning for the Fileflux explorer.
//!
//! The scanner never opens file contents: it enumerates one directory level,
//! resolves entry kinds through metadata only, and reports partial failures as
//! explicit [`Listing::warnings`] instead of hiding entries.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::{self, FileType, Metadata};
use std::io::{self, Read as _};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::{Entry, EntryKind, Listing};

/// Scans one directory level and returns a deterministically sorted listing.
///
/// Sorting is folders-first, then case-insensitive by name with a raw-path tie
/// breaker (see [`sort_entries`]). Root-level failures (missing or unreadable
/// directory) return `Err`; per-entry enumeration or metadata failures become
/// [`Listing::warnings`] with the affected path. Broken symlinks stay in the
/// listing as [`EntryKind::Symlink`] without size/modified metadata.
pub fn read_directory(path: &Path) -> io::Result<Listing> {
    let mut listing = Listing::default();
    // A failed root read is the caller's navigation error, not a warning.
    let reader = fs::read_dir(path)?;
    for item in reader {
        let dir_entry = match item {
            Ok(entry) => entry,
            Err(err) => {
                listing.warnings.push(format!(
                    "could not enumerate an entry in {}: {err}",
                    path.display()
                ));
                continue;
            }
        };
        // In-flight copies and replaced originals are not user files.
        if crate::core::ops::is_transfer_temp(&dir_entry.file_name()) {
            continue;
        }
        match build_entry(dir_entry) {
            Ok(entry) => listing.entries.push(entry),
            Err(warning) => listing.warnings.push(warning),
        }
    }
    sort_entries(&mut listing.entries);
    listing.hidden_names = read_hidden_names(path);
    Ok(listing)
}

/// The most of a `.hidden` file that is read: a list of names, never large.
const HIDDEN_FILE_LIMIT: u64 = 1 << 20;

/// The names in `<dir>/.hidden`, one per line (`\n` or `\r\n`, blank lines
/// skipped, bytes kept as written). A missing, unreadable or non-regular
/// file lists nothing: it only ever hides, so failing to read it is not an
/// error of the folder.
fn read_hidden_names(dir: &Path) -> BTreeSet<OsString> {
    let Ok(file) = fs::File::open(dir.join(".hidden")) else {
        return BTreeSet::new();
    };
    let mut bytes = Vec::new();
    if !file.metadata().is_ok_and(|meta| meta.is_file())
        || file
            .take(HIDDEN_FILE_LIMIT)
            .read_to_end(&mut bytes)
            .is_err()
    {
        return BTreeSet::new();
    }
    bytes
        .split(|&byte| byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| !line.is_empty())
        .map(|line| OsStr::from_bytes(line).to_os_string())
        .collect()
}

/// Sorts entries folders-first, then by natural name order
/// ([`natural_cmp`]), raw path tie. The order is independent of locale and
/// deterministic for non-UTF8 names. Equal entries keep their order.
///
/// A million names sort in a fraction of a second: each entry gets a
/// [`natural_sort_prefix`] that decides almost every comparison without
/// reading the names, and the sort works on those and moves the entries
/// once, at the end.
pub fn sort_entries(entries: &mut [Entry]) {
    let mut order: Vec<(bool, u128, usize)> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            (
                !entry.is_directory(),
                natural_sort_prefix(&entry.name.to_string_lossy()),
                index,
            )
        })
        .collect();
    let exact = |a: usize, b: usize| match (entries.get(a), entries.get(b)) {
        (Some(a), Some(b)) => natural_cmp(&a.name, &b.name).then_with(|| a.path.cmp(&b.path)),
        _ => Ordering::Equal,
    };
    order.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| compare_sort_prefixes(a.1, b.1).unwrap_or_else(|| exact(a.2, b.2)))
    });
    // Put each entry at its place, one cycle of the permutation at a time.
    let mut placed = vec![false; entries.len()];
    for start in 0..entries.len() {
        let mut at = start;
        while !placed.get(at).copied().unwrap_or(true) {
            if let Some(slot) = placed.get_mut(at) {
                *slot = true;
            }
            let Some(&(_, _, source)) = order.get(at) else {
                break;
            };
            if source == start {
                break;
            }
            entries.swap(at, source);
            at = source;
        }
    }
}

/// The byte of a [`natural_sort_prefix`] past a character the prefix does not
/// spell out (a non-ASCII character, or a number too long to be sized).
const STOPPED: u8 = 0xFF;
/// The byte that opens a number in a [`natural_sort_prefix`]: the count of
/// its significant digits follows, then the digits.
const NUMBER: u8 = 0xFE;
/// The first count a number cannot have in a prefix.
const LONGEST_NUMBER: u8 = 0xFD;
const PREFIX_BYTES: usize = 16;

/// A fixed-size summary of `text` for the natural order ([`natural_str_cmp`]),
/// as a big-endian integer of its first sixteen bytes: ASCII characters
/// lowercased, a run of digits as [`NUMBER`], the count of its digits without
/// leading zeros and those digits (so numbers order by value), and a name
/// that ends padded with zeros, which order before any character. The first
/// non-ASCII character, or a number of 253 digits or more, stops it: every
/// byte after is `0xFF`. Compare two summaries with
/// [`compare_sort_prefixes`], which says when they decide the order.
pub fn natural_sort_prefix(text: &str) -> u128 {
    let mut bytes = [0u8; PREFIX_BYTES];
    let mut written = 0;
    let mut put = |byte: u8| {
        if let Some(slot) = bytes.get_mut(written) {
            *slot = byte;
            written += 1;
        }
    };
    let mut rest = text.as_bytes();
    while let Some(&first) = rest.first() {
        if first.is_ascii_digit() {
            let end = rest
                .iter()
                .position(|byte| !byte.is_ascii_digit())
                .unwrap_or(rest.len());
            let (run, tail) = rest.split_at(end);
            let zeros = run.iter().take_while(|&&byte| byte == b'0').count();
            let digits = run.get(zeros..).unwrap_or_default();
            match u8::try_from(digits.len()) {
                Ok(count) if count < LONGEST_NUMBER => {
                    put(NUMBER);
                    put(count);
                    digits.iter().for_each(|&digit| put(digit));
                }
                _ => {
                    (0..PREFIX_BYTES).for_each(|_| put(STOPPED));
                    break;
                }
            }
            rest = tail;
        } else if first.is_ascii() {
            put(first.to_ascii_lowercase());
            rest = rest.get(1..).unwrap_or_default();
        } else {
            (0..PREFIX_BYTES).for_each(|_| put(STOPPED));
            break;
        }
    }
    u128::from_be_bytes(bytes)
}

/// The order of two names from their prefixes, when they decide it: at the
/// first byte where the prefixes differ, one name ended there (an ended name
/// is the lesser), or both spell out a character or a number's digits.
/// `None` when the prefixes are equal, or differ at a stop or at a number
/// against a character: then only the full comparison knows. Whenever this
/// returns `Some`, it is the order [`natural_str_cmp`] gives (the raw-bytes
/// tie of [`natural_cmp`] only matters between equal prefixes).
///
/// Summaries made as the lowercase text's first bytes (for a plain,
/// non-natural order) work as well: UTF-8 never contains the stop bytes.
pub fn compare_sort_prefixes(a: u128, b: u128) -> Option<Ordering> {
    let differing = a ^ b;
    if differing == 0 {
        return None;
    }
    let shift = 120 - (differing.leading_zeros() / 8) * 8;
    let byte = |prefix: u128| ((prefix >> shift) & 0xFF) as u8;
    let (mine, theirs) = (byte(a), byte(b));
    let spelled = |byte: u8| byte < NUMBER;
    (mine == 0 || theirs == 0 || (spelled(mine) && spelled(theirs))).then(|| mine.cmp(&theirs))
}

/// Compares two names for display: case-insensitively, with runs of ASCII
/// digits ordered by their number (`IMG_2` before `IMG_10`, however long
/// the run). Names are compared through their lossy text, so non-UTF-8 names
/// are safe; a number with leading zeros follows the same number without
/// them, and names that are still equal fall back to their raw bytes, so the
/// order is total and deterministic.
pub fn natural_cmp(a: &OsStr, b: &OsStr) -> Ordering {
    natural_str_cmp(&a.to_string_lossy(), &b.to_string_lossy()).then_with(|| a.cmp(b))
}

/// The text part of [`natural_cmp`], without the raw-bytes tie: letters
/// compare case-insensitively and digit runs by value, so `a` and `A` are
/// `Equal` here. Allocation free: the digit runs are borrowed slices.
pub fn natural_str_cmp(a: &str, b: &str) -> Ordering {
    let (mut left, mut right) = (a, b);
    // The first difference in leading zeros between numerically equal runs.
    let mut zeros = Ordering::Equal;
    loop {
        match (left.as_bytes().first(), right.as_bytes().first()) {
            (None, None) => return zeros,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let (run_a, rest_a) = split_digits(left);
                let (run_b, rest_b) = split_digits(right);
                let (number_a, number_b) =
                    (run_a.trim_start_matches('0'), run_b.trim_start_matches('0'));
                let by_number = number_a
                    .len()
                    .cmp(&number_b.len())
                    .then_with(|| number_a.cmp(number_b));
                if by_number != Ordering::Equal {
                    return by_number;
                }
                if zeros == Ordering::Equal {
                    zeros = run_a.len().cmp(&run_b.len());
                }
                (left, right) = (rest_a, rest_b);
            }
            // Two ASCII characters: their lowercase is one ASCII character.
            (Some(x), Some(y)) if x.is_ascii() && y.is_ascii() => {
                let by_char = x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase());
                if by_char != Ordering::Equal {
                    return by_char;
                }
                left = left.get(1..).unwrap_or_default();
                right = right.get(1..).unwrap_or_default();
            }
            (Some(_), Some(_)) => {
                let (Some(x), Some(y)) = (left.chars().next(), right.chars().next()) else {
                    return Ordering::Equal;
                };
                let by_char = x.to_lowercase().cmp(y.to_lowercase());
                if by_char != Ordering::Equal {
                    return by_char;
                }
                left = left.get(x.len_utf8()..).unwrap_or_default();
                right = right.get(y.len_utf8()..).unwrap_or_default();
            }
        }
    }
}

/// Splits the leading run of ASCII digits off `text`.
fn split_digits(text: &str) -> (&str, &str) {
    let end = text
        .bytes()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(text.len());
    text.split_at(end)
}

/// Returns the indices of entries whose local name contains `query`
/// case-insensitively. The filter is purely local: no recursion and no
/// content search. An empty query selects every entry, in listing order.
pub fn filtered_indices(entries: &[Entry], query: &str) -> Vec<usize> {
    let needle = query.to_lowercase();
    entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            entry
                .name
                .to_string_lossy()
                .to_lowercase()
                .contains(&needle)
        })
        .map(|(index, _)| index)
        .collect()
}

/// The unix permission bits (`st_mode`) of `path`, following symlinks, or
/// `None` on platforms without them. One `stat`: callers run it off the UI
/// thread, only for the entry the details panel shows.
pub fn read_mode(path: &Path) -> io::Result<Option<u32>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Ok(Some(fs::metadata(path)?.mode()))
    }
    #[cfg(not(unix))]
    {
        fs::metadata(path).map(|_| None)
    }
}

/// How many entries `path` holds, or `None` when it cannot be read as a
/// directory. One `read_dir` pass that only counts: nothing is opened, and
/// callers run it off the UI thread. Hidden entries count.
pub fn count_children(path: &Path) -> Option<usize> {
    let reader = fs::read_dir(path).ok()?;
    let mut count = 0usize;
    for item in reader {
        item.ok()?;
        count += 1;
    }
    Some(count)
}

/// The item count of every directory in `entries`, keyed by its path; an
/// unreadable directory maps to `None`. Files are not listed. Blocking:
/// run it on the background executor, after the listing has been shown.
pub fn child_counts(entries: &[Entry]) -> HashMap<PathBuf, Option<usize>> {
    entries
        .iter()
        .filter(|entry| entry.is_directory())
        .map(|entry| (entry.path.clone(), count_children(&entry.path)))
        .collect()
}

/// Builds one listing entry; `Err` carries a ready-to-push warning string.
fn build_entry(dir_entry: fs::DirEntry) -> Result<Entry, String> {
    let name: OsString = dir_entry.file_name();
    let path: PathBuf = dir_entry.path();
    let file_type: FileType = dir_entry
        .file_type()
        .map_err(|err| format!("could not read type of {}: {err}", path.display()))?;
    let is_symlink = file_type.is_symlink();

    let (kind, size, modified) = if is_symlink {
        // Resolve the target kind through metadata only; never open contents.
        match fs::metadata(&path) {
            Ok(meta) => metadata_facts(&meta),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // Broken link: keep it explicit rather than hiding it.
                (EntryKind::Symlink, None, None)
            }
            Err(err) => {
                return Err(format!(
                    "could not resolve symlink {}: {err}",
                    path.display()
                ));
            }
        }
    } else {
        match dir_entry.metadata() {
            Ok(meta) => metadata_facts(&meta),
            Err(err) => {
                return Err(format!(
                    "could not read metadata for {}: {err}",
                    path.display()
                ));
            }
        }
    };

    Ok(Entry {
        name,
        path,
        kind,
        is_symlink,
        size,
        modified,
        origin: None,
    })
}

/// `(kind, size, modified)` facts for an already-followed metadata read.
fn metadata_facts(meta: &Metadata) -> (EntryKind, Option<u64>, Option<SystemTime>) {
    let kind = if meta.is_dir() {
        EntryKind::Directory
    } else if meta.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    // Directory sizes are not meaningful for the explorer view.
    let size = if kind == EntryKind::Directory {
        None
    } else {
        Some(meta.len())
    };
    (kind, size, meta.modified().ok())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::symlink as symlink_file;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;

    use super::super::{Entry, EntryKind, Listing};
    use super::{child_counts, count_children, filtered_indices, read_directory, read_mode};

    /// RAII scratch directory under the system temp dir; removed on drop.
    struct Scratch {
        root: PathBuf,
    }

    static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

    fn scratch(tag: &str) -> std::io::Result<Scratch> {
        let unique = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "fileflux-fs-test-{tag}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).map_err(|err| {
            std::io::Error::new(err.kind(), format!("create scratch root: {err}"))
        })?;
        Ok(Scratch { root })
    }

    impl Scratch {
        fn path(&self) -> &Path {
            &self.root
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.root) {
                eprintln!(
                    "could not remove test fixture {}: {error}",
                    self.root.display()
                );
            }
        }
    }

    fn entry_names(listing: &Listing) -> Vec<String> {
        listing
            .entries
            .iter()
            .map(|entry| entry.display_name().into_owned())
            .collect()
    }

    /// Looks one entry up by exact name; `Err` names the missing entry.
    fn find<'a>(listing: &'a Listing, name: &str) -> Result<&'a Entry, String> {
        listing
            .entries
            .iter()
            .find(|entry| entry.name == OsStr::new(name))
            .ok_or_else(|| format!("entry {name:?} missing from listing"))
    }

    #[test]
    fn read_directory_lists_mixed_fixture_dirs_and_files_with_metadata()
    -> Result<(), Box<dyn std::error::Error>> {
        let scratch = scratch("mixed")?;
        fs::create_dir(scratch.path().join("beta_dir"))?;
        fs::create_dir(scratch.path().join("Alpha_dir"))?;
        fs::write(scratch.path().join("zeta.txt"), "payload")?;
        fs::write(scratch.path().join("empty.txt"), "")?;

        let listing = read_directory(scratch.path())?;

        assert!(
            listing.warnings.is_empty(),
            "fixture scan must be warning-free: {:?}",
            listing.warnings
        );
        assert_eq!(listing.entries.len(), 4);
        // Folders first, then files, both case-insensitive by name.
        assert_eq!(
            entry_names(&listing),
            ["Alpha_dir", "beta_dir", "empty.txt", "zeta.txt"]
        );
        let alpha = find(&listing, "Alpha_dir")?;
        assert!(alpha.is_directory());
        assert_eq!(alpha.kind, EntryKind::Directory);
        let beta = find(&listing, "beta_dir")?;
        assert_eq!(beta.kind, EntryKind::Directory);
        assert_eq!(beta.size, None, "directory size is None");
        let zeta = find(&listing, "zeta.txt")?;
        assert_eq!(zeta.kind, EntryKind::File);
        assert!(!zeta.is_symlink);
        assert_eq!(zeta.size, Some("payload".len() as u64));
        assert!(
            zeta.modified
                .is_some_and(|modified| modified <= SystemTime::now()),
            "file modified time resolves to a past stamp"
        );
        let empty = find(&listing, "empty.txt")?;
        assert_eq!(empty.size, Some(0));
        assert!(zeta.origin.is_none(), "fs entries carry no trash origin");
        assert_eq!(
            zeta.path,
            scratch.path().join("zeta.txt"),
            "paths keep raw filesystem form"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn read_mode_reports_permission_bits_and_missing_paths()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt as _;

        let scratch = scratch("mode")?;
        let file = scratch.path().join("f.txt");
        fs::write(&file, "x")?;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640))?;

        let mode = read_mode(&file)?.ok_or("unix reports a mode")?;
        assert_eq!(mode & 0o7777, 0o640);

        let missing = read_mode(&scratch.path().join("nope"));
        assert_eq!(
            missing.err().map(|err| err.kind()),
            Some(std::io::ErrorKind::NotFound)
        );
        Ok(())
    }

    #[test]
    fn filtered_indices_match_case_insensitive_local_names_without_recursion()
    -> Result<(), Box<dyn std::error::Error>> {
        let scratch = scratch("filter")?;
        fs::create_dir(scratch.path().join("Reports"))?;
        fs::create_dir(scratch.path().join("Notes"))?;
        fs::write(scratch.path().join("notes.txt"), "x")?;
        // A matching name nested one level down must never appear.
        fs::write(scratch.path().join("Reports").join("notes.txt"), "y")?;

        let listing = read_directory(scratch.path())?;

        let hits = filtered_indices(&listing.entries, "NOTES");
        assert_eq!(hits.len(), 2, "query matches both local notes entries");
        assert_eq!(
            entry_names(&listing),
            ["Notes", "Reports", "notes.txt"],
            "folders sort first"
        );
        for &index in &hits {
            let entry = listing
                .entries
                .get(index)
                .ok_or("hit index outside listing range")?;
            let name = entry.display_name().to_lowercase();
            assert!(name.contains("notes"));
        }
        // Empty query keeps every entry, in listing order.
        let all = filtered_indices(&listing.entries, "");
        assert_eq!(all, vec![0, 1, 2]);
        // No recursion: the nested fixture file is absent from the listing.
        assert!(
            listing
                .entries
                .iter()
                .all(|entry| entry.path.parent() == Some(scratch.path()))
        );
        Ok(())
    }

    #[test]
    fn read_directory_hides_in_flight_transfer_files() -> Result<(), Box<dyn std::error::Error>> {
        let scratch = scratch("in-flight")?;
        fs::write(scratch.path().join(".fileflux-partial-42-1"), b"half")?;
        fs::write(scratch.path().join(".fileflux-aside-42-2"), b"old")?;
        fs::write(scratch.path().join(".fileflux-notes"), b"user file")?;
        fs::write(scratch.path().join("report.pdf"), b"pdf")?;

        let listing = read_directory(scratch.path())?;

        let names: Vec<_> = listing.entries.iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, vec![".fileflux-notes", "report.pdf"]);
        Ok(())
    }

    #[test]
    fn read_directory_errors_on_missing_directory_without_changes()
    -> Result<(), Box<dyn std::error::Error>> {
        let scratch = scratch("missing")?;
        let missing = scratch.path().join("does-not-exist");

        let result = read_directory(&missing);

        let Err(err) = result else {
            return Err("missing directory must return Err".into());
        };
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(!missing.exists(), "failed scan must not create anything");
        assert_eq!(fs::read_dir(scratch.path())?.count(), 0);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_resolve_target_kind_and_broken_links_stay_explicit()
    -> Result<(), Box<dyn std::error::Error>> {
        let scratch = scratch("symlink")?;
        fs::create_dir(scratch.path().join("target_dir"))?;
        fs::write(scratch.path().join("target_file"), "abc")?;
        symlink_file("target_file", scratch.path().join("link_file"))?;
        symlink_file("target_dir", scratch.path().join("link_dir"))?;
        symlink_file("nowhere", scratch.path().join("link_broken"))?;

        let listing = read_directory(scratch.path())?;

        let link_file = find(&listing, "link_file")?;
        assert!(link_file.is_symlink);
        assert_eq!(link_file.kind, EntryKind::File, "kind resolves the target");
        assert_eq!(link_file.size, Some(3));
        assert!(link_file.modified.is_some());

        let link_dir = find(&listing, "link_dir")?;
        assert!(link_dir.is_symlink);
        assert_eq!(link_dir.kind, EntryKind::Directory);
        assert_eq!(link_dir.size, None);

        let broken = find(&listing, "link_broken")?;
        assert!(broken.is_symlink, "broken link keeps is_symlink");
        assert_eq!(broken.kind, EntryKind::Symlink, "broken link is explicit");
        assert_eq!(broken.size, None, "broken link has missing metadata");
        assert_eq!(broken.modified, None);
        assert!(
            listing.warnings.is_empty(),
            "broken links are explicit entries, not warnings: {:?}",
            listing.warnings
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_names_keep_raw_paths_for_navigation() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::ffi::OsStrExt;

        let scratch = scratch("nonutf8")?;
        let raw = OsStr::from_bytes(b"bad\xff\xfename").to_os_string();
        fs::write(scratch.path().join(&raw), "z")?;

        let listing = read_directory(scratch.path())?;

        assert_eq!(listing.entries.len(), 1);
        // Evaluated without binding to prove lossy display does not panic.
        let entry = listing
            .entries
            .first()
            .ok_or("expected exactly one entry")?;
        assert_eq!(entry.name, raw, "raw OsString name is preserved");
        assert_eq!(entry.path.file_name(), Some(raw.as_os_str()));
        entry.display_name(); // lossy display must not panic
        let hits = filtered_indices(&listing.entries, "bad");
        assert_eq!(hits, vec![0], "ASCII query still finds the raw entry");
        assert_eq!(entry.kind, EntryKind::File);
        Ok(())
    }
    #[test]
    fn a_hidden_file_lists_the_names_hidden_in_that_folder()
    -> Result<(), Box<dyn std::error::Error>> {
        let scratch = scratch("dot-hidden")?;
        fs::write(scratch.path().join("secret.txt"), "s")?;
        fs::write(scratch.path().join("shown.txt"), "s")?;
        fs::create_dir(scratch.path().join("Private Folder"))?;
        fs::write(
            scratch.path().join(".hidden"),
            b"secret.txt

Private Folder
not-there
  padded 
sub/nested
last",
        )?;
        let listing = read_directory(scratch.path())?;
        let names: Vec<String> = listing
            .hidden_names
            .iter()
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            [
                "  padded ",
                "Private Folder",
                "last",
                "not-there",
                "secret.txt",
                "sub/nested"
            ],
            "one name per line, CR and blank lines dropped, nothing trimmed or filtered"
        );
        assert_eq!(
            listing.entries.len(),
            4,
            "hidden entries stay in the listing"
        );
        Ok(())
    }

    #[test]
    fn hidden_names_keep_non_utf8_bytes_and_a_folder_without_the_file_has_none()
    -> Result<(), Box<dyn std::error::Error>> {
        let scratch = scratch("dot-hidden-bytes")?;
        assert!(read_directory(scratch.path())?.hidden_names.is_empty());
        fs::write(scratch.path().join(".hidden"), b"caf\xe9\n")?;
        let listing = read_directory(scratch.path())?;
        assert!(
            listing.hidden_names.contains(OsStr::from_bytes(b"caf\xe9")),
            "{:?}",
            listing.hidden_names
        );
        // A directory named .hidden is not a list.
        let other = scratch.path().join("other");
        fs::create_dir_all(other.join(".hidden"))?;
        let listing = read_directory(&other)?;
        assert!(listing.hidden_names.is_empty());
        assert_eq!(listing.entries.len(), 1);
        Ok(())
    }

    #[test]
    fn child_counts_counts_each_directory_and_reports_unreadable_as_none()
    -> Result<(), Box<dyn std::error::Error>> {
        let scratch = scratch("counts")?;
        let full = scratch.path().join("full");
        fs::create_dir(&full)?;
        fs::write(full.join("a.txt"), "a")?;
        fs::write(full.join(".hidden"), "h")?;
        fs::create_dir(full.join("nested"))?;
        let empty = scratch.path().join("empty");
        fs::create_dir(&empty)?;
        fs::write(scratch.path().join("plain.txt"), "p")?;

        assert_eq!(count_children(&full), Some(3), "hidden entries count too");
        assert_eq!(count_children(&empty), Some(0));
        assert_eq!(
            count_children(&scratch.path().join("plain.txt")),
            None,
            "a path that cannot be read as a directory has no count"
        );
        assert_eq!(count_children(&scratch.path().join("missing")), None);

        let listing = read_directory(scratch.path())?;
        let counts = child_counts(&listing.entries);
        assert_eq!(counts.get(&full), Some(&Some(3)));
        assert_eq!(counts.get(&empty), Some(&Some(0)));
        assert!(
            !counts.contains_key(&scratch.path().join("plain.txt")),
            "files are never counted"
        );
        Ok(())
    }

    mod natural {
        use super::super::natural_cmp;
        use std::cmp::Ordering;
        use std::ffi::OsStr;

        fn sorted(names: &[&str]) -> Vec<String> {
            let mut list: Vec<&OsStr> = names.iter().map(OsStr::new).collect();
            list.sort_by(|a, b| natural_cmp(a, b));
            list.iter()
                .map(|n| n.to_string_lossy().into_owned())
                .collect()
        }

        #[test]
        fn digit_runs_order_by_number() {
            assert_eq!(
                sorted(&["IMG_10", "IMG_2", "IMG_1", "IMG_100"]),
                ["IMG_1", "IMG_2", "IMG_10", "IMG_100"]
            );
            assert_eq!(
                sorted(&["a10b2", "a10b10", "a9b99"]),
                ["a9b99", "a10b2", "a10b10"]
            );
        }

        #[test]
        fn case_is_ignored_before_the_tie_break() {
            assert_eq!(sorted(&["b", "A", "a", "B"]), ["A", "a", "B", "b"]);
            assert_eq!(
                natural_cmp(OsStr::new("img_2"), OsStr::new("IMG_10")),
                Ordering::Less
            );
        }

        #[test]
        fn leading_zeros_do_not_change_the_number_but_break_ties() {
            assert_eq!(
                sorted(&["f02", "f1", "f001", "f2"]),
                ["f1", "f001", "f2", "f02"]
            );
            assert_eq!(
                natural_cmp(OsStr::new("a007"), OsStr::new("a8")),
                Ordering::Less
            );
        }

        #[test]
        fn huge_numbers_never_overflow() {
            let big = "n99999999999999999999999999";
            let bigger = "n100000000000000000000000000";
            assert_eq!(
                natural_cmp(OsStr::new(big), OsStr::new(bigger)),
                Ordering::Less
            );
        }

        #[test]
        fn mixed_digits_and_letters_compare_by_character() {
            assert_eq!(sorted(&["a1", "ab", "a-"]), ["a-", "a1", "ab"]);
            assert_eq!(sorted(&["file", "file1"]), ["file", "file1"]);
        }

        #[test]
        fn unicode_names_fold_case() {
            assert_eq!(
                natural_cmp(OsStr::new("Éclair"), OsStr::new("éclair")),
                Ordering::Less
            );
            assert_eq!(sorted(&["ñ2", "ñ10", "Ñ1"]), ["Ñ1", "ñ2", "ñ10"]);
        }

        #[test]
        fn equal_names_tie_break_by_raw_bytes() {
            assert_eq!(
                natural_cmp(OsStr::new("a"), OsStr::new("a")),
                Ordering::Equal
            );
            assert_eq!(
                natural_cmp(OsStr::new("A"), OsStr::new("a")),
                Ordering::Less
            );
        }

        #[cfg(unix)]
        #[test]
        fn non_utf8_names_are_ordered_deterministically() {
            use std::os::unix::ffi::OsStrExt as _;
            let bad_a = OsStr::from_bytes(b"x\xff1");
            let bad_b = OsStr::from_bytes(b"x\xfe1");
            assert_ne!(natural_cmp(bad_a, bad_b), Ordering::Equal);
            assert_eq!(natural_cmp(bad_a, bad_b), natural_cmp(bad_a, bad_b));
            assert_eq!(
                natural_cmp(bad_a, bad_b),
                natural_cmp(bad_b, bad_a).reverse()
            );
            let mut names = [bad_a, OsStr::new("x2"), bad_b, OsStr::new("x10")];
            names.sort_by(|a, b| natural_cmp(a, b));
            let mut again = names;
            again.reverse();
            again.sort_by(|a, b| natural_cmp(a, b));
            assert_eq!(names, again, "the order does not depend on the input order");
        }
    }

    mod prefixes {
        use super::super::{
            compare_sort_prefixes, natural_cmp, natural_sort_prefix, natural_str_cmp, sort_entries,
        };
        use crate::core::{Entry, EntryKind};
        use std::cmp::Ordering;
        use std::path::PathBuf;

        /// A tiny xorshift64*: deterministic and dependency free.
        struct Rng(u64);

        impl Rng {
            fn next(&mut self) -> u64 {
                self.0 ^= self.0 >> 12;
                self.0 ^= self.0 << 25;
                self.0 ^= self.0 >> 27;
                self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
            }

            fn name(&mut self) -> String {
                const PIECES: [&str; 16] = [
                    "a",
                    "A",
                    "b",
                    "IMG_",
                    "img",
                    "1",
                    "01",
                    "007",
                    "2",
                    "10",
                    "9",
                    "é",
                    "İ",
                    "_",
                    " ",
                    "99999999999999999999",
                ];
                let count = 1 + self.next() % 6;
                (0..count)
                    .map(|_| {
                        PIECES
                            .get((self.next() % 16) as usize)
                            .copied()
                            .unwrap_or("x")
                    })
                    .collect()
            }
        }

        /// The natural comparison as it was first written: characters and
        /// owned digit runs, nothing clever.
        fn plain_natural(a: &str, b: &str) -> Ordering {
            let (mut left, mut right) = (a.chars().peekable(), b.chars().peekable());
            let mut zeros = Ordering::Equal;
            loop {
                match (left.peek().copied(), right.peek().copied()) {
                    (None, None) => return zeros,
                    (None, Some(_)) => return Ordering::Less,
                    (Some(_), None) => return Ordering::Greater,
                    (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                        let run = |chars: &mut std::iter::Peekable<std::str::Chars<'_>>| {
                            let mut run = String::new();
                            while let Some(digit) = chars.next_if(char::is_ascii_digit) {
                                run.push(digit);
                            }
                            run
                        };
                        let (run_a, run_b) = (run(&mut left), run(&mut right));
                        let (number_a, number_b) =
                            (run_a.trim_start_matches('0'), run_b.trim_start_matches('0'));
                        let by_number = number_a
                            .len()
                            .cmp(&number_b.len())
                            .then_with(|| number_a.cmp(number_b));
                        if by_number != Ordering::Equal {
                            return by_number;
                        }
                        if zeros == Ordering::Equal {
                            zeros = run_a.len().cmp(&run_b.len());
                        }
                    }
                    (Some(x), Some(y)) => {
                        let by_char = x.to_lowercase().cmp(y.to_lowercase());
                        if by_char != Ordering::Equal {
                            return by_char;
                        }
                        left.next();
                        right.next();
                    }
                }
            }
        }

        #[test]
        fn the_fast_natural_comparison_equals_the_plain_one() {
            let mut rng = Rng(0xBEEF);
            for _ in 0..40_000 {
                let (a, b) = (rng.name(), rng.name());
                assert_eq!(
                    natural_str_cmp(&a, &b),
                    plain_natural(&a, &b),
                    "{a:?} {b:?}"
                );
            }
        }

        #[test]
        fn a_deciding_prefix_comparison_is_the_natural_comparison() {
            let mut rng = Rng(0x5EED);
            let (mut decided, mut total) = (0, 0);
            for _ in 0..40_000 {
                let (a, b) = (rng.name(), rng.name());
                total += 1;
                if let Some(order) =
                    compare_sort_prefixes(natural_sort_prefix(&a), natural_sort_prefix(&b))
                {
                    decided += 1;
                    assert_eq!(order, natural_str_cmp(&a, &b), "{a:?} against {b:?}");
                }
            }
            assert!(
                decided * 4 > total,
                "the prefixes should decide a good share of the pairs ({decided} of {total})"
            );
        }

        #[test]
        fn numbers_compare_by_value_inside_the_prefix() {
            let before = |a: &str, b: &str| {
                compare_sort_prefixes(natural_sort_prefix(a), natural_sort_prefix(b))
            };
            assert_eq!(before("img2", "img10"), Some(Ordering::Less));
            assert_eq!(before("img10", "img9"), Some(Ordering::Greater));
            assert_eq!(
                before("img", "img1"),
                Some(Ordering::Less),
                "ended is lesser"
            );
            assert_eq!(
                before("IMG1", "img1"),
                None,
                "case is a tie for the full order"
            );
            assert_eq!(before("a007", "a7"), None, "leading zeros are a tie");
            assert_eq!(
                before("a1", "ab"),
                None,
                "a number against a letter is exact work"
            );
            assert_eq!(
                before("é1", "é2"),
                None,
                "past a non-ASCII character it stops"
            );
        }

        fn entry(name: &str, kind: EntryKind, parent: &str) -> Entry {
            Entry {
                name: name.into(),
                path: PathBuf::from(parent).join(name),
                kind,
                is_symlink: false,
                size: None,
                modified: None,
                origin: None,
            }
        }

        #[test]
        fn sort_entries_orders_like_a_plain_comparison_sort() {
            let mut rng = Rng(0xFEED);
            for _ in 0..20 {
                let mut entries: Vec<Entry> = (0..300)
                    .map(|_| {
                        let kind = if rng.next().is_multiple_of(4) {
                            EntryKind::Directory
                        } else {
                            EntryKind::File
                        };
                        let parent = if rng.next().is_multiple_of(5) {
                            "/b"
                        } else {
                            "/a"
                        };
                        entry(&rng.name(), kind, parent)
                    })
                    .collect();
                let mut expected = entries.clone();
                expected.sort_by(|a, b| {
                    b.is_directory()
                        .cmp(&a.is_directory())
                        .then_with(|| natural_cmp(&a.name, &b.name))
                        .then_with(|| a.path.cmp(&b.path))
                });
                sort_entries(&mut entries);
                let paths = |list: &[Entry]| {
                    list.iter()
                        .map(|entry| (entry.path.clone(), entry.kind))
                        .collect::<Vec<_>>()
                };
                assert_eq!(paths(&entries), paths(&expected));
            }
        }
    }
}
