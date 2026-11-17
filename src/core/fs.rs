//! Read-only directory scanning for the Fileflux explorer.
//!
//! The scanner never opens file contents: it enumerates one directory level,
//! resolves entry kinds through metadata only, and reports partial failures as
//! explicit [`Listing::warnings`] instead of hiding entries.

use std::ffi::OsString;
use std::fs::{self, FileType, Metadata};
use std::io;
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
        match build_entry(dir_entry) {
            Ok(entry) => listing.entries.push(entry),
            Err(warning) => listing.warnings.push(warning),
        }
    }
    sort_entries(&mut listing.entries);
    Ok(listing)
}

/// Sorts entries folders-first, then case-insensitive by name, raw path tie.
///
/// Comparisons use the lossy lowercase display name, so the order is stable
/// and independent of locale; equal names fall back to the raw [`PathBuf`]
/// bytes so non-UTF8 names keep a deterministic position.
pub fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.is_directory()
            .cmp(&a.is_directory())
            .then_with(|| {
                a.name
                    .to_string_lossy()
                    .to_lowercase()
                    .cmp(&b.name.to_string_lossy().to_lowercase())
            })
            .then_with(|| a.path.cmp(&b.path))
    });
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
    use std::os::unix::fs::symlink as symlink_file;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;

    use super::super::{Entry, EntryKind, Listing};
    use super::{filtered_indices, read_directory};

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
}
