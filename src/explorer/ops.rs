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
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bolt::controls::InputState;
use fileflux::core::Entry;
use fileflux::core::apps::{App, OpenWith, expand_exec, filter_apps};
use fileflux::core::devices::DevicePlan;
use fileflux::core::images::is_image_path;
use fileflux::core::ops::{self as core_ops, validate_name};
use fileflux::core::places::{FuseRoots, Places};
use fileflux::core::trash::{
    self, RestoreConflict, TrashError, TrashTarget, TrashedItem, trash_dir_for,
};
use fileflux::core::watch::nearest_existing_ancestor;
use gpui::Entity;

use super::arrange::ArrangeCommand;
use super::progress::Sink;

#[path = "paste.rs"]
pub(crate) mod paste;

pub(crate) use paste::PlacedEntry;
use paste::{ConflictRequest, PasteJob};

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

/// Which file a path named when an operation finished: device, inode and
/// birth time. An undo acts only when the item at the recorded location is
/// still this one; a different file under the same name is someone else's.
///
/// Device and inode alone are not enough: ext4 hands a deleted file's inode
/// number to the next file created, so "delete, then write a new file with
/// the same name" looks identical. The birth time (statx `btime`) tells
/// them apart wherever the filesystem records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) born: Option<std::time::SystemTime>,
}

impl Identity {
    fn from_meta(meta: &fs::Metadata) -> Identity {
        Identity {
            dev: meta.dev(),
            ino: meta.ino(),
            born: meta.created().ok(),
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
    /// Undo of a paste that copied: move the created copies to the Trash,
    /// each only if it is still the item that was created.
    Copy { items: Vec<PlacedEntry> },
    /// Undo of a paste that moved: move the items back to where they came
    /// from, each only if it is still the item that was moved, never
    /// replacing anything.
    Move { items: Vec<PlacedEntry> },
}

/// An undo action with the id a toast's Undo button names it by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UndoEntry {
    pub(crate) id: u64,
    pub(crate) action: UndoAction,
    /// The toast whose Undo button names this entry, once it is shown.
    pub(crate) toast: Option<u64>,
}

/// The session's undo history: newest last, bounded, surviving navigation.
#[derive(Debug, Default)]
pub(crate) struct UndoStack {
    entries: Vec<UndoEntry>,
    last_id: u64,
    /// Entries whose undo waits for the device check: no second press may
    /// start the same one.
    pending: Vec<u64>,
}

/// Why an entry cannot be claimed for an undo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unavailable {
    /// Not on the stack: undone already (or never there).
    Gone,
    /// Another undo of it is waiting for its device check.
    InFlight,
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
            toast: None,
        });
        self.last_id
    }

    /// Records which toast offers Undo for entry `id`, so undoing it some
    /// other way (Ctrl+Z) can close that toast instead of leaving a button
    /// that no longer does anything.
    pub(crate) fn attach_toast(&mut self, id: u64, toast: u64) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
            entry.toast = Some(toast);
        }
    }

    /// The newest entry, left on the stack: an undo that may be refused
    /// looks before it takes.
    pub(crate) fn newest(&self) -> Option<&UndoEntry> {
        self.entries.last()
    }

    /// One entry by id, left on the stack.
    pub(crate) fn get(&self, id: u64) -> Option<&UndoEntry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    /// Claims entry `id` for an undo that waits for the device check. Until
    /// [`Self::settle`], the entry stays on the stack (a refused undo keeps
    /// it) but no other undo can claim it.
    pub(crate) fn begin(&mut self, id: u64) -> Result<(), Unavailable> {
        if self.get(id).is_none() {
            Err(Unavailable::Gone)
        } else if self.pending.contains(&id) {
            Err(Unavailable::InFlight)
        } else {
            self.pending.push(id);
            Ok(())
        }
    }

    /// Ends the claim of [`Self::begin`] with the verdict of the final
    /// device check: allowed, the entry leaves the history and is returned
    /// to run; refused, it stays undoable.
    pub(crate) fn settle(&mut self, id: u64, allowed: bool) -> Option<UndoEntry> {
        self.pending.retain(|pending| *pending != id);
        if allowed { self.take(id) } else { None }
    }

    /// Removes and returns the newest entry.
    #[cfg(test)]
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
    /// Sources a paste moved away from their original place (a cut that
    /// is only partly done keeps the rest on the clipboard).
    pub(crate) moved: Vec<PathBuf>,
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
    /// Copy or move from the clipboard; asks the UI about conflicts.
    Paste(PasteJob),
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
    /// path; anything else is `NoTrash`. Paths on gvfs (phones, shares) have
    /// no Trash, whatever `mounts` says.
    #[cfg(test)]
    pub(crate) fn from_env(trash_root: PathBuf, mounts: Vec<PathBuf>) -> JobCtx {
        Self::from_env_with(trash_root, mounts, FuseRoots::system())
    }

    /// The chooser for the discovered `places`, on the UI thread: the mounts
    /// and the FUSE roots are the ones discovery read off it, so this reads
    /// neither the mount table nor the filesystem. Without places the root is
    /// empty, which no Trash can be chosen under: deletes then ask for
    /// confirmation instead.
    pub(crate) fn for_places(places: Option<&Places>, trash_root: Option<PathBuf>) -> JobCtx {
        let mounts = places.map_or_else(Vec::new, |places| {
            places
                .volumes
                .iter()
                .filter(|volume| !volume.kind.is_gvfs())
                .map(|volume| volume.path.clone())
                .collect()
        });
        let roots = places.map_or_else(FuseRoots::default, |places| places.fuse.clone());
        Self::from_env_with(trash_root.unwrap_or_default(), mounts, roots)
    }

    /// [`Self::from_env`] with the FUSE roots given.
    pub(crate) fn from_env_with(
        trash_root: PathBuf,
        mounts: Vec<PathBuf>,
        roots: FuseRoots,
    ) -> JobCtx {
        let root = trash_root.clone();
        JobCtx {
            trash_root,
            target_for: Box::new(move |path| choose_target(path, &root, &mounts, &roots)),
        }
    }
}

fn choose_target(
    path: &Path,
    trash_root: &Path,
    mounts: &[PathBuf],
    roots: &FuseRoots,
) -> Result<TrashTarget, TrashError> {
    // gvfs has no Trash, and a `.Trash-<uid>` must never be created on a
    // phone or a share: the delete falls to its confirmation instead.
    if roots.is_gvfs(path) {
        return Err(TrashError::NoTrash(format!(
            "{} is on a phone or network mount, which has no Trash",
            path.display()
        )));
    }
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
        Job::Paste(job) => paste::run_paste(job, sink),
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
        let name = match core_ops::unique_name(dir, OsStr::new(kind.base_name())) {
            Ok(name) => name,
            Err(error) => {
                return Report::error(format!("Could not create the {}: {error}", kind.noun()));
            }
        };
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
        UndoAction::Copy { items } => Ok(paste::undo_copy(items, ctx, sink)),
        UndoAction::Move { items } => Ok(paste::undo_move(items, sink)),
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

/// The text the inline editor starts with for `name`, or why this name
/// cannot be edited inline. The editor is a single-line field that drops
/// line breaks and other control characters, so committing an untouched
/// field would rename the file to a different name than the one shown;
/// such names are refused up front and the file is left alone.
pub(crate) fn rename_seed(name: &OsStr) -> Result<String, &'static str> {
    let text = name.to_string_lossy();
    if text.chars().any(char::is_control) {
        return Err("This name has characters that can't be edited here");
    }
    Ok(text.into_owned())
}

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
    /// Open the single selected image in the viewer window.
    OpenInViewer,
    /// Open the single selected folder in a new background tab.
    OpenInNewTab,
    /// Recent view: go to the folder that holds the single selected entry,
    /// with the entry selected there.
    OpenContainingFolder,
    /// Choose the application to open the single selected file with.
    OpenWith,
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
    Cut,
    Copy,
    /// Paste into the folder that is shown.
    Paste,
    /// Paste into the single selected folder ("Paste into folder").
    PasteIntoFolder,
    /// A sort or filter change; the menu stays open and redraws its checks.
    Arrange(ArrangeCommand),
    /// Shows another page of the open menu ("Sort by", "Filter").
    Page(MenuPage),
    /// Sidebar rows: pin the row's folder to the Favorites.
    AddFavorite,
    /// Sidebar rows: unpin the row's folder from the Favorites.
    RemoveFavorite,
    /// Devices: mount the unmounted filesystem.
    Mount,
    /// Devices: unmount the mounted filesystem.
    Unmount,
    /// Devices: unmount every partition of the disk and power it off.
    Eject,
}

/// Which page of the one menu is showing. The empty-space menu opens on
/// [`MenuPage::Context`] and drills into the other two; the subbar buttons
/// open them directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuPage {
    Context,
    Sort,
    Filter,
}

/// One drawn menu item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MenuItemSpec {
    pub(crate) command: MenuCommand,
    pub(crate) label: &'static str,
    pub(crate) shortcut: Option<&'static str>,
    pub(crate) danger: bool,
    /// A disabled item is drawn dimmed, skipped by the arrow keys and does
    /// nothing when activated.
    pub(crate) enabled: bool,
    /// `Some` for a checkable row: `true` draws the check mark.
    pub(crate) checked: Option<bool>,
    /// The row opens another page of the menu: drawn with a chevron.
    pub(crate) submenu: bool,
}

impl MenuItemSpec {
    /// An enabled, plain row.
    pub(crate) fn new(command: MenuCommand, label: &'static str) -> Self {
        Self {
            command,
            label,
            shortcut: None,
            danger: false,
            enabled: true,
            checked: None,
            submenu: false,
        }
    }
}

/// A row of the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MenuRow {
    Item(MenuItemSpec),
    /// A caption naming the group of rows under it; never selectable.
    Header(&'static str),
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
    /// Whether a terminal can be opened (see
    /// [`fileflux::core::terminal::resolve`]).
    pub(crate) terminal: bool,
    /// Whether the clipboard holds something to paste.
    pub(crate) can_paste: bool,
    /// Exactly one entry is selected and it is a folder (not a link to one).
    pub(crate) folder_selected: bool,
    /// Exactly one entry is selected and it is not a folder: "Open with…"
    /// applies.
    pub(crate) single_file: bool,
    /// The one selected folder is a favorite already.
    pub(crate) folder_favorite: bool,
}

/// What Enter or a double click does to one entry.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OpenAction {
    /// Go into the folder.
    Navigate,
    /// Hand it to the default application.
    External,
}

/// A folder, and a symlink that points to one, is entered; a file, a link to
/// a file and a broken link open with the default application. The scan
/// resolved a link's kind through its target, so `is_directory` already
/// covers the link to a folder.
pub(crate) fn open_action(entry: &Entry) -> OpenAction {
    if entry.is_directory() {
        OpenAction::Navigate
    } else {
        OpenAction::External
    }
}

/// What opening one entry's file does.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OpenTarget {
    /// The viewer window, on `list[index]`, walking `list`.
    Viewer { list: Vec<PathBuf>, index: usize },
    /// The system's default application (`xdg-open`).
    External,
}

/// Which window opens `path`: an image opens the viewer, walking the
/// images of `displayed` (the folder's entries in the order the explorer
/// shows them); everything else goes to the default application. An image
/// that is not among `displayed` is viewed on its own.
pub(crate) fn open_target(path: &Path, displayed: &[&Entry]) -> OpenTarget {
    if !is_image_path(path) {
        return OpenTarget::External;
    }
    let list: Vec<PathBuf> = displayed
        .iter()
        .filter(|entry| !entry.is_directory() && is_image_path(&entry.path))
        .map(|entry| entry.path.clone())
        .collect();
    match list.iter().position(|candidate| candidate == path) {
        Some(index) => OpenTarget::Viewer { list, index },
        None => OpenTarget::Viewer {
            list: vec![path.to_path_buf()],
            index: 0,
        },
    }
}

/// Adds "Open in viewer" right after "Open" in the menu of one selected
/// image. A menu without an "Open" row is left alone.
pub(crate) fn add_viewer_row(rows: &mut Vec<MenuRow>) {
    let after_open = rows
        .iter()
        .position(|row| matches!(row, MenuRow::Item(spec) if spec.command == MenuCommand::Open));
    if let Some(at) = after_open {
        rows.insert(
            at + 1,
            MenuRow::Item(MenuItemSpec::new(
                MenuCommand::OpenInViewer,
                "Open in viewer",
            )),
        );
    }
}

/// The menu rows for a target; empty when there is nothing to offer (the
/// caller then opens no menu). Drag & drop comes with its own task.
pub(crate) fn menu_rows(target: &MenuTarget) -> Vec<MenuRow> {
    let item = |command, label, shortcut, danger| {
        MenuRow::Item(MenuItemSpec {
            shortcut,
            danger,
            ..MenuItemSpec::new(command, label)
        })
    };
    // Paste needs something on the clipboard; Cut and Copy are only ever
    // offered on a selection.
    let paste = |command, label| {
        MenuRow::Item(MenuItemSpec {
            shortcut: Some("Ctrl+V"),
            enabled: target.can_paste,
            ..MenuItemSpec::new(command, label)
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
        let mut rows = vec![item(MenuCommand::Open, "Open", Some("Enter"), false)];
        if target.folder_selected {
            rows.push(item(
                MenuCommand::OpenInNewTab,
                "Open in new tab",
                None,
                false,
            ));
            rows.push(if target.folder_favorite {
                item(
                    MenuCommand::RemoveFavorite,
                    "Remove from favorites",
                    None,
                    false,
                )
            } else {
                item(MenuCommand::AddFavorite, "Add to favorites", None, false)
            });
            if target.terminal {
                rows.push(item(
                    MenuCommand::OpenTerminal,
                    "Open in terminal",
                    None,
                    false,
                ));
            }
        }
        if target.single_file {
            rows.push(item(
                MenuCommand::OpenWith,
                "Open with\u{2026}",
                None,
                false,
            ));
        }
        rows.extend([
            MenuRow::Separator,
            item(MenuCommand::Cut, "Cut", Some("Ctrl+X"), false),
            item(MenuCommand::Copy, "Copy", Some("Ctrl+C"), false),
            if target.folder_selected {
                paste(MenuCommand::PasteIntoFolder, "Paste into folder")
            } else {
                paste(MenuCommand::Paste, "Paste")
            },
            MenuRow::Separator,
        ]);
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
        paste(MenuCommand::Paste, "Paste"),
        MenuRow::Separator,
        page_row(MenuPage::Sort, "Sort by"),
        page_row(MenuPage::Filter, "Filter"),
        MenuRow::Separator,
    ];
    if target.terminal {
        rows.push(item(
            MenuCommand::OpenTerminal,
            "Open in terminal",
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

/// The menu of the Recent view, from the folder view's `rows`: nothing is
/// created or pasted into a list of files that live elsewhere, and one
/// selected entry offers "Open containing folder".
pub(crate) fn recent_menu_rows(rows: Vec<MenuRow>, selected: usize) -> Vec<MenuRow> {
    let kept = |row: &MenuRow| {
        !matches!(row, MenuRow::Item(spec) if matches!(
            spec.command,
            MenuCommand::NewFolder
                | MenuCommand::NewFile
                | MenuCommand::Paste
                | MenuCommand::PasteIntoFolder
                | MenuCommand::OpenTerminal
                | MenuCommand::CopyFolderPath
        ))
    };
    let mut shown: Vec<MenuRow> = Vec::with_capacity(rows.len() + 1);
    for row in rows.into_iter().filter(kept) {
        // A separator only ever sits between two items.
        if row == MenuRow::Separator && matches!(shown.last(), None | Some(MenuRow::Separator)) {
            continue;
        }
        let is_open = matches!(&row, MenuRow::Item(spec) if spec.command == MenuCommand::Open);
        shown.push(row);
        if is_open && selected == 1 {
            shown.push(MenuRow::Item(MenuItemSpec::new(
                MenuCommand::OpenContainingFolder,
                "Open containing folder",
            )));
        }
    }
    if matches!(shown.last(), Some(MenuRow::Separator)) {
        shown.pop();
    }
    shown
}

/// The title of the context menu: the single entry's name, "N items" for a
/// multi-selection, or "<Folder> (this folder)" on the empty space.
pub(crate) fn menu_title(selected: &[String], folder: &str) -> String {
    match selected {
        [] => format!("{folder} (this folder)"),
        [name] => name.clone(),
        many => format!("{} items", many.len()),
    }
}

/// What a sidebar row's menu is opened on.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SidebarTarget {
    /// The virtual Trash row.
    pub(crate) trash: bool,
    /// The virtual Recent row: it only opens.
    pub(crate) recent: bool,
    /// The row's folder is already a favorite.
    pub(crate) favorite: bool,
    /// Whether a terminal can be opened.
    pub(crate) terminal: bool,
}

/// The menu of a sidebar row. Every row opens in place or in a new tab; a
/// folder can be pinned or unpinned, copied and inspected; the Trash is
/// never pinned and can be emptied.
pub(crate) fn sidebar_menu_rows(target: &SidebarTarget) -> Vec<MenuRow> {
    let item = |command, label, danger| {
        MenuRow::Item(MenuItemSpec {
            danger,
            ..MenuItemSpec::new(command, label)
        })
    };
    let mut rows = vec![
        item(MenuCommand::Open, "Open", false),
        item(MenuCommand::OpenInNewTab, "Open in new tab", false),
    ];
    if target.recent {
        return rows;
    }
    if target.trash {
        rows.extend([
            MenuRow::Separator,
            item(MenuCommand::EmptyTrash, "Empty Trash\u{2026}", true),
        ]);
        return rows;
    }
    if target.terminal {
        rows.push(item(MenuCommand::OpenTerminal, "Open in terminal", false));
    }
    rows.push(MenuRow::Separator);
    rows.push(if target.favorite {
        item(MenuCommand::RemoveFavorite, "Remove from favorites", false)
    } else {
        item(MenuCommand::AddFavorite, "Add to favorites", false)
    });
    rows.extend([
        item(MenuCommand::CopyPath, "Copy path", false),
        MenuRow::Separator,
        item(MenuCommand::Properties, "Properties", false),
    ]);
    rows
}

/// A row that opens another page of the menu.
pub(crate) fn page_row(page: MenuPage, label: &'static str) -> MenuRow {
    MenuRow::Item(MenuItemSpec {
        submenu: true,
        ..MenuItemSpec::new(MenuCommand::Page(page), label)
    })
}

/// The row the arrow keys move to from `current` (`None`: nothing
/// highlighted yet), skipping separators and disabled items and wrapping around. `forward`
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
        if matches!(
            index.and_then(|at| rows.get(at)),
            Some(MenuRow::Item(MenuItemSpec { enabled: true, .. }))
        ) {
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
    /// Choose the application to open `file` (of type `mime`) with.
    OpenWith {
        file: String,
        mime: String,
        /// The list of every application shows instead of the ones for `mime`.
        other: bool,
    },
    /// A paste found `name` already in `folder`; the two lines describe the
    /// item there and the one arriving.
    Conflict {
        name: String,
        folder: String,
        existing: String,
        incoming: String,
    },
    /// Running operations use the device that is about to be unmounted or
    /// ejected; `count` of them, the device is called `label`.
    DeviceBusy {
        label: String,
        count: usize,
        eject: bool,
    },
}

/// What a confirmed dialog does. It carries exactly what its own dialog
/// showed: nothing that happens while the dialog is open can change it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DialogAction {
    DeletePermanently(Vec<PathBuf>),
    DeleteFromTrash(Vec<OsString>),
    /// The Trash names the dialog counted; nothing else is deleted.
    EmptyTrash(Vec<OsString>),
    /// Run the selected application on the file.
    OpenWith(OpenWithState),
    /// A running paste waits for the answer. `apply_all` is the checkbox
    /// "Apply to all remaining conflicts".
    Conflict {
        job: u64,
        apply_all: bool,
    },
    /// "Wait and eject": queue the plan until nothing uses the device.
    DeviceWhenIdle(DevicePlan),
}

/// The "Open with…" chooser. Its rows are the applications for `mime` and
/// a last "Other application…" row; once that is chosen they are every
/// application that takes a file, narrowed by `filter`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OpenWithState {
    pub(crate) path: PathBuf,
    pub(crate) mime: String,
    /// The applications that handle `mime`, the default first.
    pub(crate) apps: Vec<App>,
    /// Every application that takes a file, by name.
    pub(crate) all_apps: Vec<App>,
    pub(crate) selected: usize,
    /// The list of every application shows.
    pub(crate) other: bool,
    pub(crate) filter: String,
    /// "Remember for <mime>": the chosen application becomes the default.
    pub(crate) remember: bool,
    /// The filter box, once the dialog has a window to make it in.
    pub(crate) filter_input: Option<Entity<InputState>>,
}

/// The label of the last row of the list for the file's type.
const OTHER_ROW: &str = "Other application\u{2026}";

impl OpenWithState {
    /// The applications the rows list now.
    fn rows(&self) -> Vec<App> {
        if self.other {
            filter_apps(&self.all_apps, &self.filter)
        } else {
            self.apps.clone()
        }
    }

    /// How many rows there are: the typed list has the extra last row.
    fn row_count(&self) -> usize {
        if self.other {
            filter_apps(&self.all_apps, &self.filter).len()
        } else {
            self.apps.len() + 1
        }
    }
}

/// What confirming the "Open with…" chooser does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OpenWithPick {
    /// The "Other application…" row: show every application instead.
    Other,
    /// Run `app` on `path`; `remember` is the MIME type to make `app` the
    /// default for.
    Launch {
        path: PathBuf,
        app: App,
        remember: Option<String>,
    },
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

    /// The dialog a paste opens when its target already exists.
    pub(crate) fn conflict(job: u64, request: ConflictRequest) -> DialogState {
        DialogState {
            text: dialog_text(&request.kind, &[]),
            kind: request.kind,
            action: DialogAction::Conflict {
                job,
                apply_all: false,
            },
        }
    }

    /// The "Open with…" chooser for `path`: the applications for its type
    /// in the order found (the default first, selected), then the
    /// "Other application…" row.
    pub(crate) fn open_with(path: &Path, found: &OpenWith) -> DialogState {
        let state = OpenWithState {
            path: path.to_path_buf(),
            mime: found.mime.clone(),
            apps: found.apps.clone(),
            all_apps: found.all_apps.clone(),
            selected: 0,
            other: false,
            filter: String::new(),
            remember: false,
            filter_input: None,
        };
        let kind = DialogKind::OpenWith {
            file: display_name(path),
            mime: found.mime.clone(),
            other: false,
        };
        let mut dialog = DialogState {
            text: dialog_text(&kind, &[]),
            kind,
            action: DialogAction::OpenWith(state),
        };
        dialog.refresh_open_with();
        dialog
    }

    fn open_with_state(&self) -> Option<&OpenWithState> {
        match &self.action {
            DialogAction::OpenWith(state) => Some(state),
            _ => None,
        }
    }

    fn open_with_state_mut(&mut self) -> Option<&mut OpenWithState> {
        match &mut self.action {
            DialogAction::OpenWith(state) => Some(state),
            _ => None,
        }
    }

    /// Rebuilds the words and rows from the chooser's state.
    fn refresh_open_with(&mut self) {
        let Some(state) = self.open_with_state() else {
            return;
        };
        let mut names: Vec<String> = state.rows().into_iter().map(|app| app.name).collect();
        if !state.other {
            names.push(OTHER_ROW.to_owned());
        }
        let other = state.other;
        if let DialogKind::OpenWith { other: kind, .. } = &mut self.kind {
            *kind = other;
        }
        self.text = dialog_text(&self.kind, &names);
    }

    /// "Other application…" was chosen: list every application.
    pub(crate) fn show_other_apps(&mut self) {
        if let Some(state) = self.open_with_state_mut() {
            state.other = true;
            state.selected = 0;
            state.filter.clear();
        }
        self.refresh_open_with();
    }

    /// The text of the filter box changed: narrows the list of every
    /// application and selects its first row.
    pub(crate) fn set_open_with_filter(&mut self, text: &str) {
        let Some(state) = self.open_with_state_mut() else {
            return;
        };
        if !state.other {
            return;
        }
        state.filter = text.to_owned();
        state.selected = 0;
        self.refresh_open_with();
    }

    /// Up / Down: moves the selected row, stopping at both ends.
    pub(crate) fn move_open_with(&mut self, delta: isize) {
        if let Some(state) = self.open_with_state_mut() {
            state.selected = move_selection(state.selected, state.row_count(), delta);
        }
    }

    /// The row at `index`, when there is one.
    pub(crate) fn select_open_with(&mut self, index: usize) {
        if let Some(state) = self.open_with_state_mut()
            && index < state.row_count()
        {
            state.selected = index;
        }
    }

    pub(crate) fn set_open_with_filter_input(&mut self, input: Entity<InputState>) {
        if let Some(state) = self.open_with_state_mut() {
            state.filter_input = Some(input);
        }
    }

    pub(crate) fn set_open_with_remember(&mut self, remember: bool) {
        if let Some(state) = self.open_with_state_mut() {
            state.remember = remember;
        }
    }

    pub(crate) fn open_with_remember(&self) -> bool {
        self.open_with_state().is_some_and(|state| state.remember)
    }

    /// The checkbox label of the "Open with…" chooser.
    pub(crate) fn remember_label(&self) -> Option<String> {
        self.open_with_state()
            .map(|state| format!("Remember for {}", state.mime))
    }

    /// What confirming does now; `None` when no row is selected (a filter
    /// that matches nothing) or this is not the chooser.
    pub(crate) fn open_with_pick(&self) -> Option<OpenWithPick> {
        let state = self.open_with_state()?;
        if !state.other && state.selected == state.apps.len() {
            return Some(OpenWithPick::Other);
        }
        let app = state.rows().into_iter().nth(state.selected)?;
        Some(OpenWithPick::Launch {
            path: state.path.clone(),
            app,
            remember: state.remember.then(|| state.mime.clone()),
        })
    }

    /// The button that has the keyboard focus when the dialog opens: the
    /// safe one, never the one that destroys something. Opening a file
    /// destroys nothing, so Enter there launches the selected application.
    pub(crate) fn initial_focus(&self) -> DialogChoice {
        match self.kind {
            DialogKind::OpenWith { .. } => DialogChoice::Confirm,
            DialogKind::Conflict { .. } => CONFLICT_DEFAULT,
            _ => DIALOG_DEFAULT,
        }
    }

    /// The paste this dialog speaks for, when it is a conflict question.
    pub(crate) fn conflict_job(&self) -> Option<u64> {
        match self.action {
            DialogAction::Conflict { job, .. } => Some(job),
            _ => None,
        }
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

/// How long a dialog that has just appeared ignores its danger button.
/// A double-click on the button of the dialog before it would otherwise
/// land its second click on whatever dialog is promoted under the pointer.
pub(crate) const ARM_DELAY: Duration = Duration::from_millis(400);

/// Whether a dialog that appeared at `opened_at` accepts its danger button
/// at `now`.
pub(crate) fn is_armed(opened_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(opened_at) >= ARM_DELAY
}

/// Dialogs wait in line: one is open, a background job's follow-up queues
/// behind it and opens only after the open one closed. A dialog's clock
/// starts when it *appears*, whether it was offered to an empty queue or
/// promoted from the line.
#[derive(Debug, Default)]
pub(crate) struct DialogQueue {
    open: Option<(DialogState, Instant)>,
    waiting: VecDeque<DialogState>,
}

impl DialogQueue {
    /// Opens `dialog` now when none is open, else queues it.
    pub(crate) fn offer(&mut self, dialog: DialogState) {
        self.offer_at(dialog, Instant::now());
    }

    pub(crate) fn offer_at(&mut self, dialog: DialogState, now: Instant) {
        if self.open.is_none() {
            self.open = Some((dialog, now));
        } else {
            self.waiting.push_back(dialog);
        }
    }

    pub(crate) fn current(&self) -> Option<&DialogState> {
        self.open.as_ref().map(|(dialog, _)| dialog)
    }

    pub(crate) fn current_mut(&mut self) -> Option<&mut DialogState> {
        self.open.as_mut().map(|(dialog, _)| dialog)
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Whether the open dialog accepts its danger button at `now`; false
    /// while it is still too fresh, and when none is open.
    pub(crate) fn armed_at(&self, now: Instant) -> bool {
        self.open
            .as_ref()
            .is_some_and(|(_, opened_at)| is_armed(*opened_at, now))
    }

    /// Whether a press of `choice` is acted on at `now`: nothing is open,
    /// or the danger button is still too fresh, and the press is ignored.
    pub(crate) fn accepts(&self, choice: DialogChoice, now: Instant) -> bool {
        self.is_open() && (!choice.is_danger() || self.armed_at(now))
    }

    /// The button the open dialog takes the keyboard on. This is the one
    /// place the default is decided, for a dialog that opened at once and
    /// one promoted from the line alike: Enter presses the focused button,
    /// so Enter alone never reaches a danger button.
    pub(crate) fn default_focus(&self) -> Option<DialogChoice> {
        self.current().map(DialogState::initial_focus)
    }

    #[cfg(test)]
    pub(crate) fn waiting(&self) -> usize {
        self.waiting.len()
    }

    /// Closes the open dialog, returns it, and opens the next in line.
    pub(crate) fn close(&mut self) -> Option<DialogState> {
        self.close_at(Instant::now())
    }

    pub(crate) fn close_at(&mut self, now: Instant) -> Option<DialogState> {
        let closed = self.open.take().map(|(dialog, _)| dialog);
        self.open = self.waiting.pop_front().map(|next| (next, now));
        closed
    }

    /// Drops every conflict question of `job` (the job ended or was
    /// cancelled, nobody is waiting for the answer any more). Returns
    /// whether the open dialog changed.
    pub(crate) fn discard_conflicts_of(&mut self, job: u64, now: Instant) -> bool {
        self.waiting
            .retain(|dialog| dialog.conflict_job() != Some(job));
        let open_is_it = self
            .current()
            .is_some_and(|dialog| dialog.conflict_job() == Some(job));
        if open_is_it {
            self.close_at(now);
        }
        open_is_it
    }
}

/// The buttons of a dialog. A confirmation has Cancel and its danger
/// button; a conflict question has all four.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DialogChoice {
    Cancel,
    /// The danger button: Delete permanently, Empty Trash, Replace.
    Confirm,
    Skip,
    KeepBoth,
}

impl DialogChoice {
    /// The button that destroys something and is therefore armed late and
    /// never focused first.
    pub(crate) fn is_danger(self) -> bool {
        self == DialogChoice::Confirm
    }
}

/// The button a confirmation opens focused on: the safe one. Enter presses
/// the focused button, so with this default Enter alone never confirms a
/// deletion; only a deliberate move of the focus does.
pub(crate) const DIALOG_DEFAULT: DialogChoice = DialogChoice::Cancel;

/// The button a conflict question opens focused on: Skip touches nothing.
/// Replace is never the default.
pub(crate) const CONFLICT_DEFAULT: DialogChoice = DialogChoice::Skip;

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
        DialogKind::Conflict { name, folder, .. } => (
            format!("\u{201c}{name}\u{201d} already exists in {folder}"),
            "Replace it, skip this item, or keep both?".to_owned(),
            "Replace",
        ),
        DialogKind::OpenWith { file, mime, other } => (
            format!("Open \u{201c}{file}\u{201d} with\u{2026}"),
            if *other {
                "Every application. Type to filter, Up and Down choose, Enter opens.".to_owned()
            } else {
                format!("Applications for {mime}. Up and Down choose, Enter opens.")
            },
            "Open",
        ),
        DialogKind::DeviceBusy {
            label,
            count,
            eject,
        } => {
            let gerund = if *eject { "Ejecting" } else { "Unmounting" };
            let (are, them) = if *count == 1 {
                ("1 operation is", "it")
            } else {
                (&*format!("{count} operations are"), "them")
            };
            (
                format!("{are} using {label}"),
                format!("{gerund} now could corrupt data. Wait for {them} to finish, or cancel."),
                if *eject {
                    "Wait and eject"
                } else {
                    "Wait and unmount"
                },
            )
        }
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
    let names = match kind {
        DialogKind::Conflict {
            existing, incoming, ..
        } => vec![
            format!("Existing  {existing}"),
            format!("Incoming  {incoming}"),
        ],
        // Every application is a row of its own: none is folded away.
        DialogKind::OpenWith { .. } => names.to_vec(),
        _ => listed(names),
    };
    DialogText {
        title,
        body,
        names,
        confirm,
    }
}

// --- open with ----------------------------------------------------------------------

fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// The selected row after moving by `delta` in a list of `len` rows; it
/// stops at both ends.
pub(crate) fn move_selection(selected: usize, len: usize, delta: isize) -> usize {
    selected
        .saturating_add_signed(delta)
        .min(len.saturating_sub(1))
}

/// A program to start, detached, in `cwd`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OpenWithCommand {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<OsString>,
    /// The folder of the file.
    pub(crate) cwd: PathBuf,
}

/// What running `app` on `path` starts: its `Exec=` line expanded for the
/// file, in the file's folder.
pub(crate) fn open_with_command(path: &Path, app: &App) -> Result<OpenWithCommand, String> {
    let argv = expand_exec(&app.exec, path)
        .map_err(|error| format!("Could not open with {}: {error}", app.name))?;
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| format!("Could not open with {}: nothing to run", app.name))?;
    Ok(OpenWithCommand {
        program: PathBuf::from(program),
        args: args.to_vec(),
        cwd: path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf),
    })
}

#[cfg(test)]
#[path = "ops_tests.rs"]
mod tests;
