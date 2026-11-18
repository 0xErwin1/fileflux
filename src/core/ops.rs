//! Filesystem mutations: create, rename, copy and move. Nothing here ever
//! overwrites silently, follows symlinks for copy/move, or deletes a source
//! before its replacement is verified in place.

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use rustix::fs::{CWD, RenameFlags, renameat_with};
use rustix::io::Errno;

/// Longest file name the filesystems we target accept, in bytes.
const MAX_NAME_BYTES: usize = 255;
/// Bytes copied between cancel checks and progress reports.
const CHUNK: usize = 1 << 20;

/// Why a file name is not acceptable for create or rename.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameError {
    Empty,
    CurrentDir,
    ParentDir,
    Separator,
    Nul,
    TooLong,
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            NameError::Empty => "the name is empty",
            NameError::CurrentDir => "\".\" is not a valid name",
            NameError::ParentDir => "\"..\" is not a valid name",
            NameError::Separator => "a name cannot contain \"/\"",
            NameError::Nul => "a name cannot contain a NUL byte",
            NameError::TooLong => "the name is longer than 255 bytes",
        })
    }
}

impl Error for NameError {}

/// Failure of a single create or rename.
#[derive(Debug)]
pub enum OpError {
    Name(NameError),
    Io(io::Error),
}

impl OpError {
    /// True when the target already existed and nothing was changed.
    pub fn is_already_exists(&self) -> bool {
        matches!(self, OpError::Io(error) if error.kind() == io::ErrorKind::AlreadyExists)
    }
}

impl fmt::Display for OpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpError::Name(error) => error.fmt(f),
            OpError::Io(error) => error.fmt(f),
        }
    }
}

impl Error for OpError {}

impl From<NameError> for OpError {
    fn from(error: NameError) -> Self {
        OpError::Name(error)
    }
}

impl From<io::Error> for OpError {
    fn from(error: io::Error) -> Self {
        OpError::Io(error)
    }
}

/// Copy keeps the source; Move removes it only after the target is in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Copy,
    Move,
}

/// The answer to "the target already exists".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conflict {
    Skip,
    Replace,
    KeepBoth,
    Cancel,
}

/// Bytes done / total across the whole transfer plus the item in flight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub current: PathBuf,
}

/// Why one item of a transfer failed.
#[derive(Debug)]
pub enum TransferError {
    Io(io::Error),
    /// A device, socket or fifo; never copied, never silently skipped.
    Unsupported(PathBuf),
    /// The destination is the source itself, inside it, or replaces an
    /// ancestor of it.
    IntoItself,
    /// A copied file did not match its source size.
    VerifyFailed(PathBuf),
    /// Cancelled before or during this item; the source is untouched.
    Cancelled,
    /// The move copied and verified the data at the destination, but the
    /// source could not be removed afterwards: both copies exist.
    SourceNotRemoved(io::Error),
}

impl fmt::Display for TransferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransferError::Io(error) => error.fmt(f),
            TransferError::Unsupported(path) => {
                write!(f, "unsupported file type: {}", path.display())
            }
            TransferError::IntoItself => f.write_str("cannot place a folder inside itself"),
            TransferError::VerifyFailed(path) => {
                write!(f, "copy verification failed for {}", path.display())
            }
            TransferError::Cancelled => f.write_str("cancelled"),
            TransferError::SourceNotRemoved(error) => {
                write!(f, "copied, but the source was not removed: {error}")
            }
        }
    }
}

impl Error for TransferError {}

impl From<io::Error> for TransferError {
    fn from(error: io::Error) -> Self {
        TransferError::Io(error)
    }
}

/// What happened to one source.
#[derive(Debug)]
pub enum ItemOutcome {
    Ok { dest: PathBuf, bytes: u64 },
    Skipped,
    Failed(TransferError),
}

/// One source and its outcome.
#[derive(Debug)]
pub struct ItemReport {
    pub source: PathBuf,
    pub outcome: ItemOutcome,
}

/// Per-item results; every source appears exactly once, in order.
#[derive(Debug, Default)]
pub struct TransferReport {
    pub items: Vec<ItemReport>,
    /// Bytes of the items that completed.
    pub bytes: u64,
    pub cancelled: bool,
}

/// Rejects names that are empty, `.`/`..`, contain `/` or NUL, or exceed
/// 255 bytes.
pub fn validate_name(name: &OsStr) -> Result<(), NameError> {
    let bytes = name.as_bytes();
    if bytes.is_empty() {
        Err(NameError::Empty)
    } else if bytes == b"." {
        Err(NameError::CurrentDir)
    } else if bytes == b".." {
        Err(NameError::ParentDir)
    } else if bytes.contains(&b'/') {
        Err(NameError::Separator)
    } else if bytes.contains(&0) {
        Err(NameError::Nul)
    } else if bytes.len() > MAX_NAME_BYTES {
        Err(NameError::TooLong)
    } else {
        Ok(())
    }
}

/// Creates a new directory `parent/name`; an existing entry of any kind is
/// an `AlreadyExists` error and is left untouched.
pub fn create_dir(parent: &Path, name: &OsStr) -> Result<PathBuf, OpError> {
    validate_name(name)?;
    let path = parent.join(name);
    fs::create_dir(&path)?;
    Ok(path)
}

/// Creates a new empty file `parent/name` (`O_EXCL`); an existing entry of
/// any kind is an `AlreadyExists` error and is left untouched.
pub fn create_file(parent: &Path, name: &OsStr) -> Result<PathBuf, OpError> {
    validate_name(name)?;
    let path = parent.join(name);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    Ok(path)
}

/// Renames `path` inside its own directory and returns the new path. Never
/// replaces an existing entry (`AlreadyExists`); renaming to the current
/// name is a successful no-op.
pub fn rename(path: &Path, new_name: &OsStr) -> Result<PathBuf, OpError> {
    validate_name(new_name)?;
    if path.file_name() == Some(new_name) {
        return Ok(path.to_path_buf());
    }
    let target = path.with_file_name(new_name);
    rename_noreplace(path, &target)?;
    Ok(target)
}

/// Renames `from` to `to` without ever replacing an existing target, using
/// `renameat2(RENAME_NOREPLACE)`. On filesystems or kernels without it the
/// fallback is a pre-check followed by a plain rename, which has a window
/// in which a concurrent writer can create `to` and be replaced.
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    match renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        Err(Errno::NOSYS | Errno::INVAL | Errno::NOTSUP) => {
            if exists(to) {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            fs::rename(from, to)
        }
        Err(error) => Err(error.into()),
    }
}

/// True when anything, including a dangling symlink, occupies `path`.
/// Unreadable counts as present: never assume free.
fn exists(path: &Path) -> bool {
    !matches!(fs::symlink_metadata(path), Err(error) if error.kind() == io::ErrorKind::NotFound)
}

/// Byte offset where the extension starts (`.tar.*` counts as one), or
/// `None` for names without one. A leading dot is not an extension.
fn extension_start(name: &[u8]) -> Option<usize> {
    let last = name.iter().rposition(|&byte| byte == b'.')?;
    if last == 0 || last + 1 == name.len() {
        return None;
    }
    let head = name.get(..last)?;
    if let Some(previous) = head.iter().rposition(|&byte| byte == b'.')
        && previous > 0
        && head.get(previous + 1..) == Some(b"tar".as_slice())
    {
        return Some(previous);
    }
    Some(last)
}

/// `name` with ` (n)` inserted before the extension (folders have none).
pub(crate) fn numbered_name(name: &OsStr, n: u32, is_dir: bool) -> OsString {
    let bytes = name.as_bytes();
    let split = if is_dir {
        bytes.len()
    } else {
        extension_start(bytes).unwrap_or(bytes.len())
    };
    let mut out = bytes.get(..split).unwrap_or(bytes).to_vec();
    out.extend_from_slice(format!(" ({n})").as_bytes());
    out.extend_from_slice(bytes.get(split..).unwrap_or_default());
    OsString::from_vec(out)
}

/// A name for `dir` that does not exist yet: `name` itself when free, else
/// "report.pdf" → "report (2).pdf", "archive.tar.gz" → "archive (2).tar.gz",
/// "Photos" → "Photos (2)", counting up past existing entries.
pub fn unique_name(dir: &Path, name: &OsStr) -> OsString {
    let first = dir.join(name);
    if !exists(&first) {
        return name.to_owned();
    }
    let is_dir = fs::symlink_metadata(&first).is_ok_and(|meta| meta.is_dir());
    let mut n = 2u32;
    loop {
        let candidate = numbered_name(name, n, is_dir);
        if !exists(&dir.join(&candidate)) || n == u32::MAX {
            return candidate;
        }
        n += 1;
    }
}

/// Copies or moves `sources` into `dest_dir`.
///
/// `conflict` is asked (with the existing target) whenever a target is
/// already there; nothing is replaced unless it answers `Replace`. Copies
/// are built under a temporary sibling name, verified and renamed into
/// place, so a cancelled or failed copy never leaves a half file under the
/// real name. A move renames when possible and, across devices (`EXDEV`),
/// copies, verifies, renames into place and only then removes the source.
/// Symlinks are recreated, never followed. `cancel` is polled between items
/// and files and every 1 MiB chunk.
pub fn transfer(
    sources: &[PathBuf],
    dest_dir: &Path,
    mode: Mode,
    conflict: impl FnMut(&Path) -> Conflict,
    progress: impl FnMut(Progress),
    cancel: &AtomicBool,
) -> TransferReport {
    transfer_with(
        sources,
        dest_dir,
        mode,
        conflict,
        progress,
        cancel,
        &rename_noreplace,
    )
}

/// `transfer` with the move's first `rename` attempt injectable, so the
/// cross-device fallback is testable without two filesystems.
fn transfer_with(
    sources: &[PathBuf],
    dest_dir: &Path,
    mode: Mode,
    mut conflict: impl FnMut(&Path) -> Conflict,
    mut progress: impl FnMut(Progress),
    cancel: &AtomicBool,
    rename: &dyn Fn(&Path, &Path) -> io::Result<()>,
) -> TransferReport {
    let sizes: Vec<u64> = sources.iter().map(|source| tree_size(source)).collect();
    let mut ctx = Ctx {
        rename,
        progress: &mut progress,
        cancel,
        total: sizes.iter().sum(),
        done: 0,
        current: PathBuf::new(),
        seq: 0,
        deferred: Vec::new(),
    };
    let mut report = TransferReport::default();
    for (source, size) in sources.iter().zip(sizes) {
        let outcome = if report.cancelled || cancel.load(Ordering::Relaxed) {
            ItemOutcome::Failed(TransferError::Cancelled)
        } else {
            let before = ctx.done;
            let outcome = ctx.item(source, dest_dir, mode, &mut conflict, size);
            if !matches!(outcome, ItemOutcome::Failed(TransferError::Cancelled)) {
                ctx.done = before + size;
                ctx.emit();
            }
            outcome
        };
        match &outcome {
            ItemOutcome::Ok { bytes, .. } => report.bytes += bytes,
            ItemOutcome::Failed(TransferError::Cancelled) => report.cancelled = true,
            _ => {}
        }
        report.items.push(ItemReport {
            source: source.clone(),
            outcome,
        });
    }
    report
}

/// Sum of regular file sizes under `path`, without following symlinks.
/// Unreadable parts count as zero; this only feeds progress.
fn tree_size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_file() {
        meta.len()
    } else if meta.is_dir() {
        fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| tree_size(&entry.path()))
            .sum()
    } else {
        0
    }
}

/// Deletes `path` for good: a file or link is unlinked (a link is never
/// followed), a directory is removed recursively with every link inside it
/// removed as a link. There is no undo; the UI only calls this behind a
/// confirmation (Shift+Del, the Trash view, or a missing Trash).
pub fn remove_permanently(path: &Path) -> io::Result<()> {
    remove_all(path)
}

/// Removes a file, link or directory tree without following symlinks.
fn remove_all(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// A directory whose mode and mtime are applied after its contents are in,
/// so a read-only source directory neither blocks its own copy nor blocks
/// cleanup of a failed one.
struct DeferredDir {
    path: PathBuf,
    permissions: fs::Permissions,
    modified: Option<SystemTime>,
}

/// What a successful placement achieved.
struct Placed {
    /// Set when a move's source could not be removed after the copy.
    leftover: Option<io::Error>,
}

struct Ctx<'a> {
    rename: &'a dyn Fn(&Path, &Path) -> io::Result<()>,
    progress: &'a mut dyn FnMut(Progress),
    cancel: &'a AtomicBool,
    total: u64,
    done: u64,
    current: PathBuf,
    seq: u32,
    deferred: Vec<DeferredDir>,
}

impl Ctx<'_> {
    fn emit(&mut self) {
        (self.progress)(Progress {
            bytes_done: self.done,
            bytes_total: self.total,
            current: self.current.clone(),
        });
    }

    fn check_cancel(&self) -> Result<(), TransferError> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(TransferError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// A free hidden sibling name in `dir` for partial copies and the
    /// moved-aside original of a replace.
    fn temp_name(&mut self, dir: &Path, tag: &str) -> PathBuf {
        loop {
            self.seq += 1;
            let candidate = dir.join(format!(
                ".fileflux-{tag}-{}-{}",
                std::process::id(),
                self.seq
            ));
            if !exists(&candidate) {
                return candidate;
            }
        }
    }

    fn item(
        &mut self,
        src: &Path,
        dest_dir: &Path,
        mode: Mode,
        conflict: &mut impl FnMut(&Path) -> Conflict,
        size: u64,
    ) -> ItemOutcome {
        match self.plan_and_place(src, dest_dir, mode, conflict, size) {
            Ok(Some((dest, placed))) => match placed.leftover {
                None => ItemOutcome::Ok { dest, bytes: size },
                Some(error) => ItemOutcome::Failed(TransferError::SourceNotRemoved(error)),
            },
            Ok(None) => ItemOutcome::Skipped,
            Err(error) => ItemOutcome::Failed(error),
        }
    }

    /// `Ok(None)` means skipped.
    fn plan_and_place(
        &mut self,
        src: &Path,
        dest_dir: &Path,
        mode: Mode,
        conflict: &mut impl FnMut(&Path) -> Conflict,
        size: u64,
    ) -> Result<Option<(PathBuf, Placed)>, TransferError> {
        self.check_cancel()?;
        let name = src.file_name().ok_or_else(|| {
            TransferError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source has no file name",
            ))
        })?;
        let meta = fs::symlink_metadata(src)?;
        self.current = src.to_path_buf();
        self.emit();

        // Compare real locations, but never resolve the source itself: a
        // symlink source is the link, not what it points to.
        let dest_canon = fs::canonicalize(dest_dir)?;
        let parent = match src.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        let parent_canon = fs::canonicalize(parent)?;
        let src_canon = parent_canon.join(name);
        if meta.is_dir() && dest_canon.starts_with(&src_canon) {
            return Err(TransferError::IntoItself);
        }

        let mut target = dest_dir.join(name);
        let mut replace = false;
        if dest_canon == parent_canon {
            match mode {
                Mode::Move => return Ok(None),
                Mode::Copy => target = dest_dir.join(unique_name(dest_dir, name)),
            }
        } else if exists(&target) {
            match conflict(&target) {
                Conflict::Skip => return Ok(None),
                Conflict::Cancel => return Err(TransferError::Cancelled),
                Conflict::KeepBoth => target = dest_dir.join(unique_name(dest_dir, name)),
                Conflict::Replace => {
                    if src_canon.starts_with(dest_canon.join(name)) {
                        return Err(TransferError::IntoItself);
                    }
                    replace = true;
                }
            }
        }

        let placed = self.place(src, &target, replace, mode, size)?;
        Ok(Some((target, placed)))
    }

    /// Puts `src` at `target`. With `replace`, the existing target is moved
    /// aside first, deleted only once the new one is in place, and restored
    /// when placement fails.
    fn place(
        &mut self,
        src: &Path,
        target: &Path,
        replace: bool,
        mode: Mode,
        size: u64,
    ) -> Result<Placed, TransferError> {
        let dir = target.parent().unwrap_or(Path::new("."));
        let aside = if replace {
            let aside = self.temp_name(dir, "aside");
            fs::rename(target, &aside)?;
            Some(aside)
        } else {
            None
        };
        match self.materialize(src, target, mode, size) {
            Ok(placed) => {
                if let Some(aside) = aside {
                    // The new copy is in place; a stuck aside copy is only
                    // leftover hidden data, never a failure of the transfer.
                    remove_all(&aside).ok();
                }
                Ok(placed)
            }
            Err(error) => {
                if let Some(aside) = aside {
                    rename_noreplace(&aside, target).ok();
                }
                Err(error)
            }
        }
    }

    /// Creates `target` atomically from `src` (target must not exist).
    fn materialize(
        &mut self,
        src: &Path,
        target: &Path,
        mode: Mode,
        size: u64,
    ) -> Result<Placed, TransferError> {
        if mode == Mode::Move {
            match (self.rename)(src, target) {
                Ok(()) => {
                    self.done += size;
                    self.emit();
                    return Ok(Placed { leftover: None });
                }
                Err(error) if error.raw_os_error() == Some(Errno::XDEV.raw_os_error()) => {}
                Err(error) => return Err(error.into()),
            }
        }

        let dir = target.parent().unwrap_or(Path::new("."));
        let partial = self.temp_name(dir, "partial");
        self.deferred.clear();
        let built = self
            .copy_node(src, &partial)
            .and_then(|()| self.finalize_dirs().map_err(TransferError::from))
            .and_then(|()| rename_noreplace(&partial, target).map_err(TransferError::from));
        if let Err(error) = built {
            self.deferred.clear();
            remove_all(&partial).ok();
            return Err(error);
        }
        let leftover = if mode == Mode::Move {
            remove_all(src).err()
        } else {
            None
        };
        Ok(Placed { leftover })
    }

    fn finalize_dirs(&mut self) -> io::Result<()> {
        while let Some(dir) = self.deferred.pop() {
            if let Some(modified) = dir.modified {
                fs::File::open(&dir.path)?.set_modified(modified)?;
            }
            fs::set_permissions(&dir.path, dir.permissions)?;
        }
        Ok(())
    }

    fn copy_node(&mut self, src: &Path, dst: &Path) -> Result<(), TransferError> {
        self.check_cancel()?;
        let meta = fs::symlink_metadata(src)?;
        let kind = meta.file_type();
        if kind.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
        } else if kind.is_dir() {
            fs::create_dir(dst)?;
            self.deferred.push(DeferredDir {
                path: dst.to_path_buf(),
                permissions: meta.permissions(),
                modified: meta.modified().ok(),
            });
            for entry in fs::read_dir(src)? {
                let entry = entry?;
                self.copy_node(&entry.path(), &dst.join(entry.file_name()))?;
            }
        } else if kind.is_file() {
            self.copy_file(src, dst, &meta)?;
        } else {
            return Err(TransferError::Unsupported(src.to_path_buf()));
        }
        Ok(())
    }

    fn copy_file(
        &mut self,
        src: &Path,
        dst: &Path,
        meta: &fs::Metadata,
    ) -> Result<(), TransferError> {
        self.current = src.to_path_buf();
        let mut input = OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(src)?;
        let expected = input.metadata()?.len();
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(meta.mode() & 0o7777)
            .open(dst)?;
        let mut buffer = vec![0u8; CHUNK];
        let mut copied = 0u64;
        loop {
            self.check_cancel()?;
            let read = input.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            output.write_all(buffer.get(..read).unwrap_or_default())?;
            copied += read as u64;
            self.done += read as u64;
            self.emit();
        }
        output.sync_all()?;
        if copied != expected || output.metadata()?.len() != copied {
            return Err(TransferError::VerifyFailed(src.to_path_buf()));
        }
        if let Ok(modified) = meta.modified() {
            output.set_modified(modified)?;
        }
        output.set_permissions(meta.permissions())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::atomic::AtomicU64;
    use std::time::{Duration, UNIX_EPOCH};

    type TestResult = Result<(), Box<dyn Error>>;

    static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

    /// Unique scratch directory under the system temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(prefix: &str) -> io::Result<Scratch> {
            let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
            let tick = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            let dir = std::env::temp_dir().join(format!(
                "fileflux-ops-{prefix}-{}-{tick}-{seq}",
                std::process::id()
            ));
            fs::create_dir_all(&dir)?;
            Ok(Scratch(dir))
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        fn dir(&self, name: &str) -> io::Result<PathBuf> {
            let path = self.0.join(name);
            fs::create_dir_all(&path)?;
            Ok(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            // Cancel/readonly fixtures may have restrictive modes.
            fix_modes(&self.0);
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn fix_modes(path: &Path) {
        if let Ok(meta) = fs::symlink_metadata(path)
            && meta.is_dir()
        {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).ok();
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    fix_modes(&entry.path());
                }
            }
        }
    }

    fn names(dir: &Path) -> Result<Vec<String>, Box<dyn Error>> {
        let mut found = Vec::new();
        for entry in fs::read_dir(dir)? {
            found.push(entry?.file_name().to_string_lossy().into_owned());
        }
        found.sort();
        Ok(found)
    }

    fn no_conflict(_: &Path) -> Conflict {
        Conflict::Cancel
    }

    fn run(
        sources: &[PathBuf],
        dest: &Path,
        mode: Mode,
        conflict: impl FnMut(&Path) -> Conflict,
    ) -> TransferReport {
        transfer(
            sources,
            dest,
            mode,
            conflict,
            |_| {},
            &AtomicBool::new(false),
        )
    }

    fn xdev(_: &Path, _: &Path) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(Errno::XDEV.raw_os_error()))
    }

    fn run_xdev(
        sources: &[PathBuf],
        dest: &Path,
        conflict: impl FnMut(&Path) -> Conflict,
    ) -> TransferReport {
        transfer_with(
            sources,
            dest,
            Mode::Move,
            conflict,
            |_| {},
            &AtomicBool::new(false),
            &xdev,
        )
    }

    fn only_item(report: &TransferReport) -> Result<&ItemOutcome, Box<dyn Error>> {
        Ok(&report.items.first().ok_or("no item reported")?.outcome)
    }

    fn make_fifo(path: &Path) -> Result<(), Box<dyn Error>> {
        rustix::fs::mknodat(
            CWD,
            path,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )?;
        Ok(())
    }

    // ---- names ----

    #[test]
    fn validate_name_rejects_bad_names() {
        assert_eq!(validate_name(OsStr::new("")), Err(NameError::Empty));
        assert_eq!(validate_name(OsStr::new(".")), Err(NameError::CurrentDir));
        assert_eq!(validate_name(OsStr::new("..")), Err(NameError::ParentDir));
        assert_eq!(validate_name(OsStr::new("a/b")), Err(NameError::Separator));
        assert_eq!(
            validate_name(OsStr::from_bytes(b"a\0b")),
            Err(NameError::Nul)
        );
        let long = "x".repeat(256);
        assert_eq!(validate_name(OsStr::new(&long)), Err(NameError::TooLong));
    }

    #[test]
    fn validate_name_accepts_good_names() {
        let longest = "x".repeat(255);
        for name in [
            "a",
            "...",
            ".hidden",
            "with space",
            "ñandú",
            longest.as_str(),
        ] {
            assert_eq!(validate_name(OsStr::new(name)), Ok(()), "{name}");
        }
        assert_eq!(validate_name(OsStr::from_bytes(b"caf\xe9")), Ok(()));
    }

    // ---- create ----

    #[test]
    fn create_dir_and_file_make_new_entries() -> TestResult {
        let scratch = Scratch::new("create")?;
        let dir = create_dir(&scratch.0, OsStr::new("folder"))?;
        let file = create_file(&scratch.0, OsStr::new("note.txt"))?;
        assert!(dir.is_dir());
        assert!(file.is_file());
        assert_eq!(fs::metadata(&file)?.len(), 0);
        Ok(())
    }

    #[test]
    fn create_never_overwrites() -> TestResult {
        let scratch = Scratch::new("create-exists")?;
        let file = scratch.path("note.txt");
        fs::write(&file, b"precious")?;
        fs::create_dir(scratch.path("folder"))?;
        fs::write(scratch.path("folder/inner"), b"inner")?;

        let file_err = create_file(&scratch.0, OsStr::new("note.txt"))
            .err()
            .ok_or("create_file overwrote")?;
        let dir_err = create_dir(&scratch.0, OsStr::new("folder"))
            .err()
            .ok_or("create_dir succeeded")?;
        let dir_over_file = create_dir(&scratch.0, OsStr::new("note.txt"))
            .err()
            .ok_or("create_dir over a file succeeded")?;

        assert!(file_err.is_already_exists());
        assert!(dir_err.is_already_exists());
        assert!(dir_over_file.is_already_exists());
        assert_eq!(fs::read(&file)?, b"precious");
        assert_eq!(fs::read(scratch.path("folder/inner"))?, b"inner");
        Ok(())
    }

    #[test]
    fn create_rejects_invalid_names_without_touching_disk() -> TestResult {
        let scratch = Scratch::new("create-invalid")?;
        let err = create_file(&scratch.0, OsStr::new("a/b"))
            .err()
            .ok_or("accepted")?;
        assert!(matches!(err, OpError::Name(NameError::Separator)));
        let err = create_dir(&scratch.0, OsStr::new(".."))
            .err()
            .ok_or("accepted")?;
        assert!(matches!(err, OpError::Name(NameError::ParentDir)));
        assert!(names(&scratch.0)?.is_empty());
        Ok(())
    }

    // ---- rename ----

    #[test]
    fn rename_moves_within_the_directory() -> TestResult {
        let scratch = Scratch::new("rename")?;
        fs::write(scratch.path("a.txt"), b"data")?;
        let target = rename(&scratch.path("a.txt"), OsStr::new("b.txt"))?;
        assert_eq!(target, scratch.path("b.txt"));
        assert_eq!(names(&scratch.0)?, ["b.txt"]);
        assert_eq!(fs::read(&target)?, b"data");
        Ok(())
    }

    #[test]
    fn rename_never_replaces_an_existing_target() -> TestResult {
        let scratch = Scratch::new("rename-exists")?;
        fs::write(scratch.path("a.txt"), b"first")?;
        fs::write(scratch.path("b.txt"), b"second")?;
        fs::create_dir(scratch.path("dir"))?;

        let err = rename(&scratch.path("a.txt"), OsStr::new("b.txt"))
            .err()
            .ok_or("replaced a file")?;
        assert!(err.is_already_exists());
        let err = rename(&scratch.path("a.txt"), OsStr::new("dir"))
            .err()
            .ok_or("replaced a dir")?;
        assert!(err.is_already_exists());
        assert_eq!(fs::read(scratch.path("a.txt"))?, b"first");
        assert_eq!(fs::read(scratch.path("b.txt"))?, b"second");
        Ok(())
    }

    #[test]
    fn rename_to_same_name_is_a_noop() -> TestResult {
        let scratch = Scratch::new("rename-same")?;
        fs::write(scratch.path("a.txt"), b"data")?;
        let target = rename(&scratch.path("a.txt"), OsStr::new("a.txt"))?;
        assert_eq!(target, scratch.path("a.txt"));
        assert_eq!(fs::read(&target)?, b"data");
        assert_eq!(names(&scratch.0)?, ["a.txt"]);
        Ok(())
    }

    #[test]
    fn rename_rejects_invalid_names_and_keeps_the_source() -> TestResult {
        let scratch = Scratch::new("rename-invalid")?;
        fs::write(scratch.path("a.txt"), b"data")?;
        for bad in ["", ".", "..", "x/y"] {
            let err = rename(&scratch.path("a.txt"), OsStr::new(bad))
                .err()
                .ok_or("accepted")?;
            assert!(matches!(err, OpError::Name(_)), "{bad:?}");
        }
        assert_eq!(names(&scratch.0)?, ["a.txt"]);
        Ok(())
    }

    // ---- unique_name ----

    #[test]
    fn unique_name_numbers_files_folders_and_tar_archives() -> TestResult {
        let scratch = Scratch::new("unique")?;
        let dir = &scratch.0;
        fs::write(dir.join("report.pdf"), b"")?;
        fs::write(dir.join("archive.tar.gz"), b"")?;
        fs::write(dir.join("noext"), b"")?;
        fs::write(dir.join(".bashrc"), b"")?;
        fs::create_dir(dir.join("Photos"))?;
        fs::create_dir(dir.join("v1.2"))?;

        assert_eq!(unique_name(dir, OsStr::new("report.pdf")), "report (2).pdf");
        assert_eq!(
            unique_name(dir, OsStr::new("archive.tar.gz")),
            "archive (2).tar.gz"
        );
        assert_eq!(unique_name(dir, OsStr::new("noext")), "noext (2)");
        assert_eq!(unique_name(dir, OsStr::new(".bashrc")), ".bashrc (2)");
        assert_eq!(unique_name(dir, OsStr::new("Photos")), "Photos (2)");
        assert_eq!(unique_name(dir, OsStr::new("v1.2")), "v1.2 (2)");
        assert_eq!(unique_name(dir, OsStr::new("free.txt")), "free.txt");
        Ok(())
    }

    #[test]
    fn unique_name_increments_past_existing() -> TestResult {
        let scratch = Scratch::new("unique-inc")?;
        let dir = &scratch.0;
        fs::write(dir.join("report.pdf"), b"")?;
        fs::write(dir.join("report (2).pdf"), b"")?;
        // A dangling symlink occupies a name too.
        symlink("nowhere", dir.join("report (3).pdf"))?;
        assert_eq!(unique_name(dir, OsStr::new("report.pdf")), "report (4).pdf");
        Ok(())
    }

    // ---- copy ----

    #[test]
    fn copy_file_preserves_content_mode_and_mtime() -> TestResult {
        let scratch = Scratch::new("copy-file")?;
        let src = scratch.path("src/a.bin");
        let dest = scratch.dir("dest")?;
        fs::create_dir(scratch.path("src"))?;
        fs::write(&src, b"hello world")?;
        let when = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        fs::File::options()
            .write(true)
            .open(&src)?
            .set_modified(when)?;
        fs::set_permissions(&src, fs::Permissions::from_mode(0o751))?;

        let report = run(std::slice::from_ref(&src), &dest, Mode::Copy, no_conflict);

        let ItemOutcome::Ok { dest: out, bytes } = only_item(&report)? else {
            return Err("copy did not succeed".into());
        };
        assert_eq!(out, &dest.join("a.bin"));
        assert_eq!(*bytes, 11);
        assert_eq!(report.bytes, 11);
        assert!(!report.cancelled);
        let meta = fs::metadata(dest.join("a.bin"))?;
        assert_eq!(fs::read(dest.join("a.bin"))?, b"hello world");
        assert_eq!(meta.mode() & 0o7777, 0o751);
        assert_eq!(meta.modified()?, when);
        assert!(src.exists(), "copy keeps the source");
        assert_eq!(names(&dest)?, ["a.bin"], "no partial files remain");
        Ok(())
    }

    #[test]
    fn copy_directory_recursively_with_symlinks_and_modes() -> TestResult {
        let scratch = Scratch::new("copy-dir")?;
        let src = scratch.dir("src/tree")?;
        let dest = scratch.dir("dest")?;
        fs::create_dir(src.join("sub"))?;
        fs::write(src.join("top.txt"), b"top")?;
        fs::write(src.join("sub/deep.txt"), b"deep")?;
        fs::write(scratch.path("outside.txt"), b"outside")?;
        symlink(scratch.path("outside.txt"), src.join("link-file"))?;
        symlink("does-not-exist", src.join("dangling"))?;
        symlink("sub", src.join("link-dir"))?;
        fs::set_permissions(src.join("sub"), fs::Permissions::from_mode(0o500))?;
        let when = UNIX_EPOCH + Duration::from_secs(900_000_000);
        fs::File::open(src.join("sub"))?.set_modified(when)?;

        let report = run(std::slice::from_ref(&src), &dest, Mode::Copy, no_conflict);

        assert!(matches!(only_item(&report)?, ItemOutcome::Ok { .. }));
        let out = dest.join("tree");
        assert_eq!(fs::read(out.join("top.txt"))?, b"top");
        assert_eq!(fs::read(out.join("sub/deep.txt"))?, b"deep");
        assert_eq!(
            fs::read_link(out.join("link-file"))?,
            scratch.path("outside.txt")
        );
        assert_eq!(
            fs::read_link(out.join("dangling"))?,
            Path::new("does-not-exist")
        );
        assert_eq!(fs::read_link(out.join("link-dir"))?, Path::new("sub"));
        assert!(fs::symlink_metadata(out.join("link-dir"))?.is_symlink());
        let sub = fs::metadata(out.join("sub"))?;
        assert_eq!(sub.mode() & 0o7777, 0o500);
        assert_eq!(sub.modified()?, when);
        assert_eq!(report.bytes, 3 + 4);
        assert_eq!(names(&dest)?, ["tree"]);
        Ok(())
    }

    #[test]
    fn copy_a_symlink_copies_the_link_not_the_target() -> TestResult {
        let scratch = Scratch::new("copy-link")?;
        let dest = scratch.dir("dest")?;
        fs::write(scratch.path("target.txt"), b"target")?;
        symlink("target.txt", scratch.path("good"))?;
        symlink("missing", scratch.path("dangling"))?;

        let report = run(
            &[scratch.path("good"), scratch.path("dangling")],
            &dest,
            Mode::Copy,
            no_conflict,
        );

        assert!(
            report
                .items
                .iter()
                .all(|item| matches!(item.outcome, ItemOutcome::Ok { .. }))
        );
        assert_eq!(fs::read_link(dest.join("good"))?, Path::new("target.txt"));
        assert_eq!(fs::read_link(dest.join("dangling"))?, Path::new("missing"));
        Ok(())
    }

    #[test]
    fn special_files_fail_as_unsupported() -> TestResult {
        let scratch = Scratch::new("special")?;
        let dest = scratch.dir("dest")?;
        make_fifo(&scratch.path("pipe"))?;

        let report = run(&[scratch.path("pipe")], &dest, Mode::Copy, no_conflict);

        assert!(matches!(
            only_item(&report)?,
            ItemOutcome::Failed(TransferError::Unsupported(_))
        ));
        assert!(names(&dest)?.is_empty());
        assert!(scratch.path("pipe").exists());
        Ok(())
    }

    #[test]
    fn copy_into_itself_or_a_descendant_is_refused() -> TestResult {
        let scratch = Scratch::new("copy-self")?;
        let src = scratch.dir("tree/sub")?;
        let tree = scratch.path("tree");
        fs::write(tree.join("f.txt"), b"x")?;

        for mode in [Mode::Copy, Mode::Move] {
            for dest in [&tree, &src] {
                let report = run(std::slice::from_ref(&tree), dest, mode, no_conflict);
                assert!(
                    matches!(
                        only_item(&report)?,
                        ItemOutcome::Failed(TransferError::IntoItself)
                    ),
                    "{mode:?} into {}",
                    dest.display()
                );
            }
        }
        assert_eq!(names(&tree)?, ["f.txt", "sub"]);
        assert!(names(&src)?.is_empty());
        Ok(())
    }

    #[test]
    fn copy_to_its_own_directory_keeps_both() -> TestResult {
        let scratch = Scratch::new("copy-same")?;
        fs::write(scratch.path("a.txt"), b"data")?;
        fs::create_dir(scratch.path("Photos"))?;

        let report = run(
            &[scratch.path("a.txt"), scratch.path("Photos")],
            &scratch.0,
            Mode::Copy,
            |_| Conflict::Cancel,
        );

        assert!(
            report
                .items
                .iter()
                .all(|item| matches!(item.outcome, ItemOutcome::Ok { .. }))
        );
        assert_eq!(fs::read(scratch.path("a (2).txt"))?, b"data");
        assert!(scratch.path("Photos (2)").is_dir());
        assert_eq!(fs::read(scratch.path("a.txt"))?, b"data");
        Ok(())
    }

    #[test]
    fn move_to_its_own_directory_is_a_noop() -> TestResult {
        let scratch = Scratch::new("move-same")?;
        fs::write(scratch.path("a.txt"), b"data")?;
        let report = run(
            &[scratch.path("a.txt")],
            &scratch.0,
            Mode::Move,
            no_conflict,
        );
        assert!(matches!(only_item(&report)?, ItemOutcome::Skipped));
        assert_eq!(names(&scratch.0)?, ["a.txt"]);
        Ok(())
    }

    // ---- move ----

    #[test]
    fn move_on_one_device_renames() -> TestResult {
        let scratch = Scratch::new("move")?;
        let dest = scratch.dir("dest")?;
        fs::create_dir(scratch.path("tree"))?;
        fs::write(scratch.path("tree/f.txt"), b"x")?;
        symlink("f.txt", scratch.path("tree/link"))?;

        let report = run(&[scratch.path("tree")], &dest, Mode::Move, no_conflict);

        assert!(matches!(only_item(&report)?, ItemOutcome::Ok { .. }));
        assert!(!scratch.path("tree").exists());
        assert_eq!(fs::read(dest.join("tree/f.txt"))?, b"x");
        assert_eq!(fs::read_link(dest.join("tree/link"))?, Path::new("f.txt"));
        Ok(())
    }

    #[test]
    fn move_tries_rename_first_with_the_final_name() -> TestResult {
        let scratch = Scratch::new("move-rename-first")?;
        let dest = scratch.dir("dest")?;
        fs::write(scratch.path("a.txt"), b"x")?;
        let calls = Cell::new(0);
        let seen = Cell::new(false);
        let source = scratch.path("a.txt");
        let wanted = dest.join("a.txt");

        let report = transfer_with(
            std::slice::from_ref(&source),
            &dest,
            Mode::Move,
            no_conflict,
            |_| {},
            &AtomicBool::new(false),
            &|from, to| {
                calls.set(calls.get() + 1);
                seen.set(from == source && to == wanted);
                rename_noreplace(from, to)
            },
        );

        assert!(matches!(only_item(&report)?, ItemOutcome::Ok { .. }));
        assert_eq!(calls.get(), 1);
        assert!(seen.get());
        Ok(())
    }

    #[test]
    fn move_across_devices_copies_verifies_then_removes_source() -> TestResult {
        let scratch = Scratch::new("move-exdev")?;
        let dest = scratch.dir("dest")?;
        let src = scratch.dir("tree/sub")?;
        let tree = scratch.path("tree");
        fs::write(src.join("deep.bin"), vec![7u8; 2_500_000])?;
        fs::write(tree.join("top.txt"), b"top")?;
        symlink("top.txt", tree.join("link"))?;
        symlink("missing", tree.join("dangling"))?;
        fs::set_permissions(tree.join("top.txt"), fs::Permissions::from_mode(0o640))?;

        let report = run_xdev(std::slice::from_ref(&tree), &dest, no_conflict);

        assert!(
            matches!(only_item(&report)?, ItemOutcome::Ok { .. }),
            "{report:?}"
        );
        assert!(!tree.exists(), "source removed only after the copy");
        let out = dest.join("tree");
        assert_eq!(fs::read(out.join("sub/deep.bin"))?.len(), 2_500_000);
        assert_eq!(fs::read(out.join("top.txt"))?, b"top");
        assert_eq!(fs::metadata(out.join("top.txt"))?.mode() & 0o7777, 0o640);
        assert_eq!(fs::read_link(out.join("link"))?, Path::new("top.txt"));
        assert_eq!(fs::read_link(out.join("dangling"))?, Path::new("missing"));
        assert_eq!(names(&dest)?, ["tree"], "no partial copy left behind");
        assert_eq!(report.bytes, 2_500_003);
        Ok(())
    }

    #[test]
    fn failed_cross_device_move_leaves_source_intact_and_no_partial() -> TestResult {
        let scratch = Scratch::new("move-exdev-fail")?;
        let dest = scratch.dir("dest")?;
        let tree = scratch.dir("tree")?;
        fs::write(tree.join("a.txt"), b"a")?;
        fs::write(tree.join("z.txt"), b"z")?;
        make_fifo(&tree.join("pipe"))?;

        let report = run_xdev(std::slice::from_ref(&tree), &dest, no_conflict);

        assert!(matches!(
            only_item(&report)?,
            ItemOutcome::Failed(TransferError::Unsupported(_))
        ));
        assert_eq!(names(&tree)?, ["a.txt", "pipe", "z.txt"]);
        assert_eq!(fs::read(tree.join("a.txt"))?, b"a");
        assert!(names(&dest)?.is_empty(), "partial copy was removed");
        Ok(())
    }

    #[test]
    fn failed_copy_of_readonly_subdirectory_still_cleans_the_partial() -> TestResult {
        let scratch = Scratch::new("copy-readonly-fail")?;
        let dest = scratch.dir("dest")?;
        let tree = scratch.dir("tree/ro")?;
        fs::write(tree.join("f"), b"f")?;
        fs::set_permissions(&tree, fs::Permissions::from_mode(0o500))?;
        make_fifo(&scratch.path("tree/zz-pipe"))?;

        let report = run(&[scratch.path("tree")], &dest, Mode::Copy, no_conflict);

        assert!(matches!(
            only_item(&report)?,
            ItemOutcome::Failed(TransferError::Unsupported(_))
        ));
        assert!(names(&dest)?.is_empty());
        Ok(())
    }

    // ---- cancel / progress ----

    #[test]
    fn cancel_mid_copy_leaves_no_partial_and_source_intact() -> TestResult {
        let scratch = Scratch::new("cancel")?;
        let dest = scratch.dir("dest")?;
        let src = scratch.path("big.bin");
        fs::write(&src, vec![1u8; 3 * 1024 * 1024])?;
        fs::write(scratch.path("small.txt"), b"s")?;
        let cancel = AtomicBool::new(false);

        let report = transfer(
            &[src.clone(), scratch.path("small.txt")],
            &dest,
            Mode::Copy,
            no_conflict,
            |progress| {
                if progress.bytes_done > 0 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &cancel,
        );

        assert!(report.cancelled);
        assert_eq!(report.items.len(), 2, "every source is reported");
        assert!(
            report
                .items
                .iter()
                .all(|item| matches!(item.outcome, ItemOutcome::Failed(TransferError::Cancelled)))
        );
        assert!(names(&dest)?.is_empty(), "no half file under any name");
        assert_eq!(fs::metadata(&src)?.len(), 3 * 1024 * 1024);
        Ok(())
    }

    #[test]
    fn cancel_mid_cross_device_move_keeps_source() -> TestResult {
        let scratch = Scratch::new("cancel-move")?;
        let dest = scratch.dir("dest")?;
        let src = scratch.path("big.bin");
        fs::write(&src, vec![1u8; 3 * 1024 * 1024])?;
        let cancel = AtomicBool::new(false);

        let report = transfer_with(
            std::slice::from_ref(&src),
            &dest,
            Mode::Move,
            no_conflict,
            |progress| {
                if progress.bytes_done > 0 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &cancel,
            &xdev,
        );

        assert!(report.cancelled);
        assert!(names(&dest)?.is_empty());
        assert_eq!(fs::metadata(&src)?.len(), 3 * 1024 * 1024);
        Ok(())
    }

    #[test]
    fn cancel_before_start_touches_nothing() -> TestResult {
        let scratch = Scratch::new("cancel-early")?;
        let dest = scratch.dir("dest")?;
        fs::write(scratch.path("a.txt"), b"a")?;
        let report = transfer(
            &[scratch.path("a.txt")],
            &dest,
            Mode::Move,
            no_conflict,
            |_| {},
            &AtomicBool::new(true),
        );
        assert!(report.cancelled);
        assert!(scratch.path("a.txt").exists());
        assert!(names(&dest)?.is_empty());
        Ok(())
    }

    #[test]
    fn progress_reports_total_done_and_current_item() -> TestResult {
        let scratch = Scratch::new("progress")?;
        let dest = scratch.dir("dest")?;
        fs::write(scratch.path("a.bin"), vec![0u8; 2_500_000])?;
        fs::write(scratch.path("b.bin"), vec![0u8; 100])?;
        let mut seen = Vec::new();

        let report = transfer(
            &[scratch.path("a.bin"), scratch.path("b.bin")],
            &dest,
            Mode::Copy,
            no_conflict,
            |progress| seen.push(progress),
            &AtomicBool::new(false),
        );

        assert_eq!(report.bytes, 2_500_100);
        assert!(seen.iter().all(|p| p.bytes_total == 2_500_100));
        let last = seen.last().ok_or("no progress")?;
        assert_eq!(last.bytes_done, 2_500_100);
        assert_eq!(last.current, scratch.path("b.bin"));
        assert!(
            seen.iter()
                .zip(seen.iter().skip(1))
                .all(|(a, b)| a.bytes_done <= b.bytes_done)
        );
        assert!(
            seen.iter()
                .filter(|p| p.current == scratch.path("a.bin") && p.bytes_done > 0)
                .count()
                >= 2,
            "large files report per chunk"
        );
        Ok(())
    }

    // ---- conflicts ----

    fn conflict_fixture(scratch: &Scratch) -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
        let dest = scratch.dir("dest")?;
        fs::write(scratch.path("a.txt"), b"new")?;
        fs::write(dest.join("a.txt"), b"old")?;
        Ok((scratch.path("a.txt"), dest))
    }

    #[test]
    fn conflict_skip_keeps_both_untouched() -> TestResult {
        let scratch = Scratch::new("skip")?;
        let (src, dest) = conflict_fixture(&scratch)?;
        let asked = Cell::new(None);
        let report = run(std::slice::from_ref(&src), &dest, Mode::Move, |existing| {
            asked.set(Some(existing.to_path_buf()));
            Conflict::Skip
        });
        assert!(matches!(only_item(&report)?, ItemOutcome::Skipped));
        assert_eq!(asked.take(), Some(dest.join("a.txt")));
        assert_eq!(fs::read(&src)?, b"new");
        assert_eq!(fs::read(dest.join("a.txt"))?, b"old");
        assert_eq!(names(&dest)?, ["a.txt"]);
        Ok(())
    }

    #[test]
    fn conflict_keep_both_uses_a_unique_name() -> TestResult {
        let scratch = Scratch::new("keepboth")?;
        let (src, dest) = conflict_fixture(&scratch)?;
        let report = run(std::slice::from_ref(&src), &dest, Mode::Copy, |_| {
            Conflict::KeepBoth
        });
        let ItemOutcome::Ok { dest: out, .. } = only_item(&report)? else {
            return Err("not ok".into());
        };
        assert_eq!(out, &dest.join("a (2).txt"));
        assert_eq!(fs::read(dest.join("a (2).txt"))?, b"new");
        assert_eq!(fs::read(dest.join("a.txt"))?, b"old");
        Ok(())
    }

    #[test]
    fn conflict_replace_swaps_in_the_new_file_and_drops_the_old() -> TestResult {
        let scratch = Scratch::new("replace")?;
        for (mode, xdev_move) in [(Mode::Copy, false), (Mode::Move, false), (Mode::Move, true)] {
            let (src, dest) = conflict_fixture(&scratch)?;
            let report = if xdev_move {
                run_xdev(std::slice::from_ref(&src), &dest, |_| Conflict::Replace)
            } else {
                run(std::slice::from_ref(&src), &dest, mode, |_| {
                    Conflict::Replace
                })
            };
            assert!(
                matches!(only_item(&report)?, ItemOutcome::Ok { .. }),
                "{report:?}"
            );
            assert_eq!(fs::read(dest.join("a.txt"))?, b"new");
            assert_eq!(names(&dest)?, ["a.txt"], "aside copy deleted: {mode:?}");
            assert_eq!(src.exists(), mode == Mode::Copy);
            fs::remove_dir_all(&dest)?;
            fs::remove_file(&src).ok();
        }
        Ok(())
    }

    #[test]
    fn conflict_cancel_stops_everything() -> TestResult {
        let scratch = Scratch::new("conflict-cancel")?;
        let (src, dest) = conflict_fixture(&scratch)?;
        fs::write(scratch.path("b.txt"), b"b")?;
        let report = run(
            &[src.clone(), scratch.path("b.txt")],
            &dest,
            Mode::Move,
            |_| Conflict::Cancel,
        );
        assert!(report.cancelled);
        assert_eq!(report.items.len(), 2);
        assert!(
            report
                .items
                .iter()
                .all(|item| matches!(item.outcome, ItemOutcome::Failed(TransferError::Cancelled)))
        );
        assert_eq!(fs::read(&src)?, b"new");
        assert_eq!(fs::read(scratch.path("b.txt"))?, b"b");
        assert_eq!(fs::read(dest.join("a.txt"))?, b"old");
        assert_eq!(names(&dest)?, ["a.txt"]);
        Ok(())
    }

    #[test]
    fn dangling_symlink_at_the_target_counts_as_a_conflict() -> TestResult {
        let scratch = Scratch::new("conflict-dangling")?;
        let dest = scratch.dir("dest")?;
        fs::write(scratch.path("a.txt"), b"new")?;
        symlink("missing", dest.join("a.txt"))?;
        let asked = Cell::new(false);
        let report = run(&[scratch.path("a.txt")], &dest, Mode::Copy, |_| {
            asked.set(true);
            Conflict::Skip
        });
        assert!(asked.get());
        assert!(matches!(only_item(&report)?, ItemOutcome::Skipped));
        Ok(())
    }

    #[test]
    fn failed_replace_restores_the_original() -> TestResult {
        let scratch = Scratch::new("replace-fail")?;
        let dest = scratch.dir("dest")?;
        fs::create_dir(dest.join("tree"))?;
        fs::write(dest.join("tree/keep.txt"), b"keep me")?;
        let tree = scratch.dir("tree")?;
        fs::write(tree.join("a.txt"), b"a")?;
        make_fifo(&tree.join("pipe"))?;

        for mode in [Mode::Copy, Mode::Move] {
            // A same-device move is one atomic rename; only the cross-device
            // path copies (and can fail midway).
            let report = if mode == Mode::Move {
                run_xdev(std::slice::from_ref(&tree), &dest, |_| Conflict::Replace)
            } else {
                run(std::slice::from_ref(&tree), &dest, mode, |_| {
                    Conflict::Replace
                })
            };
            assert!(
                matches!(only_item(&report)?, ItemOutcome::Failed(_)),
                "{mode:?}: {report:?}"
            );
            assert_eq!(fs::read(dest.join("tree/keep.txt"))?, b"keep me");
            assert_eq!(
                names(&dest)?,
                ["tree"],
                "no aside or partial left: {mode:?}"
            );
            assert_eq!(names(&dest.join("tree"))?, ["keep.txt"]);
            assert!(tree.join("a.txt").exists());
        }
        Ok(())
    }

    #[test]
    fn replace_across_types_uses_the_same_aside_scheme() -> TestResult {
        let scratch = Scratch::new("replace-types")?;
        let dest = scratch.dir("dest")?;
        fs::create_dir(dest.join("thing"))?;
        fs::write(dest.join("thing/inner"), b"inner")?;
        fs::write(scratch.path("thing"), b"file now")?;

        let report = run(&[scratch.path("thing")], &dest, Mode::Copy, |_| {
            Conflict::Replace
        });

        assert!(matches!(only_item(&report)?, ItemOutcome::Ok { .. }));
        assert_eq!(fs::read(dest.join("thing"))?, b"file now");
        assert_eq!(names(&dest)?, ["thing"]);
        Ok(())
    }

    #[test]
    fn replace_never_removes_an_ancestor_of_the_source() -> TestResult {
        let scratch = Scratch::new("replace-ancestor")?;
        let outer = scratch.dir("d/x")?;
        fs::write(outer.join("keep.txt"), b"keep")?;
        let nested = scratch.dir("d/x/inner")?;
        fs::create_dir(nested.join("x"))?;
        let report = run(&[nested.join("x")], &scratch.path("d"), Mode::Move, |_| {
            Conflict::Replace
        });
        assert!(matches!(
            only_item(&report)?,
            ItemOutcome::Failed(TransferError::IntoItself)
        ));
        assert_eq!(fs::read(outer.join("keep.txt"))?, b"keep");
        assert!(nested.join("x").is_dir());
        Ok(())
    }

    #[test]
    fn one_failed_item_does_not_stop_the_others() -> TestResult {
        let scratch = Scratch::new("partial-batch")?;
        let dest = scratch.dir("dest")?;
        fs::write(scratch.path("a.txt"), b"a")?;
        let report = run(
            &[scratch.path("missing.txt"), scratch.path("a.txt")],
            &dest,
            Mode::Copy,
            no_conflict,
        );
        assert_eq!(report.items.len(), 2);
        assert!(matches!(
            report.items.first().map(|i| &i.outcome),
            Some(ItemOutcome::Failed(TransferError::Io(_)))
        ));
        assert!(matches!(
            report.items.get(1).map(|i| &i.outcome),
            Some(ItemOutcome::Ok { .. })
        ));
        assert_eq!(fs::read(dest.join("a.txt"))?, b"a");
        Ok(())
    }

    #[test]
    fn remove_permanently_deletes_trees_and_links_without_following() -> TestResult {
        let scratch = Scratch::new("remove-permanently")?;
        let outside = scratch.dir("outside")?;
        fs::write(outside.join("precious.txt"), b"precious")?;
        let tree = scratch.dir("tree/sub")?;
        fs::write(tree.join("file.txt"), b"x")?;
        std::os::unix::fs::symlink(&outside, scratch.path("tree/link"))?;
        remove_permanently(&scratch.path("tree"))?;
        assert!(!scratch.path("tree").exists());
        assert_eq!(fs::read(outside.join("precious.txt"))?, b"precious");

        std::os::unix::fs::symlink(&outside, scratch.path("lone-link"))?;
        remove_permanently(&scratch.path("lone-link"))?;
        assert!(outside.is_dir(), "a lone link is unlinked, not followed");
        assert!(remove_permanently(&scratch.path("missing")).is_err());
        Ok(())
    }
}
