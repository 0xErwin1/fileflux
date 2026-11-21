//! Filesystem mutations: create, rename, copy and move. Nothing here ever
//! overwrites silently, follows symlinks for copy/move, or deletes a source
//! before its replacement is verified in place.

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use rustix::fs::{CWD, RenameFlags, renameat_with};
use rustix::io::Errno;

use super::places::FuseRoots;

/// Longest file name the filesystems we target accept, in bytes.
pub(crate) const MAX_NAME_BYTES: usize = 255;
/// Bytes copied between cancel checks and progress reports.
const CHUNK: usize = 1 << 20;

/// The filesystem calls a transfer makes whose answer differs on FUSE and
/// gvfs (phones, shares): a transfer reaches them through this seam so
/// tests can make each one fail the way such a filesystem does.
trait Backend {
    /// The first attempt of a move: a rename that never replaces.
    fn move_rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        rename_noreplace(from, to)
    }

    /// Puts a finished partial copy under its real name, never replacing.
    fn place_rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        rename_noreplace(from, to)
    }

    fn sync(&self, file: &fs::File) -> io::Result<()> {
        file.sync_all()
    }

    fn write(&self, file: &mut fs::File, data: &[u8]) -> io::Result<()> {
        file.write_all(data)
    }

    fn set_modified(&self, file: &fs::File, time: SystemTime) -> io::Result<()> {
        file.set_modified(time)
    }

    fn set_permissions(&self, file: &fs::File, permissions: fs::Permissions) -> io::Result<()> {
        file.set_permissions(permissions)
    }

    /// Whether `dir` is on FUSE or gvfs, where metadata and atomic renames
    /// may be unavailable.
    fn is_weak(&self, dir: &Path) -> bool {
        FuseRoots::system().covers(dir)
    }

    /// The identity source of an entry a direct copy just created.
    fn created_metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        fs::symlink_metadata(path)
    }
}

/// The real filesystem.
struct System;

impl Backend for System {}

/// The real filesystem with the first rename of a move replaced.
#[cfg(test)]
struct MoveVia<'a>(&'a dyn Fn(&Path, &Path) -> io::Result<()>);

#[cfg(test)]
impl Backend for MoveVia<'_> {
    fn move_rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        (self.0)(from, to)
    }
}

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
    /// Hidden originals a successful Replace could not delete afterwards;
    /// the transfer itself succeeded, but this data is still on disk.
    pub leftovers: Vec<(PathBuf, io::Error)>,
    /// What a weaker filesystem (gvfs, FUSE) could not do and the transfer
    /// went on without: timestamps, modes. Each is said once.
    pub notes: Vec<String>,
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
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    rename_noreplace_via(from, to, || {
        renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE)
    })
}

/// [`rename_noreplace`] with the `renameat2` call injectable. A kernel or
/// filesystem that cannot do `RENAME_NOREPLACE` (`ENOSYS`, `EINVAL`,
/// `ENOTSUP`: FUSE and gvfs answer so) falls back to the pre-check and a
/// plain rename; any other failure, `EXDEV` included, is returned as it is.
fn rename_noreplace_via(
    from: &Path,
    to: &Path,
    attempt: impl FnOnce() -> Result<(), Errno>,
) -> io::Result<()> {
    match attempt() {
        Ok(()) => Ok(()),
        Err(Errno::NOSYS | Errno::INVAL | Errno::NOTSUP) => {
            if occupied(to)? {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            fs::rename(from, to)
        }
        Err(error) => Err(error.into()),
    }
}

/// Whether anything, including a dangling symlink, occupies `path`. Only
/// `NotFound` means free; any other failure (`ENAMETOOLONG`, `EACCES`, ...)
/// is returned, because treating it as "taken" makes name searches spin on
/// a name that can never be tested.
fn occupied(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
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

/// `name` with ` (n)` inserted before the extension (folders have none),
/// shortened so the result fits a file name.
pub(crate) fn numbered_name(name: &OsStr, n: u32, is_dir: bool) -> OsString {
    fit_name(name, &format!(" ({n})"), is_dir, MAX_NAME_BYTES)
}

/// `name` with `suffix` inserted before the extension (folders have none),
/// at most `max` bytes long: when it would be longer, the stem is cut back
/// (never into the middle of a UTF-8 sequence; names that are not UTF-8 are
/// cut byte-wise) while the extension stays. An extension too long to leave
/// room is treated as part of the stem.
pub(crate) fn fit_name(name: &OsStr, suffix: &str, is_dir: bool, max: usize) -> OsString {
    let bytes = name.as_bytes();
    let mut split = if is_dir {
        bytes.len()
    } else {
        extension_start(bytes).unwrap_or(bytes.len())
    };
    if suffix.len() + bytes.len().saturating_sub(split) > max {
        split = bytes.len();
    }
    let extension = bytes.get(split..).unwrap_or_default();
    let room = max.saturating_sub(suffix.len() + extension.len());
    let mut cut = split.min(room);
    // Step back off UTF-8 continuation bytes (at most three of them).
    for _ in 0..3 {
        if cut > 0 && bytes.get(cut).is_some_and(|byte| byte & 0xC0 == 0x80) {
            cut -= 1;
        }
    }
    let mut out = bytes.get(..cut).unwrap_or_default().to_vec();
    out.extend_from_slice(suffix.as_bytes());
    out.extend_from_slice(extension);
    OsString::from_vec(out)
}

/// How many numbered candidates a name search tries before giving up.
const MAX_NAME_ATTEMPTS: u32 = 100_000;

/// A name for `dir` that does not exist yet: `name` itself when free, else
/// "report.pdf" → "report (2).pdf", "archive.tar.gz" → "archive (2).tar.gz",
/// "Photos" → "Photos (2)", counting up past existing entries. Fails when a
/// candidate cannot be tested (anything but "not found") or none is free
/// within a bounded number of tries.
pub fn unique_name(dir: &Path, name: &OsStr) -> io::Result<OsString> {
    unique_name_cancellable(dir, name, &AtomicBool::new(false))
}

/// [`unique_name`] that gives up with `Interrupted` once `cancel` is set.
pub fn unique_name_cancellable(
    dir: &Path,
    name: &OsStr,
    cancel: &AtomicBool,
) -> io::Result<OsString> {
    let first = dir.join(name);
    if !occupied(&first)? {
        return Ok(name.to_owned());
    }
    let is_dir = fs::symlink_metadata(&first).is_ok_and(|meta| meta.is_dir());
    for n in 2..=MAX_NAME_ATTEMPTS {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let candidate = numbered_name(name, n, is_dir);
        if !occupied(&dir.join(&candidate))? {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free numbered name",
    ))
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
    transfer_on(sources, dest_dir, mode, conflict, progress, cancel, &System)
}

/// `transfer` with the move's first `rename` attempt injectable, so the
/// cross-device fallback is testable without two filesystems.
#[cfg(test)]
fn transfer_with(
    sources: &[PathBuf],
    dest_dir: &Path,
    mode: Mode,
    conflict: impl FnMut(&Path) -> Conflict,
    progress: impl FnMut(Progress),
    cancel: &AtomicBool,
    rename: &dyn Fn(&Path, &Path) -> io::Result<()>,
) -> TransferReport {
    transfer_on(
        sources,
        dest_dir,
        mode,
        conflict,
        progress,
        cancel,
        &MoveVia(rename),
    )
}

/// `transfer` over an injectable filesystem [`Backend`].
fn transfer_on(
    sources: &[PathBuf],
    dest_dir: &Path,
    mode: Mode,
    mut conflict: impl FnMut(&Path) -> Conflict,
    mut progress: impl FnMut(Progress),
    cancel: &AtomicBool,
    backend: &dyn Backend,
) -> TransferReport {
    let sizes: Vec<u64> = sources.iter().map(|source| tree_size(source)).collect();
    let mut ctx = Ctx {
        backend,
        progress: &mut progress,
        cancel,
        total: sizes.iter().sum(),
        done: 0,
        current: PathBuf::new(),
        seq: 0,
        deferred: Vec::new(),
        leftovers: Vec::new(),
        notes: Vec::new(),
        weak: false,
        direct: false,
        made: None,
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
    report.leftovers = ctx.leftovers;
    report.notes = ctx.notes;
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

/// Removes the moved-aside original of a replace. A read-only directory
/// inside it would block the plain removal, so on failure the directories
/// of this tree (and only them: nothing is followed, a symlink is just
/// unlinked) get owner rwx and the removal is repeated.
fn remove_aside(aside: &Path) -> io::Result<()> {
    if remove_all(aside).is_ok() {
        return Ok(());
    }
    remove_tree_unlocking(aside)
}

fn remove_tree_unlocking(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return fs::remove_file(path);
    }
    let mode = meta.mode() & 0o7777;
    if mode & 0o700 != 0o700 {
        unlock_directory(path, mode)?;
    }
    let mut first_error = None;
    for entry in fs::read_dir(path)? {
        let result = entry.and_then(|entry| remove_tree_unlocking(&entry.path()));
        if let Err(error) = result {
            first_error.get_or_insert(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => fs::remove_dir(path),
    }
}

/// Gives the directory at `path` owner rwx. The directory is opened with
/// `O_PATH | O_NOFOLLOW | O_DIRECTORY`, so a symlink (or anything swapped
/// in for the directory) is refused instead of followed, and the mode is
/// then changed through that descriptor.
fn unlock_directory(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = rustix::fs::openat(
        CWD,
        path,
        rustix::fs::OFlags::PATH
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    let through_fd = PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()));
    fs::set_permissions(through_fd, fs::Permissions::from_mode(mode | 0o700))
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
    backend: &'a dyn Backend,
    progress: &'a mut dyn FnMut(Progress),
    cancel: &'a AtomicBool,
    total: u64,
    done: u64,
    current: PathBuf,
    seq: u32,
    deferred: Vec<DeferredDir>,
    leftovers: Vec<(PathBuf, io::Error)>,
    notes: Vec<String>,
    /// The destination of the item in hand is on FUSE or gvfs.
    weak: bool,
    /// A weak destination refused the rename that puts a partial copy under
    /// its real name: the rest of the transfer copies straight to it.
    direct: bool,
    /// What a direct copy has created so far, in order, so a failure removes
    /// exactly that and nothing a third party wrote there meanwhile. `None`
    /// outside a direct copy.
    made: Option<Vec<Made>>,
}

/// Device, inode and birth time of a file. Device and inode alone are not
/// enough: ext4 hands a deleted file's inode number to the next file
/// created, so a file that replaced ours would look like ours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    born: Option<std::time::SystemTime>,
}

impl FileIdentity {
    fn of(meta: &fs::Metadata) -> FileIdentity {
        FileIdentity {
            dev: meta.dev(),
            ino: meta.ino(),
            born: meta.created().ok(),
        }
    }
}

/// An entry a direct copy created, with the identity it had when created.
struct Made {
    path: PathBuf,
    is_dir: bool,
    /// Device, inode and birth time: a different file at the same path is
    /// not ours.
    /// `None` when looking at the entry failed right after creating it: it is
    /// then rolled back by path alone, as it was before identities existed.
    identity: Option<FileIdentity>,
}

impl Made {
    /// Whether `path` still names the entry that was created.
    fn is_still_there(&self) -> bool {
        let Ok(meta) = fs::symlink_metadata(&self.path) else {
            return false;
        };
        self.identity
            .is_none_or(|identity| FileIdentity::of(&meta) == identity)
    }
}

/// What a transfer says once when a weak filesystem refuses the metadata.
const METADATA_NOTE: &str = "Modification times and permissions could not be set on this filesystem; the copies have the time they were made and default permissions";

/// Whether a failed rename means "this filesystem cannot do that" rather
/// than a problem with the names involved.
fn rename_unsupported(error: &io::Error) -> bool {
    is_errno(
        error,
        &[
            Errno::NOTSUP,
            Errno::NOSYS,
            Errno::INVAL,
            Errno::XDEV,
            Errno::PERM,
        ],
    )
}

/// Whether `error` is one of the OS errors in `any`.
fn is_errno(error: &io::Error, any: &[Errno]) -> bool {
    any.iter()
        .any(|errno| error.raw_os_error() == Some(errno.raw_os_error()))
}

/// Whether a failed timestamp or mode change means "this filesystem does not
/// keep that" (`ENOTSUP`, the same number as `EOPNOTSUPP` on Linux; `ENOSYS`;
/// `EINVAL`, which MTP backends answer; the `EPERM`/`EACCES` FUSE answers with) rather than a real failure such as
/// `EIO` or a full disk.
fn metadata_unsupported(error: &io::Error) -> bool {
    is_errno(
        error,
        &[
            Errno::NOTSUP,
            Errno::NOSYS,
            Errno::INVAL,
            Errno::PERM,
            Errno::ACCESS,
        ],
    )
}

impl Ctx<'_> {
    /// `fsync` where a weak filesystem has none is not a failure: there is
    /// nothing more to wait for. On a disk, any failure is one.
    fn sync_file(&self, file: &fs::File) -> io::Result<()> {
        match self.backend.sync(file) {
            Err(error)
                if self.weak && is_errno(&error, &[Errno::NOTSUP, Errno::INVAL, Errno::NOSYS]) =>
            {
                Ok(())
            }
            other => other,
        }
    }

    /// A timestamp or mode that a weak filesystem says it does not keep is
    /// noted, once, and the transfer goes on; on a disk, and for any other
    /// failure on a weak filesystem (`EIO`...), it is still a failure.
    fn lenient(&mut self, result: io::Result<()>) -> io::Result<()> {
        match result {
            Err(error) if self.weak && metadata_unsupported(&error) => {
                if !self.notes.iter().any(|note| note == METADATA_NOTE) {
                    self.notes.push(METADATA_NOTE.to_owned());
                }
                Ok(())
            }
            other => other,
        }
    }

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
    /// moved-aside original of a replace (see [`is_transfer_temp`]).
    fn temp_name(&mut self, dir: &Path, tag: &str) -> io::Result<PathBuf> {
        for _ in 0..MAX_NAME_ATTEMPTS {
            self.seq += 1;
            let candidate = dir.join(format!(
                "{TEMP_PREFIX}{tag}-{}-{}",
                std::process::id(),
                self.seq
            ));
            if !occupied(&candidate)? {
                return Ok(candidate);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free temporary name",
        ))
    }

    /// [`unique_name_cancellable`] bound to this transfer's cancel flag.
    fn unique(&self, dir: &Path, name: &OsStr) -> Result<PathBuf, TransferError> {
        match unique_name_cancellable(dir, name, self.cancel) {
            Ok(unique) => Ok(dir.join(unique)),
            Err(_) if self.cancel.load(Ordering::Relaxed) => Err(TransferError::Cancelled),
            Err(error) => Err(error.into()),
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

        self.weak = self.backend.is_weak(&dest_canon);
        let mut target = dest_dir.join(name);
        let mut replace = false;
        if dest_canon == parent_canon {
            match mode {
                Mode::Move => return Ok(None),
                Mode::Copy => target = self.unique(dest_dir, name)?,
            }
        } else if occupied(&target)? {
            match conflict(&target) {
                Conflict::Skip => return Ok(None),
                Conflict::Cancel => return Err(TransferError::Cancelled),
                Conflict::KeepBoth => target = self.unique(dest_dir, name)?,
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
            let aside = self.temp_name(dir, "aside")?;
            fs::rename(target, &aside)?;
            Some(aside)
        } else {
            None
        };
        match self.materialize(src, target, mode, size) {
            Ok(placed) => {
                if let Some(aside) = aside {
                    // The new copy is in place, so a stuck aside copy is not
                    // a failure of the transfer; it is reported instead.
                    if let Err(error) = remove_aside(&aside) {
                        self.leftovers.push((aside, error));
                    }
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
            match self.backend.move_rename(src, target) {
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
        let before = self.done;
        if !(self.weak && self.direct) && !self.copy_via_partial(src, target, dir)? {
            // The filesystem cannot put the finished copy under its name:
            // the partial copy is gone and the data is written once more,
            // straight to the name.
            self.done = before;
            self.direct = true;
        }
        if self.direct && self.weak {
            self.copy_direct(src, target)?;
        }
        let leftover = if mode == Mode::Move {
            remove_all(src).err()
        } else {
            None
        };
        Ok(Placed { leftover })
    }

    /// Builds the copy under a temporary sibling name and renames it into
    /// place. `Ok(false)` when the destination is weak and refused that
    /// rename (the partial copy is removed).
    fn copy_via_partial(
        &mut self,
        src: &Path,
        target: &Path,
        dir: &Path,
    ) -> Result<bool, TransferError> {
        let partial = self.temp_name(dir, "partial")?;
        self.deferred.clear();
        let built = self
            .copy_node(src, &partial)
            .and_then(|()| self.finalize_dirs().map_err(TransferError::from));
        let placed = match built {
            Ok(()) => self.backend.place_rename(&partial, target),
            Err(error) => {
                self.deferred.clear();
                remove_all(&partial).ok();
                return Err(error);
            }
        };
        match placed {
            Ok(()) => Ok(true),
            Err(error) if self.weak && rename_unsupported(&error) => {
                remove_all(&partial).ok();
                Ok(false)
            }
            Err(error) => {
                remove_all(&partial).ok();
                Err(error.into())
            }
        }
    }

    /// Writes the copy straight to `target`, which must not exist (a name
    /// that appeared meanwhile is never overwritten or deleted). A failed or
    /// cancelled copy deletes exactly the entries it created, newest first;
    /// a folder that somebody else put something into stays.
    fn copy_direct(&mut self, src: &Path, target: &Path) -> Result<(), TransferError> {
        if occupied(target)? {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists).into());
        }
        self.deferred.clear();
        self.made = Some(Vec::new());
        let built = self
            .copy_node(src, target)
            .and_then(|()| self.finalize_dirs().map_err(TransferError::from));
        let made = self.made.take().unwrap_or_default();
        if built.is_err() {
            self.deferred.clear();
            for entry in made.iter().rev() {
                // A different file now at the path is not ours. `remove_dir`
                // only takes an empty folder, `remove_file` never a folder.
                if !entry.is_still_there() {
                    continue;
                }
                if entry.is_dir {
                    fs::remove_dir(&entry.path).ok();
                } else {
                    fs::remove_file(&entry.path).ok();
                }
            }
        }
        built
    }

    /// Notes an entry a direct copy created, by the identity it has now.
    fn made(&mut self, path: &Path, is_dir: bool) {
        if let Some(made) = &mut self.made {
            let identity = match self.backend.created_metadata(path) {
                Ok(meta) => Some(FileIdentity::of(&meta)),
                Err(error) => {
                    log::debug!(
                        "no identity for the created {}: {error}; rolling it back by path",
                        path.display()
                    );
                    None
                }
            };
            made.push(Made {
                path: path.to_path_buf(),
                is_dir,
                identity,
            });
        }
    }

    fn finalize_dirs(&mut self) -> io::Result<()> {
        while let Some(dir) = self.deferred.pop() {
            let file = match fs::File::open(&dir.path) {
                Ok(file) => file,
                Err(error) => {
                    self.lenient(Err(error))?;
                    continue;
                }
            };
            if let Some(modified) = dir.modified {
                let result = self.backend.set_modified(&file, modified);
                self.lenient(result)?;
            }
            let result = self.backend.set_permissions(&file, dir.permissions);
            self.lenient(result)?;
        }
        Ok(())
    }

    fn copy_node(&mut self, src: &Path, dst: &Path) -> Result<(), TransferError> {
        self.check_cancel()?;
        let meta = fs::symlink_metadata(src)?;
        let kind = meta.file_type();
        if kind.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
            self.made(dst, false);
        } else if kind.is_dir() {
            fs::create_dir(dst)?;
            self.made(dst, true);
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
        self.made(dst, false);
        let mut buffer = vec![0u8; CHUNK];
        let mut copied = 0u64;
        loop {
            self.check_cancel()?;
            let read = input.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            self.backend
                .write(&mut output, buffer.get(..read).unwrap_or_default())?;
            copied += read as u64;
            self.done += read as u64;
            self.emit();
        }
        self.sync_file(&output)?;
        if let Ok(modified) = meta.modified() {
            let result = self.backend.set_modified(&output, modified);
            self.lenient(result)?;
        }
        let result = self.backend.set_permissions(&output, meta.permissions());
        self.lenient(result)?;
        // A weak filesystem may only know the final size once the file is
        // closed (gvfs hands the data over then): ask the path instead.
        let written = if self.weak {
            drop(output);
            fs::metadata(dst)?.len()
        } else {
            output.metadata()?.len()
        };
        if copied != expected || written != copied {
            return Err(TransferError::VerifyFailed(src.to_path_buf()));
        }
        Ok(())
    }
}

/// Prefix of the hidden names a transfer works under before it places an
/// item; with [`TEMP_TAGS`] it identifies them for listings.
const TEMP_PREFIX: &str = ".fileflux-";
/// The temp-name tags `transfer` uses: partial copies and moved-aside
/// originals of a replace.
const TEMP_TAGS: [&str; 2] = ["partial-", "aside-"];

/// Whether `name` is an in-flight transfer temp file (a partial copy or a
/// moved-aside original), which listings hide so users never act on it.
pub fn is_transfer_temp(name: &OsStr) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix(TEMP_PREFIX))
        .is_some_and(|rest| TEMP_TAGS.iter().any(|tag| rest.starts_with(tag)))
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

    /// Runs `work` on its own thread and fails instead of hanging when it
    /// does not finish in time.
    fn bounded<T: Send + 'static>(
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, Box<dyn Error>> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            tx.send(work()).ok();
        });
        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|_| "operation did not finish in time".into())
    }

    #[test]
    fn unique_name_terminates_for_long_names() -> TestResult {
        let s = Scratch::new("longname")?;
        let name = OsString::from("N".repeat(253));
        fs::write(s.0.join(&name), b"x")?;
        let (tx, rx) = std::sync::mpsc::channel();
        let dir = s.0.clone();
        let n = name.clone();
        std::thread::spawn(move || {
            tx.send(unique_name(&dir, &n)).ok();
        });
        let got = rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "unique_name hung")??;
        assert!(got.len() <= 255 && !s.0.join(&got).exists());
        Ok(())
    }

    #[test]
    fn numbered_name_fits_255_bytes_on_char_boundaries_and_keeps_the_extension() {
        let accented = OsString::from(format!("{}.txt", "é".repeat(125)));
        let got = numbered_name(&accented, 2, false);
        assert!(got.len() <= 255, "{}", got.len());
        let text = got.to_str().unwrap_or_default();
        assert!(text.ends_with(" (2).txt"), "{text}");
        assert!(text.starts_with('é'));

        let raw = OsString::from_vec([&[0xFFu8; 253][..], b".gz"].concat());
        let got = numbered_name(&raw, 12, false);
        assert!(got.len() <= 255);
        assert!(got.as_bytes().ends_with(b" (12).gz"));
        assert!(got.as_bytes().starts_with(&[0xFF, 0xFF]));

        // An absurd extension is treated as part of the stem.
        let odd = OsString::from(format!("a.{}", "x".repeat(253)));
        assert!(numbered_name(&odd, 2, false).len() <= 255);

        // Short names are untouched.
        assert_eq!(numbered_name(OsStr::new("a.txt"), 2, false), "a (2).txt");
    }

    #[test]
    fn unique_name_propagates_errors_other_than_not_found() -> TestResult {
        let scratch = Scratch::new("unique-eacces")?;
        let locked = scratch.dir("locked")?;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))?;
        if fs::read_dir(&locked).is_ok() {
            return Ok(()); // running as root: permissions do not apply
        }
        let got = bounded(move || unique_name(&locked, OsStr::new("a.txt")))?;
        assert!(got.is_err(), "an unreadable folder is not 'free': {got:?}");
        Ok(())
    }

    #[test]
    fn unique_name_stops_when_cancelled() -> TestResult {
        let scratch = Scratch::new("unique-cancel")?;
        fs::write(scratch.path("a.txt"), b"x")?;
        let cancel = AtomicBool::new(true);
        let got = unique_name_cancellable(&scratch.0, OsStr::new("a.txt"), &cancel);
        assert!(got.is_err());
        Ok(())
    }

    #[test]
    fn copy_into_its_own_folder_shortens_a_253_byte_name() -> TestResult {
        let scratch = Scratch::new("copy-long")?;
        let name = "L".repeat(253);
        fs::write(scratch.path(&name), b"data")?;
        let (src, dir) = (scratch.path(&name), scratch.0.clone());
        let report = bounded(move || run(&[src], &dir, Mode::Copy, no_conflict))?;
        let ItemOutcome::Ok { dest, .. } = only_item(&report)? else {
            return Err(format!("not ok: {report:?}").into());
        };
        let copy_name = dest.file_name().ok_or("no name")?;
        assert!(copy_name.len() <= 255 && copy_name.as_bytes().ends_with(b" (2)"));
        assert_ne!(copy_name, OsStr::new(&name));
        assert_eq!(fs::read(dest)?, b"data");
        assert_eq!(fs::read(scratch.path(&name))?, b"data");
        assert_eq!(names(&scratch.0)?.len(), 2);
        Ok(())
    }

    #[test]
    fn keep_both_shortens_a_253_byte_name() -> TestResult {
        let scratch = Scratch::new("keep-long")?;
        let dest = scratch.dir("dest")?;
        let name = format!("{}.txt", "K".repeat(249));
        fs::write(scratch.path(&name), b"new")?;
        fs::write(dest.join(&name), b"old")?;
        let src = scratch.path(&name);
        let dest2 = dest.clone();
        let report = bounded(move || run(&[src], &dest2, Mode::Copy, |_| Conflict::KeepBoth))?;
        let ItemOutcome::Ok { dest: out, .. } = only_item(&report)? else {
            return Err(format!("not ok: {report:?}").into());
        };
        let out_name = out.file_name().ok_or("no name")?;
        assert!(out_name.len() <= 255 && out_name.as_bytes().ends_with(b" (2).txt"));
        assert_eq!(fs::read(out)?, b"new");
        assert_eq!(fs::read(dest.join(&name))?, b"old");
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

        assert_eq!(
            unique_name(dir, OsStr::new("report.pdf"))?,
            "report (2).pdf"
        );
        assert_eq!(
            unique_name(dir, OsStr::new("archive.tar.gz"))?,
            "archive (2).tar.gz"
        );
        assert_eq!(unique_name(dir, OsStr::new("noext"))?, "noext (2)");
        assert_eq!(unique_name(dir, OsStr::new(".bashrc"))?, ".bashrc (2)");
        assert_eq!(unique_name(dir, OsStr::new("Photos"))?, "Photos (2)");
        assert_eq!(unique_name(dir, OsStr::new("v1.2"))?, "v1.2 (2)");
        assert_eq!(unique_name(dir, OsStr::new("free.txt"))?, "free.txt");
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
        assert_eq!(
            unique_name(dir, OsStr::new("report.pdf"))?,
            "report (4).pdf"
        );
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
    fn replace_leaves_no_hidden_aside() -> TestResult {
        let scratch = Scratch::new("replace-aside")?;
        let dest = scratch.dir("dest")?;
        let outside = scratch.dir("outside")?;
        fs::write(outside.join("precious.txt"), b"precious")?;
        let old = dest.join("tree");
        fs::create_dir_all(old.join("ro/deeper"))?;
        fs::write(old.join("ro/file.txt"), b"old")?;
        fs::write(old.join("ro/deeper/x"), b"old")?;
        symlink(&outside, old.join("ro/link-out"))?;
        fs::set_permissions(old.join("ro/deeper"), fs::Permissions::from_mode(0o000))?;
        fs::set_permissions(old.join("ro"), fs::Permissions::from_mode(0o555))?;
        let new = scratch.dir("new/tree")?;
        fs::write(new.join("a.txt"), b"new")?;

        let report = run(&[new], &dest, Mode::Copy, |_| Conflict::Replace);

        assert!(matches!(only_item(&report)?, ItemOutcome::Ok { .. }));
        assert!(report.leftovers.is_empty(), "{:?}", report.leftovers);
        assert_eq!(names(&dest)?, ["tree"], "no hidden aside left");
        assert_eq!(fs::read(dest.join("tree/a.txt"))?, b"new");
        assert_eq!(
            fs::read(outside.join("precious.txt"))?,
            b"precious",
            "links inside the aside are never followed"
        );
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
    // --- FUSE and gvfs: weaker filesystem semantics ---------------------------

    use std::cell::RefCell;
    use std::sync::Arc;

    fn errno(error: Errno) -> io::Error {
        io::Error::from_raw_os_error(error.raw_os_error())
    }

    /// A filesystem that fails the way FUSE and gvfs do, on request.
    #[derive(Default)]
    struct Fuse {
        weak: bool,
        place_rename: Option<Errno>,
        /// Creates this file before the placing rename fails: somebody else
        /// took the name in the meantime.
        racer: Option<PathBuf>,
        sync: Option<Errno>,
        set_modified: Option<Errno>,
        set_permissions: Option<Errno>,
        /// The write numbered from this one (1-based) fails with `ENOSPC`.
        full_from: Option<usize>,
        /// Looking at an entry right after creating it fails.
        stat_fails: bool,
        /// The write with this number raises the cancel flag.
        cancel_at: Option<(usize, Arc<AtomicBool>)>,
        writes: Cell<usize>,
        places: Cell<usize>,
        seen: RefCell<Vec<String>>,
    }

    impl Backend for Fuse {
        fn place_rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.places.set(self.places.get() + 1);
            if let Some(error) = self.place_rename {
                if let Some(racer) = &self.racer {
                    fs::write(racer, b"theirs")?;
                }
                return Err(errno(error));
            }
            rename_noreplace(from, to)
        }

        fn created_metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
            if self.stat_fails {
                return Err(errno(Errno::IO));
            }
            fs::symlink_metadata(path)
        }

        fn sync(&self, file: &fs::File) -> io::Result<()> {
            match self.sync {
                Some(error) => Err(errno(error)),
                None => file.sync_all(),
            }
        }

        fn write(&self, file: &mut fs::File, data: &[u8]) -> io::Result<()> {
            let number = self.writes.get() + 1;
            self.writes.set(number);
            if self.full_from.is_some_and(|from| number >= from) {
                return Err(errno(Errno::NOSPC));
            }
            if let Some((at, flag)) = &self.cancel_at
                && *at == number
            {
                flag.store(true, Ordering::Relaxed);
            }
            file.write_all(data)
        }

        fn set_modified(&self, file: &fs::File, time: SystemTime) -> io::Result<()> {
            self.seen.borrow_mut().push("mtime".to_owned());
            match self.set_modified {
                Some(error) => Err(errno(error)),
                None => file.set_modified(time),
            }
        }

        fn set_permissions(&self, file: &fs::File, permissions: fs::Permissions) -> io::Result<()> {
            self.seen.borrow_mut().push("mode".to_owned());
            match self.set_permissions {
                Some(error) => Err(errno(error)),
                None => file.set_permissions(permissions),
            }
        }

        fn is_weak(&self, _dir: &Path) -> bool {
            self.weak
        }
    }

    fn run_on(
        fuse: &Fuse,
        sources: &[PathBuf],
        dest: &Path,
        mode: Mode,
        cancel: &AtomicBool,
    ) -> TransferReport {
        transfer_on(sources, dest, mode, no_conflict, |_| {}, cancel, fuse)
    }

    fn copy_on(fuse: &Fuse, sources: &[PathBuf], dest: &Path) -> TransferReport {
        run_on(fuse, sources, dest, Mode::Copy, &AtomicBool::new(false))
    }

    /// The simulated gvfs root: `run/gvfs/mtp:host=Fake_Phone_123/Internal
    /// shared storage/DCIM`, and a source folder elsewhere.
    fn phone(scratch: &Scratch) -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
        let dcim = scratch.dir("run/gvfs/mtp:host=Fake_Phone_123/Internal shared storage/DCIM")?;
        let src = scratch.dir("src")?;
        Ok((src, dcim))
    }

    fn big(size: usize) -> Vec<u8> {
        (0..size).map(|n| (n % 251) as u8).collect()
    }

    fn only_ok(report: &TransferReport) -> Result<PathBuf, Box<dyn Error>> {
        match only_item(report)? {
            ItemOutcome::Ok { dest, .. } => Ok(dest.clone()),
            other => Err(format!("expected a placed item, got {other:?}").into()),
        }
    }

    #[test]
    fn no_replace_rename_falls_back_when_the_filesystem_cannot_do_it() -> TestResult {
        let scratch = Scratch::new("noreplace-via")?;
        let from = scratch.path("from");
        let to = scratch.path("to");
        for unsupported in [Errno::INVAL, Errno::NOTSUP, Errno::NOSYS] {
            fs::write(&from, b"payload")?;
            rename_noreplace_via(&from, &to, || Err(unsupported))?;
            assert_eq!(fs::read(&to)?, b"payload", "{unsupported:?}");
            assert!(!from.exists());
            fs::remove_file(&to)?;
        }
        // The documented race: with the target taken, nothing moves.
        fs::write(&from, b"payload")?;
        fs::write(&to, b"taken")?;
        let error = rename_noreplace_via(&from, &to, || Err(Errno::INVAL))
            .err()
            .ok_or("replaced an existing target")?;
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&to)?, b"taken");
        assert!(from.exists());
        // Other errors are not hidden: EXDEV is how a move learns to copy.
        let error = rename_noreplace_via(&from, &scratch.path("elsewhere"), || Err(Errno::XDEV))
            .err()
            .ok_or("EXDEV was swallowed")?;
        assert_eq!(error.raw_os_error(), Some(Errno::XDEV.raw_os_error()));
        assert!(from.exists());
        Ok(())
    }

    #[test]
    fn fsync_unsupported_is_not_a_failure_on_a_weak_filesystem() -> TestResult {
        for unsupported in [Errno::NOTSUP, Errno::INVAL] {
            let scratch = Scratch::new("fsync")?;
            let (src, dcim) = phone(&scratch)?;
            fs::write(src.join("a.txt"), b"hello")?;
            let fuse = Fuse {
                weak: true,
                sync: Some(unsupported),
                ..Fuse::default()
            };

            let report = copy_on(&fuse, &[src.join("a.txt")], &dcim);

            let dest = only_ok(&report)?;
            assert_eq!(fs::read(dest)?, b"hello", "{unsupported:?}");
            assert!(report.notes.is_empty(), "no note for a missing fsync");
        }
        Ok(())
    }

    #[test]
    fn a_real_fsync_failure_still_fails_and_leaves_nothing() -> TestResult {
        let scratch = Scratch::new("fsync-eio")?;
        let (src, dcim) = phone(&scratch)?;
        fs::write(src.join("a.txt"), b"hello")?;
        let fuse = Fuse {
            sync: Some(Errno::IO),
            ..Fuse::default()
        };

        let report = copy_on(&fuse, &[src.join("a.txt")], &dcim);

        assert!(matches!(
            only_item(&report)?,
            ItemOutcome::Failed(TransferError::Io(_))
        ));
        assert!(names(&dcim)?.is_empty(), "no file, no partial");
        Ok(())
    }

    #[test]
    fn a_weak_filesystem_without_times_or_modes_still_receives_the_copy() -> TestResult {
        let scratch = Scratch::new("weak-meta")?;
        let (src, dcim) = phone(&scratch)?;
        let tree = scratch.dir("src/album")?;
        fs::write(tree.join("one.jpg"), b"1")?;
        fs::create_dir(tree.join("sub"))?;
        fs::write(tree.join("sub").join("two.jpg"), b"22")?;
        fs::write(src.join("loose.txt"), b"loose")?;
        let fuse = Fuse {
            weak: true,
            set_modified: Some(Errno::NOTSUP),
            set_permissions: Some(Errno::NOSYS),
            ..Fuse::default()
        };

        let report = copy_on(&fuse, &[tree.clone(), src.join("loose.txt")], &dcim);

        assert!(
            report
                .items
                .iter()
                .all(|item| matches!(item.outcome, ItemOutcome::Ok { .. })),
            "{report:?}"
        );
        assert_eq!(fs::read(dcim.join("album/sub/two.jpg"))?, b"22");
        assert_eq!(fs::read(dcim.join("loose.txt"))?, b"loose");
        assert_eq!(names(&dcim)?, ["album", "loose.txt"], "no temp names left");
        assert_eq!(report.notes.len(), 1, "said once: {:?}", report.notes);
        assert!(
            report
                .notes
                .first()
                .is_some_and(|note| note.contains("permissions")),
            "{:?}",
            report.notes
        );
        Ok(())
    }

    #[test]
    fn a_disk_that_cannot_set_times_still_fails_the_copy() -> TestResult {
        let scratch = Scratch::new("strict-meta")?;
        let (src, dcim) = phone(&scratch)?;
        fs::write(src.join("a.txt"), b"hello")?;
        for fuse in [
            Fuse {
                set_modified: Some(Errno::PERM),
                ..Fuse::default()
            },
            Fuse {
                set_permissions: Some(Errno::PERM),
                ..Fuse::default()
            },
        ] {
            let report = copy_on(&fuse, &[src.join("a.txt")], &dcim);

            assert!(matches!(
                only_item(&report)?,
                ItemOutcome::Failed(TransferError::Io(_))
            ));
            assert!(names(&dcim)?.is_empty());
            assert!(report.notes.is_empty());
        }
        Ok(())
    }

    #[test]
    fn a_copy_to_a_weak_filesystem_never_shows_a_partial_under_the_real_name() -> TestResult {
        let scratch = Scratch::new("weak-partial")?;
        let (src, dcim) = phone(&scratch)?;
        fs::write(src.join("movie.mp4"), big(2 * CHUNK + 5))?;
        // The disk fills up on the second chunk.
        let fuse = Fuse {
            weak: true,
            full_from: Some(2),
            ..Fuse::default()
        };

        let report = copy_on(&fuse, &[src.join("movie.mp4")], &dcim);

        match only_item(&report)? {
            ItemOutcome::Failed(TransferError::Io(error)) => {
                assert_eq!(error.raw_os_error(), Some(Errno::NOSPC.raw_os_error()));
            }
            other => return Err(format!("expected ENOSPC, got {other:?}").into()),
        }
        assert!(names(&dcim)?.is_empty(), "neither the real name nor a temp");
        assert!(src.join("movie.mp4").exists());
        Ok(())
    }

    #[test]
    fn without_rename_support_the_copy_goes_straight_to_the_free_final_name() -> TestResult {
        let scratch = Scratch::new("direct")?;
        let (src, dcim) = phone(&scratch)?;
        let data = big(CHUNK + 7);
        fs::write(src.join("a.bin"), &data)?;
        fs::write(src.join("b.bin"), b"second")?;
        let fuse = Fuse {
            weak: true,
            place_rename: Some(Errno::NOTSUP),
            ..Fuse::default()
        };
        let mut done = Vec::new();

        let report = transfer_on(
            &[src.join("a.bin"), src.join("b.bin")],
            &dcim,
            Mode::Copy,
            no_conflict,
            |progress| done.push((progress.bytes_done, progress.bytes_total)),
            &AtomicBool::new(false),
            &fuse,
        );

        assert!(
            report
                .items
                .iter()
                .all(|item| matches!(item.outcome, ItemOutcome::Ok { .. })),
            "{report:?}"
        );
        assert_eq!(fs::read(dcim.join("a.bin"))?, data);
        assert_eq!(fs::read(dcim.join("b.bin"))?, b"second");
        assert_eq!(names(&dcim)?, ["a.bin", "b.bin"], "no temp names left");
        assert_eq!(
            fuse.places.get(),
            1,
            "the second file did not retry the rename"
        );
        assert!(
            done.iter().all(|(done, total)| done <= total),
            "progress never passes the total: {done:?}"
        );
        assert_eq!(report.bytes, data.len() as u64 + 6);
        Ok(())
    }

    #[test]
    fn a_failed_direct_copy_deletes_what_it_wrote() -> TestResult {
        let scratch = Scratch::new("direct-fail")?;
        let (src, dcim) = phone(&scratch)?;
        let tree = scratch.dir("src/album")?;
        fs::write(tree.join("one.jpg"), b"1")?;
        fs::write(tree.join("two.jpg"), b"2")?;
        // Writes 1 and 2 build the partial copy; the third is the direct one.
        let fuse = Fuse {
            weak: true,
            place_rename: Some(Errno::NOTSUP),
            full_from: Some(3),
            ..Fuse::default()
        };

        let report = copy_on(&fuse, &[tree], &dcim);

        assert!(matches!(
            only_item(&report)?,
            ItemOutcome::Failed(TransferError::Io(_))
        ));
        assert!(names(&dcim)?.is_empty(), "the half-written folder is gone");
        assert!(src.join("album/one.jpg").exists(), "the source is intact");
        Ok(())
    }

    #[test]
    fn a_cancelled_direct_copy_deletes_what_it_wrote() -> TestResult {
        let scratch = Scratch::new("direct-cancel")?;
        let (src, dcim) = phone(&scratch)?;
        fs::write(src.join("movie.mp4"), big(5 * CHUNK / 2))?;
        let cancel = Arc::new(AtomicBool::new(false));
        // Three writes build the partial copy; cancel during the direct one.
        let fuse = Fuse {
            weak: true,
            place_rename: Some(Errno::NOTSUP),
            cancel_at: Some((5, Arc::clone(&cancel))),
            ..Fuse::default()
        };

        let report = run_on(&fuse, &[src.join("movie.mp4")], &dcim, Mode::Copy, &cancel);

        assert!(report.cancelled);
        assert!(names(&dcim)?.is_empty());
        assert!(src.join("movie.mp4").exists());
        Ok(())
    }

    #[test]
    fn a_direct_copy_never_touches_a_name_somebody_else_took() -> TestResult {
        let scratch = Scratch::new("direct-race")?;
        let (src, dcim) = phone(&scratch)?;
        fs::write(src.join("a.txt"), b"mine")?;
        let fuse = Fuse {
            weak: true,
            place_rename: Some(Errno::NOTSUP),
            racer: Some(dcim.join("a.txt")),
            ..Fuse::default()
        };

        let report = copy_on(&fuse, &[src.join("a.txt")], &dcim);

        match only_item(&report)? {
            ItemOutcome::Failed(TransferError::Io(error)) => {
                assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            }
            other => return Err(format!("expected AlreadyExists, got {other:?}").into()),
        }
        assert_eq!(
            fs::read(dcim.join("a.txt"))?,
            b"theirs",
            "not overwritten or deleted"
        );
        assert_eq!(names(&dcim)?, ["a.txt"]);
        Ok(())
    }

    #[test]
    fn a_move_onto_a_weak_filesystem_copies_directly_and_then_removes_the_source() -> TestResult {
        struct Moves(Fuse);
        impl Backend for Moves {
            fn move_rename(&self, _: &Path, _: &Path) -> io::Result<()> {
                Err(errno(Errno::XDEV))
            }
            fn place_rename(&self, from: &Path, to: &Path) -> io::Result<()> {
                self.0.place_rename(from, to)
            }
            fn is_weak(&self, dir: &Path) -> bool {
                self.0.is_weak(dir)
            }
        }
        let scratch = Scratch::new("weak-move")?;
        let (src, dcim) = phone(&scratch)?;
        fs::write(src.join("a.txt"), b"hello")?;
        let backend = Moves(Fuse {
            weak: true,
            place_rename: Some(Errno::INVAL),
            ..Fuse::default()
        });

        let report = transfer_on(
            &[src.join("a.txt")],
            &dcim,
            Mode::Move,
            no_conflict,
            |_| {},
            &AtomicBool::new(false),
            &backend,
        );

        assert_eq!(fs::read(only_ok(&report)?)?, b"hello");
        assert!(
            !src.join("a.txt").exists(),
            "the source goes only after the copy"
        );
        assert_eq!(names(&dcim)?, ["a.txt"]);
        Ok(())
    }

    #[test]
    fn deleting_on_a_gvfs_directory_is_a_plain_permanent_delete() -> TestResult {
        let scratch = Scratch::new("weak-delete")?;
        let (_, dcim) = phone(&scratch)?;
        fs::create_dir(dcim.join("Camera"))?;
        fs::write(dcim.join("Camera").join("IMG_1.jpg"), b"x")?;
        fs::write(dcim.join("note.txt"), b"x")?;

        remove_permanently(&dcim.join("note.txt"))?;
        remove_permanently(&dcim.join("Camera"))?;

        assert!(names(&dcim)?.is_empty());
        Ok(())
    }

    // --- T6 probes -----------------------------------------------------------

    fn vp_ino(p: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        fs::symlink_metadata(p).map(|m| m.ino()).unwrap_or(0)
    }
    #[test]
    fn weak_replace_direct_failure_restores_original() -> TestResult {
        let sc = Scratch::new("vp1")?;
        let (src, d) = phone(&sc)?;
        fs::write(src.join("a.bin"), big(2 * CHUNK + 1))?;
        fs::write(d.join("a.bin"), b"ORIG")?;
        let i = vp_ino(&d.join("a.bin"));
        let f = Fuse {
            weak: true,
            place_rename: Some(Errno::NOTSUP),
            full_from: Some(4),
            ..Fuse::default()
        };
        let r = transfer_on(
            &[src.join("a.bin")],
            &d,
            Mode::Copy,
            |_| Conflict::Replace,
            |_| {},
            &AtomicBool::new(false),
            &f,
        );
        assert!(matches!(
            only_item(&r)?,
            ItemOutcome::Failed(TransferError::Io(_))
        ));
        assert_eq!(names(&d)?, ["a.bin"]);
        assert_eq!(fs::read(d.join("a.bin"))?, b"ORIG");
        assert_eq!(vp_ino(&d.join("a.bin")), i);
        Ok(())
    }
    #[test]
    fn strict_dest_refusing_rename_never_goes_direct() -> TestResult {
        let sc = Scratch::new("vp2")?;
        let (src, d) = phone(&sc)?;
        fs::write(src.join("a.txt"), b"x")?;
        let f = Fuse {
            place_rename: Some(Errno::NOTSUP),
            ..Fuse::default()
        };
        let r = copy_on(&f, &[src.join("a.txt")], &d);
        assert!(
            matches!(only_item(&r)?,ItemOutcome::Failed(TransferError::Io(e)) if e.raw_os_error()==Some(Errno::NOTSUP.raw_os_error()))
        );
        assert!(names(&d)?.is_empty());
        assert_eq!(f.places.get(), 1);
        Ok(())
    }
    #[test]
    fn a_folder_somebody_else_made_is_never_removed() -> TestResult {
        struct R(PathBuf);
        impl Backend for R {
            fn place_rename(&self, _: &Path, _: &Path) -> io::Result<()> {
                fs::create_dir(&self.0)?;
                fs::write(self.0.join("theirs.txt"), b"t")?;
                Err(errno(Errno::NOTSUP))
            }
            fn is_weak(&self, _: &Path) -> bool {
                true
            }
        }
        let sc = Scratch::new("vp3")?;
        let (_, d) = phone(&sc)?;
        let a = sc.dir("src/album")?;
        fs::write(a.join("one.jpg"), b"1")?;
        let r = transfer_on(
            &[a],
            &d,
            Mode::Copy,
            no_conflict,
            |_| {},
            &AtomicBool::new(false),
            &R(d.join("album")),
        );
        assert!(matches!(only_item(&r)?, ItemOutcome::Failed(_)));
        assert_eq!(names(&d.join("album"))?, ["theirs.txt"]);
        Ok(())
    }
    #[test]
    fn local_folder_mode_or_time_failure_still_fails() -> TestResult {
        let sc = Scratch::new("vp4")?;
        let (src, d) = phone(&sc)?;
        sc.dir("src/e")?;
        for f in [
            Fuse {
                set_permissions: Some(Errno::PERM),
                ..Fuse::default()
            },
            Fuse {
                set_modified: Some(Errno::PERM),
                ..Fuse::default()
            },
        ] {
            let r = copy_on(&f, &[src.join("e")], &d);
            assert!(matches!(
                only_item(&r)?,
                ItemOutcome::Failed(TransferError::Io(_))
            ));
            assert!(names(&d)?.is_empty() && r.notes.is_empty());
        }
        Ok(())
    }
    #[test]
    fn weak_cancel_in_second_direct_item_keeps_the_first() -> TestResult {
        let sc = Scratch::new("vp5")?;
        let (src, d) = phone(&sc)?;
        fs::write(src.join("a.bin"), b"first")?;
        fs::write(src.join("b.bin"), big(3 * CHUNK))?;
        let c = Arc::new(AtomicBool::new(false));
        let f = Fuse {
            weak: true,
            place_rename: Some(Errno::NOTSUP),
            cancel_at: Some((3, Arc::clone(&c))),
            ..Fuse::default()
        };
        let r = run_on(
            &f,
            &[src.join("a.bin"), src.join("b.bin")],
            &d,
            Mode::Copy,
            &c,
        );
        assert!(r.cancelled);
        assert_eq!(names(&d)?, ["a.bin"]);
        Ok(())
    }

    #[test]
    fn a_file_somebody_else_wrote_into_our_folder_is_never_removed() -> TestResult {
        // The second write is the direct copy's: a third party drops a file next
        // to ours and the disk fills up.
        struct Racer {
            theirs: PathBuf,
            writes: Cell<usize>,
        }
        impl Backend for Racer {
            fn place_rename(&self, _: &Path, _: &Path) -> io::Result<()> {
                Err(errno(Errno::NOTSUP))
            }
            fn write(&self, file: &mut fs::File, data: &[u8]) -> io::Result<()> {
                self.writes.set(self.writes.get() + 1);
                if self.writes.get() == 2 {
                    fs::write(&self.theirs, b"theirs")?;
                    return Err(errno(Errno::NOSPC));
                }
                file.write_all(data)
            }
            fn is_weak(&self, _: &Path) -> bool {
                true
            }
        }
        let sc = Scratch::new("vp-inside")?;
        let (_, d) = phone(&sc)?;
        let album = sc.dir("src/album")?;
        fs::write(album.join("one.jpg"), b"1")?;
        let racer = Racer {
            theirs: d.join("album").join("theirs.txt"),
            writes: Cell::new(0),
        };

        let report = transfer_on(
            &[album],
            &d,
            Mode::Copy,
            no_conflict,
            |_| {},
            &AtomicBool::new(false),
            &racer,
        );

        assert!(matches!(only_item(&report)?, ItemOutcome::Failed(_)));
        assert_eq!(names(&d.join("album"))?, ["theirs.txt"]);
        assert_eq!(fs::read(d.join("album/theirs.txt"))?, b"theirs");
        Ok(())
    }

    #[test]
    fn a_failed_direct_copy_removes_all_of_what_it_made() -> TestResult {
        let sc = Scratch::new("vp-ours")?;
        let (src, d) = phone(&sc)?;
        let album = sc.dir("src/album")?;
        fs::write(album.join("one.jpg"), big(CHUNK))?;
        fs::create_dir(album.join("sub"))?;
        fs::write(album.join("sub/two.jpg"), big(CHUNK))?;
        std::os::unix::fs::symlink("one.jpg", album.join("link"))?;
        let _ = src;
        // The 4th write is the 2nd direct one: everything made so far is ours.
        let f = Fuse {
            weak: true,
            place_rename: Some(Errno::NOTSUP),
            full_from: Some(4),
            ..Fuse::default()
        };

        let report = copy_on(&f, &[album], &d);

        assert!(matches!(only_item(&report)?, ItemOutcome::Failed(_)));
        assert!(names(&d)?.is_empty(), "{:?}", names(&d)?);
        Ok(())
    }

    #[test]
    fn an_entry_whose_stat_fails_right_after_creation_is_still_rolled_back() -> TestResult {
        let sc = Scratch::new("vp-stat-fails")?;
        let (src, d) = phone(&sc)?;
        let album = sc.dir("src/album")?;
        fs::write(album.join("one.jpg"), big(CHUNK))?;
        fs::create_dir(album.join("sub"))?;
        fs::write(album.join("sub/two.jpg"), big(CHUNK))?;
        std::os::unix::fs::symlink("one.jpg", album.join("link"))?;
        let _ = src;
        let f = Fuse {
            weak: true,
            place_rename: Some(Errno::NOTSUP),
            full_from: Some(4),
            stat_fails: true,
            ..Fuse::default()
        };

        let report = copy_on(&f, &[album], &d);

        assert!(matches!(only_item(&report)?, ItemOutcome::Failed(_)));
        assert!(
            names(&d)?.is_empty(),
            "nothing stays behind: {:?}",
            names(&d)?
        );
        Ok(())
    }

    #[test]
    fn a_weak_filesystem_only_skips_metadata_it_says_it_does_not_keep() -> TestResult {
        for unsupported in [Errno::NOTSUP, Errno::NOSYS, Errno::PERM, Errno::ACCESS] {
            for times in [true, false] {
                let sc = Scratch::new("weak-lenient")?;
                let (src, d) = phone(&sc)?;
                fs::write(src.join("a.txt"), b"x")?;
                let f = Fuse {
                    weak: true,
                    set_modified: times.then_some(unsupported),
                    set_permissions: (!times).then_some(unsupported),
                    ..Fuse::default()
                };
                let report = copy_on(&f, &[src.join("a.txt")], &d);
                assert!(
                    matches!(only_item(&report)?, ItemOutcome::Ok { .. }),
                    "{unsupported:?} {times}"
                );
                assert_eq!(report.notes.len(), 1);
            }
        }
        Ok(())
    }

    #[test]
    fn a_weak_filesystem_that_fails_on_metadata_for_real_still_fails() -> TestResult {
        for failure in [Errno::IO, Errno::NOSPC, Errno::ROFS] {
            for times in [true, false] {
                for folder in [false, true] {
                    let sc = Scratch::new("weak-eio")?;
                    let (src, d) = phone(&sc)?;
                    let item = if folder {
                        sc.dir("src/e")?
                    } else {
                        fs::write(src.join("a.txt"), b"x")?;
                        src.join("a.txt")
                    };
                    let f = Fuse {
                        weak: true,
                        set_modified: times.then_some(failure),
                        set_permissions: (!times).then_some(failure),
                        ..Fuse::default()
                    };
                    let report = copy_on(&f, &[item], &d);
                    assert!(
                        matches!(
                            only_item(&report)?,
                            ItemOutcome::Failed(TransferError::Io(_))
                        ),
                        "{failure:?} times={times} folder={folder}"
                    );
                    assert!(names(&d)?.is_empty());
                    assert!(report.notes.is_empty());
                }
            }
        }
        Ok(())
    }

    #[test]
    fn a_disk_whose_fsync_is_unsupported_still_fails_the_copy() -> TestResult {
        for unsupported in [Errno::NOTSUP, Errno::INVAL, Errno::NOSYS] {
            let sc = Scratch::new("strict-fsync")?;
            let (src, d) = phone(&sc)?;
            fs::write(src.join("a.txt"), b"hello")?;
            let f = Fuse {
                sync: Some(unsupported),
                ..Fuse::default()
            };
            let report = copy_on(&f, &[src.join("a.txt")], &d);
            assert!(
                matches!(
                    only_item(&report)?,
                    ItemOutcome::Failed(TransferError::Io(_))
                ),
                "{unsupported:?}"
            );
            assert!(names(&d)?.is_empty());
        }
        Ok(())
    }

    #[test]
    fn a_foreign_file_that_replaced_ours_at_the_same_path_is_kept() -> TestResult {
        // Placing the partial copy is refused, so the copy goes direct;
        // its first write finds the path it created already there: the backend deletes it, puts a file of "theirs" at the same
        // path (ours stays open, so its inode cannot be reused) and fails with
        // a full disk. The rollback must keep the foreign file.
        struct Swapper {
            path: PathBuf,
            writes: Cell<usize>,
        }
        impl Backend for Swapper {
            fn place_rename(&self, _: &Path, _: &Path) -> io::Result<()> {
                Err(errno(Errno::NOTSUP))
            }
            fn write(&self, file: &mut fs::File, data: &[u8]) -> io::Result<()> {
                self.writes.set(self.writes.get() + 1);
                // Only the direct copy writes to the real name.
                if self.path.exists() {
                    // Ours stays open (its inode cannot be reused) while the
                    // path now names another file.
                    fs::remove_file(&self.path)?;
                    fs::write(&self.path, b"theirs")?;
                    return Err(errno(Errno::NOSPC));
                }
                file.write_all(data)
            }
            fn is_weak(&self, _: &Path) -> bool {
                true
            }
        }
        let sc = Scratch::new("vp-swap")?;
        let (_, d) = phone(&sc)?;
        let src = sc.dir("src")?;
        fs::write(src.join("big.bin"), big(CHUNK * 2))?;
        let swapper = Swapper {
            path: d.join("big.bin"),
            writes: Cell::new(0),
        };

        let report = transfer_on(
            &[src.join("big.bin")],
            &d,
            Mode::Copy,
            no_conflict,
            |_| {},
            &AtomicBool::new(false),
            &swapper,
        );

        assert!(matches!(only_item(&report)?, ItemOutcome::Failed(_)));
        assert_eq!(fs::read(d.join("big.bin"))?, b"theirs");
        Ok(())
    }

    #[test]
    fn a_foreign_file_in_a_nested_folder_of_ours_stays() -> TestResult {
        struct NestedRacer {
            theirs: PathBuf,
            writes: Cell<usize>,
        }
        impl Backend for NestedRacer {
            fn place_rename(&self, _: &Path, _: &Path) -> io::Result<()> {
                Err(errno(Errno::NOTSUP))
            }
            fn write(&self, file: &mut fs::File, data: &[u8]) -> io::Result<()> {
                self.writes.set(self.writes.get() + 1);
                if self.writes.get() == 4 {
                    fs::write(&self.theirs, b"theirs")?;
                    return Err(errno(Errno::NOSPC));
                }
                file.write_all(data)
            }
            fn is_weak(&self, _: &Path) -> bool {
                true
            }
        }
        let sc = Scratch::new("vp-nested")?;
        let (_, d) = phone(&sc)?;
        let album = sc.dir("src/album")?;
        fs::create_dir(album.join("sub"))?;
        fs::write(album.join("sub/one.jpg"), b"1")?;
        fs::write(album.join("sub/two.jpg"), b"2")?;
        fs::write(album.join("sub/three.jpg"), b"3")?;
        let racer = NestedRacer {
            theirs: d.join("album/sub/theirs.txt"),
            writes: Cell::new(0),
        };

        let report = transfer_on(
            &[album],
            &d,
            Mode::Copy,
            no_conflict,
            |_| {},
            &AtomicBool::new(false),
            &racer,
        );

        assert!(matches!(only_item(&report)?, ItemOutcome::Failed(_)));
        assert_eq!(names(&d.join("album/sub"))?, ["theirs.txt"]);
        assert_eq!(fs::read(d.join("album/sub/theirs.txt"))?, b"theirs");
        Ok(())
    }

    #[test]
    fn a_weak_direct_copy_tolerates_an_unsupported_fsync() -> TestResult {
        for unsupported in [Errno::NOTSUP, Errno::INVAL, Errno::NOSYS] {
            let sc = Scratch::new("direct-fsync")?;
            let (src, d) = phone(&sc)?;
            fs::write(src.join("a.txt"), b"hello")?;
            let f = Fuse {
                weak: true,
                place_rename: Some(Errno::NOTSUP),
                sync: Some(unsupported),
                ..Fuse::default()
            };

            let report = copy_on(&f, &[src.join("a.txt")], &d);

            let dest = only_ok(&report)?;
            assert_eq!(fs::read(dest)?, b"hello", "{unsupported:?}");
            assert!(report.notes.is_empty());
        }
        Ok(())
    }

    #[test]
    fn einval_on_metadata_is_tolerated_on_a_weak_filesystem_only() -> TestResult {
        for times in [true, false] {
            for folder in [false, true] {
                let sc = Scratch::new("weak-einval")?;
                let (src, d) = phone(&sc)?;
                let item = if folder {
                    sc.dir("src/e")?
                } else {
                    fs::write(src.join("a.txt"), b"x")?;
                    src.join("a.txt")
                };
                let mk = |weak| Fuse {
                    weak,
                    set_modified: times.then_some(Errno::INVAL),
                    set_permissions: (!times).then_some(Errno::INVAL),
                    ..Fuse::default()
                };
                let report = copy_on(&mk(true), std::slice::from_ref(&item), &d);
                assert!(
                    matches!(only_item(&report)?, ItemOutcome::Ok { .. }),
                    "times={times} folder={folder}"
                );
                assert_eq!(report.notes.len(), 1);

                let strict = sc.dir("strict")?;
                let report = copy_on(&mk(false), &[item], &strict);
                assert!(
                    matches!(
                        only_item(&report)?,
                        ItemOutcome::Failed(TransferError::Io(_))
                    ),
                    "local fs times={times} folder={folder}"
                );
                assert!(names(&strict)?.is_empty());
            }
        }
        Ok(())
    }
}
