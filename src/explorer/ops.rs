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

/// One reversible operation, with what undoing it needs to know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UndoAction {
    /// Undo: rename `new` back to `old`'s name.
    Rename { old: PathBuf, new: PathBuf },
    /// Undo: restore the items to where they came from.
    Trash { items: Vec<TrashedItem> },
    /// Undo: move the (still empty) new item to the Trash.
    Create { path: PathBuf, kind: NewKind },
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
                return Report {
                    select: vec![path.clone()],
                    begin_rename: true,
                    undo: Some(UndoAction::Create { path, kind }),
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
            select: vec![new.clone()],
            undo: Some(UndoAction::Rename {
                old: path.to_path_buf(),
                new,
            }),
            ..Report::default()
        },
        Err(error) if error.is_already_exists() => {
            Report::error(format!("{} already exists here", quote(new_name)))
        }
        Err(error) => Report::error(format!("Could not rename {}: {error}", quote_path(path))),
    }
}

/// The "N could not be …" tail and details shared by the item loops.
fn failure_line(path: &Path, error: impl std::fmt::Display) -> String {
    format!("{}: {error}", quote_path(path))
}

fn trash_items(paths: &[PathBuf], ctx: &JobCtx, sink: &Sink) -> Report {
    sink.set_total(paths.len() as u64);
    let now = chrono::Local::now().naive_local();
    let mut items: Vec<TrashedItem> = Vec::new();
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
        match (ctx.target_for)(path).and_then(|target| trash::put(path, &target, now)) {
            Ok(item) => items.push(item),
            Err(TrashError::NoTrash(_)) => needs_permanent.push(path.clone()),
            Err(error) => details.push(failure_line(path, error)),
        }
    }
    sink.advance(paths.len() as u64, "");

    let moved = items.len();
    let notice = if moved > 0 {
        let first = items
            .first()
            .and_then(|item| item.original.file_name())
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
    Report {
        notice,
        undo: (!items.is_empty()).then_some(UndoAction::Trash { items }),
        needs_permanent,
        details,
        cancelled,
        ..Report::default()
    }
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
/// touched.
fn undo(action: &UndoAction, ctx: &JobCtx, sink: &Sink) -> Result<Report, String> {
    match action {
        UndoAction::Rename { old, new } => undo_rename(old, new),
        UndoAction::Trash { items } => undo_trash(items, sink),
        UndoAction::Create { path, kind } => undo_create(path, *kind, ctx),
    }
}

fn presence(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", quote_path(path))),
    }
}

fn undo_rename(old: &Path, new: &Path) -> Result<Report, String> {
    if presence(new)?.is_none() {
        return Err(format!("{} no longer exists", quote_path(new)));
    }
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

fn undo_trash(items: &[TrashedItem], sink: &Sink) -> Result<Report, String> {
    let mut restored = Vec::new();
    let mut problems = Vec::new();
    sink.set_total(items.len() as u64);
    for (index, item) in items.iter().enumerate() {
        sink.advance(
            index as u64,
            &item.original.file_name().map_or_else(
                || item.name.to_string_lossy().into_owned(),
                |name| name.to_string_lossy().into_owned(),
            ),
        );
        let original = item
            .original
            .file_name()
            .map(quote)
            .unwrap_or_else(|| quote(&item.name));
        if presence(&item.files_path())?.is_none() {
            problems.push(format!("{original} is no longer in the Trash"));
            continue;
        }
        // `Fail`: if the original path was taken since, nothing is replaced
        // and nothing is renamed behind the user's back.
        match trash::restore(item, RestoreConflict::Fail) {
            Ok(dest) => restored.push(dest),
            Err(error) if error.is_already_exists() => {
                problems.push(format!("{original} already exists"));
            }
            Err(error) => problems.push(format!("{original}: {error}")),
        }
    }
    if restored.is_empty() {
        return Err(problems.join("; "));
    }
    let done = match restored.as_slice() {
        [only] => format!("Restored {}", quote_path(only)),
        many => format!("Restored {}", plural_items(many.len())),
    };
    let mut report = if problems.is_empty() {
        Report::say(Severity::Success, done)
    } else {
        Report::say(
            Severity::Warning,
            format!("{done}; {} could not be restored", problems.len()),
        )
    };
    report.details = problems;
    report.select = restored;
    Ok(report)
}

fn undo_create(path: &Path, kind: NewKind, ctx: &JobCtx) -> Result<Report, String> {
    let Some(meta) = presence(path)? else {
        return Err(format!("{} no longer exists", quote_path(path)));
    };
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
    let typed = typed.trim();
    if typed.is_empty() {
        return RenameCheck::Invalid("The name can't be empty.".to_owned());
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
