//! Recursive folder size: the bytes a folder occupies on disk.
//!
//! The accounting rules are the disk usage analyzer's (docs/roadmap.md):
//! allocated blocks (`st_blocks * 512`), not the apparent size; each
//! `(device, inode)` pair once, so hard links count once; symbolic links are
//! never followed (a link counts its own blocks); the walk stays on the
//! starting filesystem; a folder that cannot be read is skipped and flagged
//! [`DiskUsage::incomplete`] instead of aborting. The walk is blocking, so
//! callers run it off the UI thread; it checks a cancel flag per entry and
//! publishes running totals through [`Progress`].

use std::collections::HashSet;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The totals of a walk, final or partial.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiskUsage {
    /// Allocated bytes (`st_blocks * 512`), the folder's own block included.
    pub bytes: u64,
    /// Entries below the folder that are not folders (files, links, ...).
    pub files: u64,
    /// Folders below the folder, not counting it.
    pub folders: u64,
    /// Some folder could not be read, so the totals are a lower bound.
    pub incomplete: bool,
}

/// How a finished walk ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkEnd {
    Done(DiskUsage),
    /// The cancel flag was raised; the totals so far stay in [`Progress`].
    Cancelled,
}

/// Running totals shared with the thread that shows them.
#[derive(Debug, Default)]
pub struct Progress {
    bytes: AtomicU64,
    files: AtomicU64,
    folders: AtomicU64,
    incomplete: AtomicBool,
    finished: AtomicBool,
}

impl Progress {
    /// The totals counted so far.
    #[must_use]
    pub fn snapshot(&self) -> DiskUsage {
        DiskUsage {
            bytes: self.bytes.load(Ordering::Relaxed),
            files: self.files.load(Ordering::Relaxed),
            folders: self.folders.load(Ordering::Relaxed),
            incomplete: self.incomplete.load(Ordering::Relaxed),
        }
    }

    /// Whether the walk has returned (done, cancelled or failed).
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}

/// Walks `root`, publishing totals to `progress`. Fails only when `root`
/// itself cannot be inspected; everything below it degrades to
/// [`DiskUsage::incomplete`].
pub fn walk(root: &Path, cancel: &AtomicBool, progress: &Progress) -> io::Result<WalkEnd> {
    let result = walk_tree(root, cancel, progress);
    progress.finished.store(true, Ordering::Release);
    result
}

fn walk_tree(root: &Path, cancel: &AtomicBool, progress: &Progress) -> io::Result<WalkEnd> {
    let root_meta = std::fs::symlink_metadata(root)?;
    let device = root_meta.dev();
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    add_blocks(&root_meta, progress, &mut seen);
    if !root_meta.is_dir() {
        return Ok(WalkEnd::Done(progress.snapshot()));
    }
    let mut pending: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                log::debug!("size of {}: skipped, {error}", dir.display());
                progress.incomplete.store(true, Ordering::Relaxed);
                continue;
            }
        };
        for entry in entries {
            if cancel.load(Ordering::Relaxed) {
                return Ok(WalkEnd::Cancelled);
            }
            // `DirEntry::metadata` is an lstat: a link is never followed.
            let Some(meta) = entry
                .and_then(|entry| {
                    let meta = entry.metadata()?;
                    Ok((entry.path(), meta))
                })
                .map_err(|error| {
                    log::debug!("size of {}: entry skipped, {error}", dir.display());
                    progress.incomplete.store(true, Ordering::Relaxed);
                })
                .ok()
            else {
                continue;
            };
            let (path, meta) = meta;
            if meta.dev() != device {
                continue; // a mount below the folder belongs to another volume
            }
            add_blocks(&meta, progress, &mut seen);
            if meta.is_dir() {
                progress.folders.fetch_add(1, Ordering::Relaxed);
                pending.push(path);
            } else {
                progress.files.fetch_add(1, Ordering::Relaxed);
            }
        }
        if cancel.load(Ordering::Relaxed) {
            return Ok(WalkEnd::Cancelled);
        }
    }
    Ok(WalkEnd::Done(progress.snapshot()))
}

/// Adds the allocated bytes of one inode, once per `(device, inode)`.
fn add_blocks(meta: &std::fs::Metadata, progress: &Progress, seen: &mut HashSet<(u64, u64)>) {
    // Only multiply-linked files can repeat; the set stays small.
    if meta.nlink() > 1 && !meta.is_dir() && !seen.insert((meta.dev(), meta.ino())) {
        return;
    }
    progress
        .bytes
        .fetch_add(meta.blocks().saturating_mul(512), Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::sync::atomic::AtomicU64;

    use super::*;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> io::Result<Self> {
            let root = std::env::temp_dir().join(format!(
                "fileflux-du-test-{tag}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root)?;
            Ok(Self(root))
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if let Ok(entries) = fs::read_dir(&self.0) {
                // Let the unreadable-folder test's directory be removed.
                for entry in entries.flatten() {
                    fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o755)).ok();
                }
            }
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn run(root: &Path) -> io::Result<DiskUsage> {
        match walk(root, &AtomicBool::new(false), &Progress::default())? {
            WalkEnd::Done(usage) => Ok(usage),
            WalkEnd::Cancelled => Err(io::Error::other("cancelled")),
        }
    }

    fn blocks(path: &Path) -> io::Result<u64> {
        Ok(fs::symlink_metadata(path)?.blocks() * 512)
    }

    #[test]
    fn nested_files_sum_their_allocated_blocks() -> io::Result<()> {
        let scratch = Scratch::new("nested")?;
        let root = scratch.path();
        fs::create_dir_all(root.join("a/b"))?;
        fs::write(root.join("one.bin"), vec![1u8; 10_000])?;
        fs::write(root.join("a/two.bin"), vec![2u8; 70_000])?;
        fs::write(root.join("a/b/three.bin"), b"x")?;
        let expected = blocks(root)?
            + blocks(&root.join("a"))?
            + blocks(&root.join("a/b"))?
            + blocks(&root.join("one.bin"))?
            + blocks(&root.join("a/two.bin"))?
            + blocks(&root.join("a/b/three.bin"))?;
        let usage = run(root)?;
        assert_eq!(usage.bytes, expected);
        assert_eq!((usage.files, usage.folders), (3, 2));
        assert!(!usage.incomplete);
        Ok(())
    }

    #[test]
    fn a_sparse_file_counts_blocks_not_its_apparent_size() -> io::Result<()> {
        let scratch = Scratch::new("sparse")?;
        let file = fs::File::create(scratch.path().join("sparse"))?;
        file.set_len(100_000_000)?;
        let usage = run(scratch.path())?;
        assert_eq!(
            usage.bytes,
            blocks(scratch.path())? + blocks(&scratch.path().join("sparse"))?
        );
        Ok(())
    }

    #[test]
    fn a_hard_link_counts_once() -> io::Result<()> {
        let scratch = Scratch::new("hard")?;
        let root = scratch.path();
        fs::write(root.join("orig"), vec![7u8; 50_000])?;
        fs::hard_link(root.join("orig"), root.join("twin"))?;
        fs::create_dir(root.join("sub"))?;
        fs::hard_link(root.join("orig"), root.join("sub/third"))?;
        let usage = run(root)?;
        assert_eq!(
            usage.bytes,
            blocks(root)? + blocks(&root.join("sub"))? + blocks(&root.join("orig"))?
        );
        assert_eq!(usage.files, 3, "every name is still a file entry");
        Ok(())
    }

    #[test]
    fn symlinks_are_not_followed() -> io::Result<()> {
        let scratch = Scratch::new("links")?;
        let outside = Scratch::new("links-outside")?;
        fs::write(outside.path().join("big"), vec![3u8; 500_000])?;
        fs::create_dir(outside.path().join("dir"))?;
        fs::write(outside.path().join("dir/inner"), vec![3u8; 500_000])?;
        let root = scratch.path();
        symlink(outside.path().join("big"), root.join("to-file"))?;
        symlink(outside.path().join("dir"), root.join("to-dir"))?;
        symlink(root, root.join("loop"))?;
        let usage = run(root)?;
        assert_eq!(
            usage.bytes,
            blocks(root)?
                + blocks(&root.join("to-file"))?
                + blocks(&root.join("to-dir"))?
                + blocks(&root.join("loop"))?
        );
        assert_eq!((usage.files, usage.folders), (3, 0));
        Ok(())
    }

    #[test]
    fn a_symlink_as_root_counts_only_itself() -> io::Result<()> {
        let scratch = Scratch::new("rootlink")?;
        fs::create_dir(scratch.path().join("real"))?;
        fs::write(scratch.path().join("real/f"), vec![1u8; 100_000])?;
        symlink(scratch.path().join("real"), scratch.path().join("link"))?;
        let usage = run(&scratch.path().join("link"))?;
        assert_eq!(usage.bytes, blocks(&scratch.path().join("link"))?);
        Ok(())
    }

    #[test]
    fn an_unreadable_subfolder_is_skipped_and_flagged() -> io::Result<()> {
        let scratch = Scratch::new("denied")?;
        let root = scratch.path();
        fs::write(root.join("ok"), vec![1u8; 20_000])?;
        fs::create_dir(root.join("locked"))?;
        fs::write(root.join("locked/hidden"), vec![1u8; 20_000])?;
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000))?;
        if fs::read_dir(root.join("locked")).is_ok() {
            return Ok(()); // running as root: nothing is unreadable
        }
        let usage = run(root)?;
        assert!(usage.incomplete);
        assert_eq!(
            usage.bytes,
            blocks(root)? + blocks(&root.join("ok"))? + blocks(&root.join("locked"))?
        );
        Ok(())
    }

    #[test]
    fn a_complete_walk_is_not_flagged_incomplete() -> io::Result<()> {
        let scratch = Scratch::new("complete")?;
        fs::create_dir(scratch.path().join("d"))?;
        assert!(!run(scratch.path())?.incomplete);
        Ok(())
    }

    #[test]
    fn a_raised_cancel_flag_stops_the_walk_at_the_next_entry() -> io::Result<()> {
        let scratch = Scratch::new("cancel")?;
        for index in 0..50 {
            fs::write(scratch.path().join(format!("f{index}")), vec![1u8; 5_000])?;
        }
        let cancel = AtomicBool::new(true);
        let progress = Progress::default();
        let end = walk(scratch.path(), &cancel, &progress)?;
        assert_eq!(end, WalkEnd::Cancelled);
        assert_eq!(progress.snapshot().files, 0);
        assert!(progress.is_finished());
        Ok(())
    }

    #[test]
    fn a_missing_root_is_an_error() -> io::Result<()> {
        let scratch = Scratch::new("missing")?;
        let progress = Progress::default();
        let error = walk(
            &scratch.path().join("nope"),
            &AtomicBool::new(false),
            &progress,
        )
        .err()
        .map(|error| error.kind());
        assert_eq!(error, Some(io::ErrorKind::NotFound));
        assert!(progress.is_finished());
        Ok(())
    }
}
