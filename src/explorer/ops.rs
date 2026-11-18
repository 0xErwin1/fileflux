//! The file-operation engine and the pure models the UI is built on.
//!
//! [`run_job`] performs one operation off the UI thread against the core
//! (`fileflux::core::ops` and `fileflux::core::trash`) and answers a
//! [`Report`]: the toast to show, the undo entry to record, what to select
//! afterwards and the paths that need a confirmation. It reports through a
//! [`Sink`] and polls its cancel flag between items.
//!
//! Safety defaults live here and nowhere else: deleting always means the
//! freedesktop Trash first; [`Job::DeletePermanently`] is only ever built
//! from a confirmed dialog; an undo re-checks the world before it acts and
//! says "Can't undo: …" instead of touching something that changed; nothing
//! replaces an existing entry.
//!
//! Also here, as plain data the render code only draws: the undo stack, the
//! rename checks, the context-menu model, the confirmation texts and the
//! terminal lookup.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::ops::Range;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use fileflux::core::ops::{self as core_ops, validate_name};
use fileflux::core::trash::{
    self, RestoreConflict, TrashError, TrashTarget, TrashedItem, trash_dir_for,
};
use fileflux::core::watch::nearest_existing_ancestor;

use super::progress::Sink;

/// How many undoable operations a session remembers.
pub(crate) const UNDO_LIMIT: usize = 50;

/// What "New …" creates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NewKind {
    Folder,
    File,
}

impl NewKind {
    fn base_name(self) -> &'static str {
        match self {
            NewKind::Folder => "Untitled folder",
            NewKind::File => "Untitled file",
        }
    }

    fn noun(self) -> &'static str {
        match self {
            NewKind::Folder => "folder",
            NewKind::File => "file",
        }
    }
}

/// Which file a path named when an operation finished: device and inode.
/// An undo acts only when the item at the recorded location is still this
/// one; a different file under the same name is someone else's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
}

impl Identity {
    fn from_meta(meta: &fs::Metadata) -> Identity {
        Identity {
            dev: meta.dev(),
            ino: meta.ino(),
        }
    }

    /// The identity of what `path` names now; a symlink is itself, never
    /// its target.
    pub(crate) fn of(path: &Path) -> io::Result<Identity> {
        fs::symlink_metadata(path).map(|meta| Identity::from_meta(&meta))
    }
}

/// One item a Trash job moved, with what its undo must re-check: which
/// Trash it went to (to re-read its `.trashinfo`) and which file it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrashedEntry {
    pub(crate) item: TrashedItem,
    pub(crate) target: TrashTarget,
    pub(crate) identity: Identity,
}

/// One reversible operation, with what undoing it needs to know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UndoAction {
    /// Undo: rename `new` back to `old`'s name, if `new` is still the
    /// item that was renamed.
    Rename {
        old: PathBuf,
        new: PathBuf,
        identity: Identity,
    },
    /// Undo: restore the items to where they came from, each only if it is
    /// still the item that was trashed.
    Trash { items: Vec<TrashedEntry> },
    /// Undo: move the (still empty, still the same) new item to the Trash.
    Create {
        path: PathBuf,
        kind: NewKind,
        identity: Identity,
    },
}

/// An undo action with the id a toast's Undo button names it by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UndoEntry {
    pub(crate) id: u64,
    pub(crate) action: UndoAction,
}

/// The session's undo history: newest last, bounded, surviving navigation.
#[derive(Debug, Default)]
pub(crate) struct UndoStack {
    entries: Vec<UndoEntry>,
    last_id: u64,
}

impl UndoStack {
    /// Records an action and returns its id.
    pub(crate) fn push(&mut self, action: UndoAction) -> u64 {
        self.last_id += 1;
        if self.entries.len() >= UNDO_LIMIT {
            self.entries.remove(0);
        }
        self.entries.push(UndoEntry {
            id: self.last_id,
            action,
        });
        self.last_id
    }

    /// Removes and returns the newest entry (Ctrl+Z).
    pub(crate) fn pop(&mut self) -> Option<UndoEntry> {
        self.entries.pop()
    }

    /// Removes and returns one entry by id (a toast's Undo button).
    pub(crate) fn take(&mut self, id: u64) -> Option<UndoEntry> {
        let index = self.entries.iter().position(|entry| entry.id == id)?;
        Some(self.entries.remove(index))
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// How loud a result is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Severity {
    Success,
    Info,
    Warning,
    Error,
}

/// What the user is told about a finished operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Notice {
    pub(crate) severity: Severity,
    pub(crate) text: String,
}

/// The outcome of one [`Job`].
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// The summary toast; `None` when the operation has nothing to say
    /// (a plain rename) or a dialog speaks next.
    pub(crate) notice: Option<Notice>,
    /// The undo entry to record (its toast carries the Undo button).
    pub(crate) undo: Option<UndoAction>,
    /// Entries to select once the listing shows them.
    pub(crate) select: Vec<PathBuf>,
    /// Start the inline rename on the selected entry (New folder/file).
    pub(crate) begin_rename: bool,
    /// Paths that could not go to the Trash: ask before deleting them.
    pub(crate) needs_permanent: Vec<PathBuf>,
    /// Per-item failures, for the toast's "Details".
    pub(crate) details: Vec<String>,
    /// The user cancelled; what was done before stays done.
    pub(crate) cancelled: bool,
}

impl Report {
    fn say(severity: Severity, text: impl Into<String>) -> Report {
        Report {
            notice: Some(Notice {
                severity,
                text: text.into(),
            }),
            ..Report::default()
        }
    }

    fn error(text: impl Into<String>) -> Report {
        Report::say(Severity::Error, text)
    }
}

/// One operation for [`run_job`]. Everything is owned: it moves to the
/// background executor.
#[derive(Debug)]
pub(crate) enum Job {
    Create {
        dir: PathBuf,
        kind: NewKind,
    },
    Rename {
        path: PathBuf,
        new_name: OsString,
    },
    Trash(Vec<PathBuf>),
    /// Only ever built from a confirmed dialog.
    DeletePermanently(Vec<PathBuf>),
    /// Trash view: names under the Trash's `files/`.
    Restore(Vec<OsString>),
    /// Trash view, confirmed.
    DeleteFromTrash(Vec<OsString>),
    /// Trash view, confirmed: exactly the names the confirmation counted.
    /// Items trashed after the dialog opened are never part of it.
    EmptyTrash(Vec<OsString>),
    Undo(UndoAction),
}

/// Chooses the Trash a path goes to.
type Targeter = Box<dyn Fn(&Path) -> Result<TrashTarget, TrashError> + Send + Sync>;

/// What a job needs to know about the machine: where the home Trash is and
/// which Trash a given path goes to.
pub(crate) struct JobCtx {
    /// The home Trash (what the Trash view shows).
    pub(crate) trash_root: PathBuf,
    pub(crate) target_for: Targeter,
}

impl std::fmt::Debug for JobCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobCtx")
            .field("trash_root", &self.trash_root)
            .finish_non_exhaustive()
    }
}

impl JobCtx {
    /// The real chooser: the home Trash on the home device, else
    /// `$topdir/.Trash-$uid` of the mount (from `mounts`) that holds the
    /// path; anything else is `NoTrash`.
    pub(crate) fn from_env(trash_root: PathBuf, mounts: Vec<PathBuf>) -> JobCtx {
        let root = trash_root.clone();
        JobCtx {
            trash_root,
            target_for: Box::new(move |path| choose_target(path, &root, &mounts)),
        }
    }
}

fn choose_target(
    path: &Path,
    trash_root: &Path,
    mounts: &[PathBuf],
) -> Result<TrashTarget, TrashError> {
    // std has no getuid; the owner of our own /proc entry is us.
    let uid = fs::metadata("/proc/self")?.uid();
    let anchor = nearest_existing_ancestor(trash_root).ok_or_else(|| {
        TrashError::NoTrash(format!("{} has no existing parent", trash_root.display()))
    })?;
    let home_dev = fs::metadata(&anchor)?.dev();
    trash_dir_for(path, trash_root, home_dev, uid, |candidate| {
        topdir_of(mounts, candidate)
    })
}

/// The mount point that holds `path`: the longest one that is a
/// component-wise prefix of it.
pub(crate) fn topdir_of(mounts: &[PathBuf], path: &Path) -> Option<PathBuf> {
    mounts
        .iter()
        .filter(|mount| path.starts_with(mount))
        .max_by_key(|mount| mount.components().count())
        .cloned()
}

fn quote(name: &OsStr) -> String {
    format!("\u{201c}{}\u{201d}", name.to_string_lossy())
}

fn quote_path(path: &Path) -> String {
    quote(path.file_name().unwrap_or(path.as_os_str()))
}

fn plural_items(count: usize) -> String {
    format!("{count} items")
}

/// Performs one job. Blocking: call it on the background executor.
pub(crate) fn run_job(job: Job, ctx: &JobCtx, sink: &Sink) -> Report {
    match job {
        Job::Create { dir, kind } => create(&dir, kind),
        Job::Rename { path, new_name } => rename(&path, &new_name),
        Job::Trash(paths) => trash_items(&paths, ctx, sink),
        Job::DeletePermanently(paths) => delete_items(&paths, sink),
        Job::Restore(names) => restore_items(&names, ctx, sink),
        Job::DeleteFromTrash(names) => delete_from_trash(&names, ctx, sink),
        Job::EmptyTrash(names) => empty_trash(ctx, names, sink),
        Job::Undo(action) => match undo(&action, ctx, sink) {
            Ok(report) => report,
            Err(reason) => Report::error(format!("Can't undo: {reason}")),
        },
    }
}

fn create(dir: &Path, kind: NewKind) -> Report {
    // A name can be taken between choosing it and creating it; the next
    // unique name is then free again.
    for _ in 0..32 {
        let name = core_ops::unique_name(dir, OsStr::new(kind.base_name()));
        let made = match kind {
            NewKind::Folder => core_ops::create_dir(dir, &name),
            NewKind::File => core_ops::create_file(dir, &name),
        };
        match made {
            Ok(path) => {
                // Without an identity there is nothing safe to undo.
                let undo = Identity::of(&path).ok().map(|identity| UndoAction::Create {
                    path: path.clone(),
                    kind,
                    identity,
                });
                return Report {
                    select: vec![path],
                    begin_rename: true,
                    undo,
                    ..Report::default()
                };
            }
            Err(error) if error.is_already_exists() => {}
            Err(error) => {
                return Report::error(format!("Could not create the {}: {error}", kind.noun()));
            }
        }
    }
    Report::error(format!(
        "Could not find a free name for the new {}",
        kind.noun()
    ))
}

fn rename(path: &Path, new_name: &OsStr) -> Report {
    match core_ops::rename(path, new_name) {
        Ok(new) if new == path => Report::default(),
        Ok(new) => Report {
            undo: Identity::of(&new).ok().map(|identity| UndoAction::Rename {
                old: path.to_path_buf(),
                new: new.clone(),
                identity,
            }),
            notice: has_padding(new_name).then(|| Notice {
                severity: Severity::Info,
                text: format!(
                    "Renamed to {}; its leading or trailing spaces are kept",
                    quote(new_name)
                ),
            }),
            select: vec![new],
            ..Report::default()
        },
        Err(error) if error.is_already_exists() => {
            Report::error(format!("{} already exists here", quote(new_name)))
        }
        Err(error) => Report::error(format!("Could not rename {}: {error}", quote_path(path))),
    }
}

/// Whether a name starts or ends with whitespace (kept, but worth saying).
fn has_padding(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| name != name.trim())
}

/// The "N could not be …" tail and details shared by the item loops.
fn failure_line(path: &Path, error: impl std::fmt::Display) -> String {
    format!("{}: {error}", quote_path(path))
}

fn trash_items(paths: &[PathBuf], ctx: &JobCtx, sink: &Sink) -> Report {
    sink.set_total(paths.len() as u64);
    let now = chrono::Local::now().naive_local();
    let mut items: Vec<TrashedEntry> = Vec::new();
    let mut needs_permanent = Vec::new();
    let mut details = Vec::new();
    let mut cancelled = false;
    for (index, path) in paths.iter().enumerate() {
        if sink.cancelled() {
            cancelled = true;
            break;
        }
        sink.advance(
            index as u64,
            &path.file_name().map_or_else(
                || path.to_string_lossy().into_owned(),
                |name| name.to_string_lossy().into_owned(),
            ),
        );
        let trashed = (ctx.target_for)(path).and_then(|target| {
            // Read before the move: if the path is swapped in between, the
            // identity is the old one and an undo refuses (the safe side).
            let identity = Identity::of(path)?;
            let item = trash::put(path, &target, now)?;
            Ok(TrashedEntry {
                item,
                target,
                identity,
            })
        });
        match trashed {
            Ok(entry) => items.push(entry),
            Err(TrashError::NoTrash(_)) => needs_permanent.push(path.clone()),
            Err(error) => details.push(failure_line(path, error)),
        }
    }
    sink.advance(paths.len() as u64, "");

    let moved = items.len();
    let notice = if moved > 0 {
        let first = items
            .first()
            .and_then(|entry| entry.item.original.file_name())
            .map(quote)
            .unwrap_or_default();
        let mut text = if moved == 1 {
            format!("Moved {first} to Trash")
        } else {
            format!("Moved {} to Trash", plural_items(moved))
        };
        if cancelled {
            text = format!("Cancelled: {}", lowercase_first(&text));
        }
        let severity = if details.is_empty() {
            if cancelled {
                Severity::Info
            } else {
                Severity::Success
            }
        } else {
            text.push_str(&format!("; {} could not be moved", details.len()));
            Severity::Warning
        };
        Some(Notice { severity, text })
    } else if cancelled {
        Some(Notice {
            severity: Severity::Info,
            text: "Cancelled: nothing was moved to the Trash".to_owned(),
        })
    } else if let [only] = details.as_slice() {
        Some(Notice {
            severity: Severity::Error,
            text: format!("Could not move to Trash \u{2014} {only}"),
        })
    } else if !details.is_empty() {
        Some(Notice {
            severity: Severity::Error,
            text: format!("Could not move {} to Trash", plural_items(details.len())),
        })
    } else {
        None
    };
    // A cancelled job never asks about permanent deletion: what needed it
    // is said in the summary instead of silently dropped.
    let mut notice = notice;
    if cancelled && !needs_permanent.is_empty() {
        let mut shown = notice.take().unwrap_or(Notice {
            severity: Severity::Info,
            text: "Cancelled".to_owned(),
        });
        shown
            .text
            .push_str(&format!("; {}", not_deleted(needs_permanent.len())));
        if matches!(shown.severity, Severity::Success | Severity::Info) {
            shown.severity = Severity::Warning;
        }
        details.extend(
            needs_permanent
                .iter()
                .map(|path| failure_line(path, "not deleted (no Trash available)")),
        );
        notice = Some(shown);
    }
    Report {
        notice,
        undo: (!items.is_empty()).then_some(UndoAction::Trash { items }),
        needs_permanent,
        details,
        cancelled,
        ..Report::default()
    }
}

/// "1 item" / "3 items".
fn items_label(count: usize) -> String {
    if count == 1 {
        "1 item".to_owned()
    } else {
        plural_items(count)
    }
}

/// What is said about items that were neither moved to the Trash nor
/// deleted because there is no Trash for them.
fn not_deleted(count: usize) -> String {
    format!("{} not deleted (no Trash available)", items_label(count))
}

fn lowercase_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn delete_items(paths: &[PathBuf], sink: &Sink) -> Report {
    sink.set_total(paths.len() as u64);
    let mut deleted: Vec<&PathBuf> = Vec::new();
    let mut details = Vec::new();
    let mut cancelled = false;
    for (index, path) in paths.iter().enumerate() {
        if sink.cancelled() {
            cancelled = true;
            break;
        }
        sink.advance(index as u64, &quote_path(path));
        match core_ops::remove_permanently(path) {
            Ok(()) => deleted.push(path),
            Err(error) => details.push(failure_line(path, error)),
        }
    }
    sink.advance(paths.len() as u64, "");
    let count = deleted.len();
    let first = deleted
        .first()
        .map(|path| quote_path(path))
        .unwrap_or_default();
    let done = if count == 1 {
        format!("Deleted {first}")
    } else {
        format!("Deleted {}", plural_items(count))
    };
    summarize(count, done, "delete", cancelled, details)
}

/// The shared summary of an item loop: what was done, what failed, and
/// whether the user stopped it.
fn summarize(
    count: usize,
    done: String,
    verb: &str,
    cancelled: bool,
    details: Vec<String>,
) -> Report {
    let notice = if count > 0 {
        let mut text = if cancelled {
            format!("Cancelled: {}", lowercase_first(&done))
        } else {
            done
        };
        let severity = if details.is_empty() {
            if cancelled {
                Severity::Info
            } else {
                Severity::Success
            }
        } else {
            text.push_str(&format!("; {} could not be {verb}d", details.len()));
            Severity::Warning
        };
        Notice { severity, text }
    } else if cancelled {
        Notice {
            severity: Severity::Info,
            text: format!("Cancelled: nothing was {verb}d"),
        }
    } else if let [only] = details.as_slice() {
        Notice {
            severity: Severity::Error,
            text: format!("Could not {verb} \u{2014} {only}"),
        }
    } else {
        Notice {
            severity: Severity::Error,
            text: format!("Could not {verb} {}", plural_items(details.len())),
        }
    };
    Report {
        notice: Some(notice),
        details,
        cancelled,
        ..Report::default()
    }
}

/// An item of the home Trash by its name under `files/`. A missing or
/// unreadable `.trashinfo` still lets the item be deleted, never restored.
fn load_item(ctx: &JobCtx, name: &OsStr) -> Result<TrashedItem, TrashError> {
    TrashedItem::load(&TrashTarget::home(ctx.trash_root.clone()), name)
}

fn restore_items(names: &[OsString], ctx: &JobCtx, sink: &Sink) -> Report {
    sink.set_total(names.len() as u64);
    let mut restored: Vec<(TrashedItem, PathBuf)> = Vec::new();
    let mut details = Vec::new();
    let mut cancelled = false;
    for (index, name) in names.iter().enumerate() {
        if sink.cancelled() {
            cancelled = true;
            break;
        }
        sink.advance(index as u64, &name.to_string_lossy());
        let result = load_item(ctx, name).and_then(|item| {
            trash::restore(&item, RestoreConflict::KeepBoth).map(|dest| (item, dest))
        });
        match result {
            Ok(pair) => restored.push(pair),
            Err(error) => details.push(format!("{}: {error}", quote(name))),
        }
    }
    sink.advance(names.len() as u64, "");
    let renamed = restored
        .iter()
        .filter(|(item, dest)| *dest != item.original)
        .count();
    let done = match restored.as_slice() {
        [(item, dest)] => {
            let original = item.original.file_name().map(quote).unwrap_or_default();
            if *dest == item.original {
                format!("Restored {original}")
            } else {
                let now = dest.file_name().map(quote).unwrap_or_default();
                format!("Restored {original} as {now}")
            }
        }
        many => {
            let mut text = format!("Restored {}", plural_items(many.len()));
            if renamed > 0 {
                text.push_str(&format!("; {renamed} renamed to keep both"));
            }
            text
        }
    };
    let mut report = summarize(restored.len(), done, "restore", cancelled, details);
    report.select = restored.into_iter().map(|(_, dest)| dest).collect();
    report
}

fn delete_from_trash(names: &[OsString], ctx: &JobCtx, sink: &Sink) -> Report {
    sink.set_total(names.len() as u64);
    let mut deleted: Vec<String> = Vec::new();
    let mut details = Vec::new();
    let mut cancelled = false;
    for (index, name) in names.iter().enumerate() {
        if sink.cancelled() {
            cancelled = true;
            break;
        }
        sink.advance(index as u64, &name.to_string_lossy());
        let item = load_item(ctx, name).unwrap_or_else(|_| TrashedItem {
            root: ctx.trash_root.clone(),
            name: name.clone(),
            original: PathBuf::from(name),
        });
        let shown = item.original.file_name().map_or_else(|| quote(name), quote);
        match trash::delete_permanently(&item) {
            Ok(()) => deleted.push(shown),
            Err(error) => details.push(format!("{shown}: {error}")),
        }
    }
    sink.advance(names.len() as u64, "");
    let done = match deleted.as_slice() {
        [only] => format!("Deleted {only} from the Trash"),
        many => format!("Deleted {} from the Trash", plural_items(many.len())),
    };
    summarize(deleted.len(), done, "delete", cancelled, details)
}

/// Deletes exactly the confirmed `names`. There is deliberately no
/// root-wide sweep: other Trash jobs may be running, and an info file
/// without content can be a `put` that has not moved its file yet.
fn empty_trash(ctx: &JobCtx, names: Vec<OsString>, sink: &Sink) -> Report {
    if names.is_empty() {
        return Report::say(Severity::Info, "The Trash is already empty");
    }
    sink.set_total(names.len() as u64);
    let mut removed = 0usize;
    let mut details = Vec::new();
    let mut cancelled = false;
    for (index, name) in names.iter().enumerate() {
        if sink.cancelled() {
            cancelled = true;
            break;
        }
        sink.advance(index as u64, &name.to_string_lossy());
        let item = TrashedItem {
            root: ctx.trash_root.clone(),
            name: name.clone(),
            original: PathBuf::from(name),
        };
        match trash::delete_permanently(&item) {
            Ok(()) => removed += 1,
            Err(error) => details.push(format!("{}: {error}", quote(name))),
        }
    }
    sink.advance(names.len() as u64, "");
    if cancelled {
        let text = if removed == 0 {
            "Cancelled: the Trash was not emptied".to_owned()
        } else {
            format!(
                "Cancelled: deleted {} from the Trash",
                plural_items(removed)
            )
        };
        return Report {
            notice: Some(Notice {
                severity: Severity::Info,
                text,
            }),
            details,
            cancelled,
            ..Report::default()
        };
    }
    let (severity, text) = if details.is_empty() {
        (Severity::Success, "Emptied the Trash".to_owned())
    } else {
        (
            Severity::Warning,
            format!("Emptied the Trash; {} could not be removed", details.len()),
        )
    };
    Report {
        notice: Some(Notice { severity, text }),
        details,
        ..Report::default()
    }
}

/// Reverses one action, but only if the world is still as it left it.
/// Anything that changed is an `Err` with the reason, and nothing is
/// touched. A Trash undo is item by item and answers its own summary.
fn undo(action: &UndoAction, ctx: &JobCtx, sink: &Sink) -> Result<Report, String> {
    match action {
        UndoAction::Rename { old, new, identity } => undo_rename(old, new, *identity),
        UndoAction::Trash { items } => Ok(undo_trash(items, sink)),
        UndoAction::Create {
            path,
            kind,
            identity,
        } => undo_create(path, *kind, *identity, ctx),
    }
}

fn presence(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", quote_path(path))),
    }
}

/// What `path` is now, if it is still the item with `expected` identity.
/// `verb` completes "changed since it was …".
fn same_item(path: &Path, expected: Identity, verb: &str) -> Result<fs::Metadata, String> {
    let meta = presence(path)?.ok_or_else(|| format!("{} no longer exists", quote_path(path)))?;
    if Identity::from_meta(&meta) != expected {
        return Err(format!("{} changed since it was {verb}", quote_path(path)));
    }
    Ok(meta)
}

fn undo_rename(old: &Path, new: &Path, identity: Identity) -> Result<Report, String> {
    same_item(new, identity, "renamed")?;
    let old_name = old
        .file_name()
        .ok_or_else(|| "the original name is unknown".to_owned())?;
    match core_ops::rename(new, old_name) {
        Ok(path) => {
            let mut report = Report::say(
                Severity::Success,
                format!("Renamed {} back to {}", quote_path(new), quote(old_name)),
            );
            report.select = vec![path];
            Ok(report)
        }
        Err(error) if error.is_already_exists() => {
            Err(format!("{} already exists", quote(old_name)))
        }
        Err(error) => Err(format!("{}: {error}", quote_path(new))),
    }
}

/// What happened to one item of a Trash undo.
enum Step {
    Restored(PathBuf),
    /// It can never be restored (gone, or no longer the same item): skipped
    /// for good.
    Gone(String),
    /// Its original path is taken: skipped, retry after freeing the name.
    Blocked(String),
    /// An I/O error: nothing changed, retry later.
    Failed(String),
}

fn restore_one(entry: &TrashedEntry) -> Step {
    let original = entry
        .item
        .original
        .file_name()
        .map(quote)
        .unwrap_or_else(|| quote(&entry.item.name));
    let content = match presence(&entry.item.files_path()) {
        Ok(Some(meta)) => meta,
        Ok(None) => return Step::Gone(format!("{original} is no longer in the Trash")),
        Err(reason) => return Step::Failed(reason),
    };
    let changed = || Step::Gone(format!("{original} changed since it was trashed"));
    if Identity::from_meta(&content) != entry.identity {
        return changed();
    }
    // The `.trashinfo` is re-read: if it now names another origin, or is
    // gone, this is no longer the entry we trashed.
    match TrashedItem::load(&entry.target, &entry.item.name) {
        Ok(current) if current.original == entry.item.original => {}
        Err(TrashError::Io(error)) if error.kind() != io::ErrorKind::NotFound => {
            return Step::Failed(format!("{original}: {error}"));
        }
        Ok(_) | Err(_) => return changed(),
    }
    // `Fail`: if the original path was taken since, nothing is replaced
    // and nothing is renamed behind the user's back.
    match trash::restore(&entry.item, RestoreConflict::Fail) {
        Ok(dest) => Step::Restored(dest),
        Err(error) if error.is_already_exists() => {
            Step::Blocked(format!("{original} already exists"))
        }
        Err(error) => Step::Failed(format!("{original}: {error}")),
    }
}

fn undo_trash(entries: &[TrashedEntry], sink: &Sink) -> Report {
    restore_entries(entries, sink, |_| sink.cancelled())
}

/// Restores item by item: a problem with one never stops the others, and
/// `stop(index)` (the cancel flag) is asked before each. What was not
/// restored and still can be stays undoable through the report's undo.
fn restore_entries(
    entries: &[TrashedEntry],
    sink: &Sink,
    mut stop: impl FnMut(usize) -> bool,
) -> Report {
    sink.set_total(entries.len() as u64);
    let mut restored: Vec<PathBuf> = Vec::new();
    let mut remaining: Vec<TrashedEntry> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let (mut skipped, mut failed) = (0usize, 0usize);
    let mut cancelled = false;
    for (index, entry) in entries.iter().enumerate() {
        if stop(index) {
            cancelled = true;
            let rest = entries.get(index..).unwrap_or_default();
            skipped += rest.len();
            remaining.extend(rest.iter().cloned());
            break;
        }
        sink.advance(
            index as u64,
            &entry.item.original.file_name().map_or_else(
                || entry.item.name.to_string_lossy().into_owned(),
                |name| name.to_string_lossy().into_owned(),
            ),
        );
        match restore_one(entry) {
            Step::Restored(dest) => restored.push(dest),
            Step::Gone(reason) => {
                skipped += 1;
                problems.push(reason);
            }
            Step::Blocked(reason) => {
                skipped += 1;
                problems.push(reason);
                remaining.push(entry.clone());
            }
            Step::Failed(reason) => {
                failed += 1;
                problems.push(reason);
                remaining.push(entry.clone());
            }
        }
    }
    sink.advance(entries.len() as u64, "");

    let mut report = match restored.as_slice() {
        [] if cancelled => Report::say(
            if problems.is_empty() {
                Severity::Info
            } else {
                Severity::Warning
            },
            "Cancelled: nothing was restored",
        ),
        [] => Report::error(format!("Can't undo: {}", problems.join("; "))),
        some => {
            let done = match some {
                [only] => format!("Restored {}", quote_path(only)),
                many => format!("Restored {}", plural_items(many.len())),
            };
            let mut text = if cancelled {
                format!("Cancelled: {}", lowercase_first(&done))
            } else {
                done
            };
            if skipped > 0 {
                text.push_str(&format!("; {skipped} skipped"));
            }
            if failed > 0 {
                text.push_str(&format!("; {failed} failed"));
            }
            let severity = match (problems.is_empty(), cancelled) {
                (false, _) => Severity::Warning,
                (true, true) => Severity::Info,
                (true, false) => Severity::Success,
            };
            Report::say(severity, text)
        }
    };
    if !restored.is_empty() || cancelled {
        report.details = problems;
    }
    report.select = restored;
    report.cancelled = cancelled;
    report.undo = (!remaining.is_empty()).then_some(UndoAction::Trash { items: remaining });
    report
}

fn undo_create(
    path: &Path,
    kind: NewKind,
    identity: Identity,
    ctx: &JobCtx,
) -> Result<Report, String> {
    let meta = same_item(path, identity, "created")?;
    let untouched = match kind {
        NewKind::Folder => {
            meta.is_dir()
                && fs::read_dir(path)
                    .map_err(|error| format!("{}: {error}", quote_path(path)))?
                    .next()
                    .is_none()
        }
        NewKind::File => meta.is_file() && meta.len() == 0,
    };
    if !untouched {
        return Err(format!("{} is no longer empty", quote_path(path)));
    }
    // To the Trash, never a permanent delete; without a Trash it stays.
    let target = (ctx.target_for)(path).map_err(|error| match error {
        TrashError::NoTrash(_) => format!("there is no Trash for {}", quote_path(path)),
        other => format!("{}: {other}", quote_path(path)),
    })?;
    trash::put(path, &target, chrono::Local::now().naive_local())
        .map_err(|error| format!("{}: {error}", quote_path(path)))?;
    Ok(Report::say(
        Severity::Success,
        format!("Moved {} to Trash", quote_path(path)),
    ))
}

// --- inline rename ----------------------------------------------------------------

/// The part of `name` the inline rename preselects: the stem of a file (up
/// to its last dot, a leading dot is no extension), the whole name of a
/// folder. Byte offsets.
pub(crate) fn stem_range(name: &str, is_dir: bool) -> Range<usize> {
    if is_dir {
        return 0..name.len();
    }
    match name.rfind('.') {
        Some(dot) if dot > 0 => 0..dot,
        _ => 0..name.len(),
    }
}

/// What committing the typed name would do.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RenameCheck {
    /// The name did not change: close the editor, touch nothing.
    Unchanged,
    /// Rename to this name.
    Rename(OsString),
    /// Keep the editor open and show this under the field.
    Invalid(String),
}

/// Checks a typed name against the rules and the other names of the folder,
/// before anything is renamed.
pub(crate) fn check_rename<'a>(
    old: &OsStr,
    typed: &str,
    mut siblings: impl Iterator<Item = &'a OsStr>,
) -> RenameCheck {
    // The name is judged exactly as typed: Linux allows leading and
    // trailing spaces, and silently trimming would rename to a name the
    // user did not write (and make "unchanged" depend on what was hidden).
    if typed.is_empty() {
        return RenameCheck::Invalid("The name can't be empty.".to_owned());
    }
    if typed.trim().is_empty() {
        return RenameCheck::Invalid("The name can't be blank.".to_owned());
    }
    let name = OsStr::new(typed);
    if name == old {
        return RenameCheck::Unchanged;
    }
    if let Err(error) = validate_name(name) {
        let message = error.to_string();
        let mut chars = message.chars();
        let capitalized = match chars.next() {
            Some(first) => first.to_uppercase().chain(chars).collect(),
            None => message,
        };
        return RenameCheck::Invalid(capitalized);
    }
    if siblings.any(|sibling| sibling == name) {
        return RenameCheck::Invalid(format!("{} already exists here", quote(name)));
    }
    RenameCheck::Rename(name.to_owned())
}

/// What the inline editor does with a [`RenameCheck`]. Blur is a commit
/// that must never leave an editor nobody is typing in: a name it cannot
/// accept cancels the rename (disk untouched) with a message.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RenameStep {
    /// Close the editor, rename nothing.
    Close,
    /// Close the editor, rename nothing, and say why.
    Cancel(String),
    /// Keep the editor open with this under the field.
    KeepOpen(String),
    /// Close the editor and rename to this name.
    Run(OsString),
}

pub(crate) fn rename_step(check: RenameCheck, from_blur: bool) -> RenameStep {
    match check {
        RenameCheck::Unchanged => RenameStep::Close,
        RenameCheck::Invalid(message) if from_blur => {
            RenameStep::Cancel(format!("Rename canceled: {message}"))
        }
        RenameCheck::Invalid(message) => RenameStep::KeepOpen(message),
        RenameCheck::Rename(name) => RenameStep::Run(name),
    }
}

// --- context menu model -------------------------------------------------------------

/// What a menu row does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuCommand {
    Open,
    Rename,
    CopyPath,
    Properties,
    MoveToTrash,
    DeletePermanently,
    NewFolder,
    NewFile,
    OpenTerminal,
    CopyFolderPath,
    Restore,
    EmptyTrash,
}

/// One drawn menu item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MenuItemSpec {
    pub(crate) command: MenuCommand,
    pub(crate) label: &'static str,
    pub(crate) shortcut: Option<&'static str>,
    pub(crate) danger: bool,
}

/// A row of the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MenuRow {
    Item(MenuItemSpec),
    Separator,
}

/// What the menu is opened on.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MenuTarget {
    pub(crate) in_trash: bool,
    /// How many entries are selected (0: the menu of the empty space).
    pub(crate) selected: usize,
    /// Whether the shown folder has no entries at all.
    pub(crate) folder_is_empty: bool,
    /// Whether a terminal can be opened (see [`pick_terminal`]).
    pub(crate) terminal: bool,
}

/// The menu rows for a target; empty when there is nothing to offer (the
/// caller then opens no menu). Clipboard and drag & drop items come with
/// their own task.
pub(crate) fn menu_rows(target: &MenuTarget) -> Vec<MenuRow> {
    let item = |command, label, shortcut, danger| {
        MenuRow::Item(MenuItemSpec {
            command,
            label,
            shortcut,
            danger,
        })
    };
    let properties = || item(MenuCommand::Properties, "Properties", Some("Ctrl+I"), false);
    let delete = || {
        item(
            MenuCommand::DeletePermanently,
            "Delete permanently\u{2026}",
            Some("Shift+Del"),
            true,
        )
    };
    if target.in_trash {
        return if target.selected > 0 {
            vec![
                item(MenuCommand::Restore, "Restore", None, false),
                MenuRow::Separator,
                properties(),
                delete(),
            ]
        } else if !target.folder_is_empty {
            vec![item(MenuCommand::EmptyTrash, "Empty Trash", None, true)]
        } else {
            Vec::new()
        };
    }
    if target.selected > 0 {
        let mut rows = vec![
            item(MenuCommand::Open, "Open", Some("Enter"), false),
            MenuRow::Separator,
        ];
        if target.selected == 1 {
            rows.push(item(MenuCommand::Rename, "Rename", Some("F2"), false));
        }
        rows.extend([
            item(
                MenuCommand::CopyPath,
                "Copy path",
                Some("Ctrl+Shift+C"),
                false,
            ),
            MenuRow::Separator,
            properties(),
            item(MenuCommand::MoveToTrash, "Move to Trash", Some("Del"), true),
            delete(),
        ]);
        return rows;
    }
    let mut rows = vec![
        item(
            MenuCommand::NewFolder,
            "New folder",
            Some("Ctrl+Shift+N"),
            false,
        ),
        item(MenuCommand::NewFile, "New file", None, false),
        MenuRow::Separator,
    ];
    if target.terminal {
        rows.push(item(
            MenuCommand::OpenTerminal,
            "Open terminal here",
            None,
            false,
        ));
    }
    rows.push(item(
        MenuCommand::CopyFolderPath,
        "Copy folder path",
        None,
        false,
    ));
    rows
}

/// The row the arrow keys move to from `current` (`None`: nothing
/// highlighted yet), skipping separators and wrapping around. `forward`
/// false goes up. `None` when the menu has no item.
pub(crate) fn next_menu_index(
    rows: &[MenuRow],
    current: Option<usize>,
    forward: bool,
) -> Option<usize> {
    let count = rows.len();
    let mut index = current;
    for _ in 0..count {
        index = Some(match (index, forward) {
            (None, true) => 0,
            (None, false) => count.checked_sub(1)?,
            (Some(at), true) => (at + 1) % count,
            (Some(at), false) => (at + count - 1) % count,
        });
        if matches!(index.and_then(|at| rows.get(at)), Some(MenuRow::Item(_))) {
            return index;
        }
    }
    None
}

// --- confirmation dialogs -----------------------------------------------------------

/// Which decision the dialog asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DialogKind {
    /// Shift+Del.
    DeletePermanently,
    /// The Trash is unavailable for these items.
    NoTrash,
    /// Delete from the Trash view.
    DeleteFromTrash,
    /// Empty the Trash.
    EmptyTrash { count: usize },
}

/// What a confirmed dialog does. It carries exactly what its own dialog
/// showed: nothing that happens while the dialog is open can change it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DialogAction {
    DeletePermanently(Vec<PathBuf>),
    DeleteFromTrash(Vec<OsString>),
    /// The Trash names the dialog counted; nothing else is deleted.
    EmptyTrash(Vec<OsString>),
}

/// One confirmation: its words and the action its danger button runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DialogState {
    pub(crate) kind: DialogKind,
    pub(crate) text: DialogText,
    pub(crate) action: DialogAction,
}

impl DialogState {
    pub(crate) fn new(kind: DialogKind, names: &[String], action: DialogAction) -> DialogState {
        DialogState {
            text: dialog_text(&kind, names),
            kind,
            action,
        }
    }

    /// The button that has the keyboard focus when the dialog opens.
    pub(crate) fn initial_focus(&self) -> DialogChoice {
        DIALOG_DEFAULT
    }
}

/// The dialog that asks about permanently deleting what had no Trash,
/// bound to exactly these paths.
pub(crate) fn no_trash_dialog(paths: Vec<PathBuf>) -> DialogState {
    let names: Vec<String> = paths
        .iter()
        .map(|path| {
            path.file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    DialogState::new(
        DialogKind::NoTrash,
        &names,
        DialogAction::DeletePermanently(paths),
    )
}

/// What dismissing a dialog says: declining "delete permanently" for items
/// that had no Trash leaves them where they are, and that is reported
/// rather than silently dropped.
pub(crate) fn dismissed_notice(dialog: &DialogState) -> Option<Notice> {
    match (&dialog.kind, &dialog.action) {
        (DialogKind::NoTrash, DialogAction::DeletePermanently(paths)) => Some(Notice {
            severity: Severity::Info,
            text: not_deleted(paths.len()),
        }),
        _ => None,
    }
}

/// Dialogs wait in line: one is open, a background job's follow-up queues
/// behind it and opens only after the open one closed.
#[derive(Debug, Default)]
pub(crate) struct DialogQueue {
    open: Option<DialogState>,
    waiting: VecDeque<DialogState>,
}

impl DialogQueue {
    /// Opens `dialog` now when none is open, else queues it.
    pub(crate) fn offer(&mut self, dialog: DialogState) {
        if self.open.is_none() {
            self.open = Some(dialog);
        } else {
            self.waiting.push_back(dialog);
        }
    }

    pub(crate) fn current(&self) -> Option<&DialogState> {
        self.open.as_ref()
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open.is_some()
    }

    #[cfg(test)]
    pub(crate) fn waiting(&self) -> usize {
        self.waiting.len()
    }

    /// Closes the open dialog, returns it, and opens the next in line.
    pub(crate) fn close(&mut self) -> Option<DialogState> {
        let closed = self.open.take();
        self.open = self.waiting.pop_front();
        closed
    }
}

/// The buttons of a confirmation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DialogChoice {
    Cancel,
    Confirm,
}

/// The button a dialog opens focused on: the safe one. Enter presses the
/// focused button, so with this default Enter alone never confirms a
/// deletion; only a deliberate move of the focus does.
pub(crate) const DIALOG_DEFAULT: DialogChoice = DialogChoice::Cancel;

/// The words of a confirmation dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DialogText {
    pub(crate) title: String,
    pub(crate) body: String,
    /// The item names to list, shortened to a handful plus "…and N more".
    pub(crate) names: Vec<String>,
    /// The danger button's label.
    pub(crate) confirm: &'static str,
}

/// How many names a dialog lists before "…and N more".
const LISTED_NAMES: usize = 5;

fn listed(names: &[String]) -> Vec<String> {
    if names.len() <= LISTED_NAMES {
        return names.to_vec();
    }
    let mut shown: Vec<String> = names.iter().take(LISTED_NAMES).cloned().collect();
    shown.push(format!("\u{2026}and {} more", names.len() - LISTED_NAMES));
    shown
}

/// The title, body and button of a confirmation, naming the item or the
/// count.
pub(crate) fn dialog_text(kind: &DialogKind, names: &[String]) -> DialogText {
    let count = names.len();
    let first = names
        .first()
        .map(|name| format!("\u{201c}{name}\u{201d}"))
        .unwrap_or_default();
    let (title, body, confirm) = match kind {
        DialogKind::DeletePermanently => (
            if count == 1 {
                format!("Delete {first} permanently?")
            } else {
                format!("Delete {count} items permanently?")
            },
            if count == 1 {
                format!("{first} will be deleted for good. This cannot be undone.")
            } else {
                format!("These {count} items will be deleted for good. This cannot be undone.")
            },
            "Delete permanently",
        ),
        DialogKind::NoTrash => (
            "No Trash available".to_owned(),
            if count == 1 {
                format!(
                    "{first} can't be moved to the Trash. Delete it permanently instead? This cannot be undone."
                )
            } else {
                format!(
                    "These {count} items can't be moved to the Trash. Delete them permanently instead? This cannot be undone."
                )
            },
            "Delete permanently",
        ),
        DialogKind::DeleteFromTrash => (
            if count == 1 {
                format!("Delete {first} permanently?")
            } else {
                format!("Delete {count} items permanently?")
            },
            if count == 1 {
                format!("{first} will be deleted from the Trash for good. This cannot be undone.")
            } else {
                format!(
                    "These {count} items will be deleted from the Trash for good. This cannot be undone."
                )
            },
            "Delete permanently",
        ),
        DialogKind::EmptyTrash { count } => (
            "Empty the Trash?".to_owned(),
            if *count == 1 {
                "The only item in the Trash will be deleted for good. This cannot be undone."
                    .to_owned()
            } else {
                format!(
                    "All {count} items in the Trash will be deleted for good. This cannot be undone."
                )
            },
            "Empty Trash",
        ),
    };
    DialogText {
        title,
        body,
        names: listed(names),
        confirm,
    }
}

// --- terminal ----------------------------------------------------------------------

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// An executable named `name` (a bare name is looked up in `path_var`).
fn resolve_program(name: &OsStr, path_var: Option<&OsStr>) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let direct = Path::new(name);
    if direct.components().count() > 1 {
        return is_executable(direct).then(|| direct.to_path_buf());
    }
    std::env::split_paths(path_var?)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// The terminal "Open terminal here" runs: `$TERMINAL` when it names an
/// existing executable, else `xdg-terminal-exec`; `None` when neither
/// exists (the menu then omits the item).
pub(crate) fn pick_terminal(terminal: Option<&OsStr>, path_var: Option<&OsStr>) -> Option<PathBuf> {
    terminal
        .and_then(|name| resolve_program(name, path_var))
        .or_else(|| resolve_program(OsStr::new("xdg-terminal-exec"), path_var))
}

#[cfg(test)]
#[path = "ops_tests.rs"]
mod tests;
