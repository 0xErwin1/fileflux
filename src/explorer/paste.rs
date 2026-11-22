//! Clipboard, paste and the undo of a paste.
//!
//! A paste is a [`PasteJob`]: `core::ops::transfer` run off the UI thread
//! through the byte-based progress card. When the target already exists the
//! worker blocks and asks the UI over a channel ([`ConflictLink`]); the UI
//! answers from a modal that goes through the dialog queue. Everything the
//! UI needs to decide (what Paste offers, where it pastes, the texts, what
//! an undo would do) is a pure function here.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, TimeZone};
use fileflux::core::clipboard::{
    ClipboardAction, ClipboardFiles, format_plain_paths, parse_clipboard_files,
};
use fileflux::core::ops::{
    self as core_ops, Conflict, ItemOutcome, Mode, TransferError, TransferReport, rename_noreplace,
};
use fileflux::core::places::format_bytes;
use fileflux::core::trash::{self, TrashError};
use gpui::{ClipboardItem, FileOperation};
use rustix::io::Errno;

use super::{
    DialogChoice, DialogKind, Identity, JobCtx, Notice, Report, Severity, Sink, Step, UndoAction,
    failure_line, items_label, lowercase_first, plural_items, presence, quote, quote_path,
    same_item,
};
use crate::explorer::util::format_modified;

/// How often a worker waiting for an answer checks whether it was cancelled.
const ANSWER_POLL: Duration = Duration::from_millis(100);

/// What the clipboard will do with its paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClipMode {
    Copy,
    Cut,
}

impl ClipMode {
    pub(crate) fn transfer_mode(self) -> Mode {
        match self {
            ClipMode::Copy => Mode::Copy,
            ClipMode::Cut => Mode::Move,
        }
    }

    /// "Copied" / "Moved".
    fn past(self) -> &'static str {
        match self {
            ClipMode::Copy => "Copied",
            ClipMode::Cut => "Moved",
        }
    }

    /// "copy" / "move".
    fn bare(self) -> &'static str {
        match self {
            ClipMode::Copy => "copy",
            ClipMode::Cut => "move",
        }
    }

    /// "Copying" / "Moving".
    fn progressive(self) -> &'static str {
        match self {
            ClipMode::Copy => "Copying",
            ClipMode::Cut => "Moving",
        }
    }
}

/// What was copied or cut.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Clipboard {
    pub(crate) mode: ClipMode,
    pub(crate) paths: Vec<PathBuf>,
    /// Which Copy/Cut put it there: a paste that finishes later only
    /// clears the clipboard it started from.
    pub(crate) generation: u64,
}

/// The app-internal clipboard.
#[derive(Debug, Default)]
pub(crate) struct ClipboardState {
    current: Option<Clipboard>,
    last_generation: u64,
}

impl ClipboardState {
    /// Replaces the clipboard; returns its generation.
    pub(crate) fn set(&mut self, mode: ClipMode, paths: Vec<PathBuf>) -> u64 {
        self.last_generation += 1;
        self.current = Some(Clipboard {
            mode,
            paths,
            generation: self.last_generation,
        });
        self.last_generation
    }

    pub(crate) fn get(&self) -> Option<&Clipboard> {
        self.current.as_ref()
    }

    pub(crate) fn clear(&mut self) {
        self.current = None;
    }

    /// The paths a Cut waits to move, to draw dimmed.
    pub(crate) fn cut_paths(&self) -> BTreeSet<PathBuf> {
        match &self.current {
            Some(clip) if clip.mode == ClipMode::Cut => clip.paths.iter().cloned().collect(),
            _ => BTreeSet::new(),
        }
    }

    /// A cut-paste that started from `generation` finished. Completed: the
    /// clipboard is empty (the usual cut-and-paste). Cancelled: what moved
    /// leaves it and the rest keeps waiting. A clipboard that changed since
    /// is not touched.
    pub(crate) fn finish_cut(&mut self, generation: u64, moved: &[PathBuf], cancelled: bool) {
        let Some(clip) = self.current.as_mut() else {
            return;
        };
        if clip.generation != generation || clip.mode != ClipMode::Cut {
            return;
        }
        if cancelled {
            clip.paths.retain(|path| !moved.contains(path));
        }
        if !cancelled || clip.paths.is_empty() {
            self.current = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn last_generation(&self) -> u64 {
        self.last_generation
    }
}

/// Where a paste was asked to go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PasteInto {
    /// Ctrl+V and the menu's "Paste": the folder that is shown.
    CurrentFolder,
    /// The menu's "Paste into folder" on a single selected folder.
    SelectedFolder,
}

/// The folder a paste goes into. `selected` is the visible selection with
/// whether each entry is a real folder. `None`: "into the selected folder"
/// without exactly one selected folder.
pub(crate) fn paste_target(
    into: PasteInto,
    current: &Path,
    selected: &[(PathBuf, bool)],
) -> Option<PathBuf> {
    match into {
        PasteInto::CurrentFolder => Some(current.to_path_buf()),
        PasteInto::SelectedFolder => match selected {
            [(folder, true)] => Some(folder.clone()),
            _ => None,
        },
    }
}

/// How a folder reads in a message: its name, or the path for the root.
fn folder_label(dir: &Path) -> String {
    dir.file_name().map_or_else(
        || dir.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// What to do before starting a paste job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PastePlan {
    Nothing(Notice),
    Run,
}

/// Decides whether a paste has anything to do. Moving items into the folder
/// they are already in changes nothing, and says so instead of starting a
/// job; everything else runs (a copy into its own folder keeps both).
pub(crate) fn plan_paste(mode: ClipMode, sources: &[PathBuf], dest: &Path) -> PastePlan {
    let info = |text: String| {
        PastePlan::Nothing(Notice {
            severity: Severity::Info,
            text,
        })
    };
    if sources.is_empty() {
        return info("Nothing to paste".to_owned());
    }
    if mode == ClipMode::Cut && sources.iter().all(|source| source.parent() == Some(dest)) {
        return info(format!(
            "Already in {} \u{2014} nothing to move",
            folder_label(dest)
        ));
    }
    PastePlan::Run
}

/// "Copying 12 items to Pictures" / "Moving \u{201c}a.txt\u{201d} to Pictures".
pub(crate) fn paste_title(mode: ClipMode, sources: &[PathBuf], dest: &Path) -> String {
    let what = match sources {
        [one] => quote_path(one),
        many => plural_items(many.len()),
    };
    format!("{} {what} to {}", mode.progressive(), folder_label(dest))
}

/// The text that goes next to the files on the system clipboard: the paths,
/// one per line, so pasting into a terminal gives paths.
pub(crate) fn clipboard_text(paths: &[PathBuf]) -> String {
    format_plain_paths(paths)
}

/// The gpui file operation a [`ClipMode`] is offered as.
pub(crate) fn file_operation(mode: ClipMode) -> FileOperation {
    match mode {
        ClipMode::Copy => FileOperation::Copy,
        ClipMode::Cut => FileOperation::Cut,
    }
}

/// The [`ClipMode`] a gpui file operation asks for.
pub(crate) fn clip_mode(operation: FileOperation) -> ClipMode {
    match operation {
        FileOperation::Copy => ClipMode::Copy,
        FileOperation::Cut => ClipMode::Cut,
    }
}

/// What Copy and Cut put on the system clipboard: the files with their
/// operation, which gpui offers as `x-special/gnome-copied-files`,
/// `text/uri-list` and plain text (see [`clipboard_text`]).
pub(crate) fn clipboard_item(mode: ClipMode, paths: &[PathBuf]) -> ClipboardItem {
    ClipboardItem::new_files(paths.iter().cloned(), file_operation(mode))
}

/// Whether the system clipboard no longer holds what Copy/Cut wrote. An
/// unreadable clipboard says nothing; line endings and a trailing newline
/// are not a change.
pub(crate) fn clipboard_is_stale(expected: &str, system: Option<&str>) -> bool {
    fn normal(text: &str) -> String {
        text.replace("\r\n", "\n").trim_end_matches('\n').to_owned()
    }
    system.is_some_and(|system| normal(system) != normal(expected))
}

/// What Paste does about the system clipboard.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SystemClipboard {
    /// The internal clipboard stands: the system one is unreadable or still
    /// holds what Copy/Cut wrote, or it changed to something that is not files
    /// and there was no internal clipboard to drop.
    Keep,
    /// Something else was copied since: the internal clipboard goes.
    Forget,
    /// Another application copied or cut files: they replace the internal
    /// clipboard.
    Adopt(ClipboardFiles),
}

/// Decides what the system clipboard `system` means given the text `written`
/// and the `mode` that Copy/Cut last put there (`None` when the internal
/// clipboard is empty). The system clipboard wins when it changed since
/// Fileflux wrote it. Files the clipboard carries (what a file manager
/// offers) come first, with their copy or cut operation; otherwise its text
/// is read as a file list.
pub(crate) fn system_clipboard_action(
    written: Option<&str>,
    mode: Option<ClipMode>,
    system: Option<&ClipboardItem>,
    exists: impl Fn(&Path) -> bool,
) -> SystemClipboard {
    let Some(system) = system else {
        return SystemClipboard::Keep;
    };
    let text = system.text();
    let operation = system.file_operation().map(clip_mode);
    // Our own files, with the operation we wrote, are not a change; the same
    // paths cut by someone else are.
    let ours = operation.is_none() || operation == mode;
    if ours
        && let (Some(written), Some(text)) = (written, text.as_deref())
        && !clipboard_is_stale(written, Some(text))
    {
        return SystemClipboard::Keep;
    }
    if text.is_none() {
        return SystemClipboard::Keep;
    }
    let files = match (system.file_paths(), operation) {
        (Some(paths), Some(mode)) => {
            let paths: Vec<PathBuf> = paths.into_iter().filter(|path| exists(path)).collect();
            (!paths.is_empty()).then_some(ClipboardFiles {
                action: match mode {
                    ClipMode::Copy => ClipboardAction::Copy,
                    ClipMode::Cut => ClipboardAction::Cut,
                },
                paths,
            })
        }
        _ => text
            .as_deref()
            .and_then(|text| parse_clipboard_files(text, exists)),
    };
    match files {
        Some(files) => SystemClipboard::Adopt(files),
        None if written.is_some() => SystemClipboard::Forget,
        None => SystemClipboard::Keep,
    }
}

/// The status line after Copy/Cut.
pub(crate) fn clip_notice(mode: ClipMode, paths: &[PathBuf]) -> String {
    let what = match paths {
        [one] => quote_path(one),
        many => plural_items(many.len()),
    };
    match (mode, paths.len()) {
        (ClipMode::Copy, _) => format!("Copied {what} to the clipboard"),
        (ClipMode::Cut, 1) => format!("Cut {what}; Paste moves it"),
        (ClipMode::Cut, _) => format!("Cut {what}; Paste moves them"),
    }
}

/// The answer to a conflict question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConflictAnswer {
    pub(crate) choice: Conflict,
    /// "Apply to all remaining conflicts".
    pub(crate) apply_all: bool,
}

/// The answer a dialog button gives.
pub(crate) fn answer_for(choice: DialogChoice, apply_all: bool) -> ConflictAnswer {
    ConflictAnswer {
        choice: match choice {
            DialogChoice::Cancel => Conflict::Cancel,
            DialogChoice::Confirm => Conflict::Replace,
            DialogChoice::Skip => Conflict::Skip,
            DialogChoice::KeepBoth => Conflict::KeepBoth,
        },
        apply_all,
    }
}

/// Remembers "apply to all": once the user ticked it with a real answer,
/// the rest of the job is not asked again.
#[derive(Debug, Default)]
pub(crate) struct ConflictPolicy {
    remembered: Option<Conflict>,
}

impl ConflictPolicy {
    /// The decision for one conflict. `ask` is only called while nothing is
    /// remembered; no answer (the UI went away) cancels rather than guesses.
    pub(crate) fn resolve(&mut self, ask: impl FnOnce() -> Option<ConflictAnswer>) -> Conflict {
        if let Some(choice) = self.remembered {
            return choice;
        }
        match ask() {
            None => Conflict::Cancel,
            Some(answer) => {
                if answer.apply_all && answer.choice != Conflict::Cancel {
                    self.remembered = Some(answer.choice);
                }
                answer.choice
            }
        }
    }
}

/// What the dialog shows about one of the two items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ItemFacts {
    pub(crate) is_dir: bool,
    pub(crate) size: u64,
    pub(crate) modified: Option<SystemTime>,
}

impl ItemFacts {
    /// What `path` is right now (a link is itself, never its target).
    fn of(path: &Path) -> Option<ItemFacts> {
        let meta = fs::symlink_metadata(path).ok()?;
        Some(ItemFacts {
            is_dir: meta.is_dir(),
            size: meta.len(),
            modified: meta.modified().ok(),
        })
    }
}

/// "12.0 KB \u{b7} modified 3 Nov"; a folder shows "Folder" (summing its
/// size would mean walking it before the user could answer).
pub(crate) fn describe_item<Tz: TimeZone>(facts: &ItemFacts, now: &DateTime<Tz>) -> String {
    let size = if facts.is_dir {
        "Folder".to_owned()
    } else {
        format_bytes(facts.size)
    };
    format!(
        "{size} \u{b7} modified {}",
        format_modified(facts.modified, now)
    )
}

/// The question a blocked worker sends to the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConflictRequest {
    pub(crate) kind: DialogKind,
}

/// The worker's two ends of the conversation with the UI.
#[derive(Debug)]
pub(crate) struct ConflictLink {
    pub(crate) requests: Sender<ConflictRequest>,
    pub(crate) replies: Receiver<ConflictAnswer>,
}

impl ConflictLink {
    /// Asks the UI what to do about `target` and blocks for the answer. A
    /// cancelled job, or a UI that went away, answers `None`.
    fn ask(
        &self,
        sink: &Sink,
        sources: &[PathBuf],
        target: &Path,
        folder: &str,
    ) -> Option<ConflictAnswer> {
        let name = target.file_name()?;
        let incoming = sources
            .iter()
            .find(|source| source.file_name() == Some(name));
        let now = chrono::Local::now();
        let describe = |path: Option<&Path>| {
            path.and_then(ItemFacts::of).map_or_else(
                || "\u{2014}".to_owned(),
                |facts| describe_item(&facts, &now),
            )
        };
        let request = ConflictRequest {
            kind: DialogKind::Conflict {
                name: name.to_string_lossy().into_owned(),
                folder: folder.to_owned(),
                existing: describe(Some(target)),
                incoming: describe(incoming.map(PathBuf::as_path)),
            },
        };
        self.requests.send(request).ok()?;
        loop {
            match self.replies.recv_timeout(ANSWER_POLL) {
                Ok(answer) => return Some(answer),
                Err(RecvTimeoutError::Timeout) if sink.cancelled() => return None,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }
}

/// One paste, owned by the worker.
#[derive(Debug)]
pub(crate) struct PasteJob {
    pub(crate) sources: Vec<PathBuf>,
    pub(crate) dest: PathBuf,
    pub(crate) mode: ClipMode,
    pub(crate) link: ConflictLink,
}

/// An item a paste placed: where it came from, where it is now and which
/// file it was, for the undo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlacedEntry {
    pub(crate) from: PathBuf,
    pub(crate) to: PathBuf,
    pub(crate) identity: Identity,
}

/// Runs a paste: the core transfer with byte progress, and the conflict
/// questions routed to the UI.
pub(crate) fn run_paste(job: PasteJob, sink: &Sink) -> Report {
    let PasteJob {
        sources,
        dest,
        mode,
        link,
    } = job;
    let folder = folder_label(&dest);
    let mut policy = ConflictPolicy::default();
    let mut replaced: Vec<PathBuf> = Vec::new();
    let mut decide = |target: &Path| {
        let choice = policy.resolve(|| link.ask(sink, &sources, target, &folder));
        if choice == Conflict::Replace {
            replaced.push(target.to_path_buf());
        }
        choice
    };
    let transferred = core_ops::transfer_pausable(
        &sources,
        &dest,
        mode.transfer_mode(),
        &mut decide,
        |progress| {
            sink.set_total(progress.bytes_total);
            let name = progress.current.file_name().map_or_else(
                || progress.current.to_string_lossy().into_owned(),
                |name| name.to_string_lossy().into_owned(),
            );
            sink.advance(progress.bytes_done, &name);
        },
        sink.cancel_flag(),
        sink.pause_gate(),
    );

    let (notice, details) = transfer_summary(mode, &dest, &transferred, &replaced);
    let undo = undo_plan(mode, &transferred, &replaced, |path| {
        Identity::of(path).ok()
    });
    let placed = |item: &core_ops::ItemReport| match &item.outcome {
        ItemOutcome::Ok { dest, .. } => Some((item.source.clone(), dest.clone())),
        _ => None,
    };
    let (moved, select): (Vec<PathBuf>, Vec<PathBuf>) =
        transferred.items.iter().filter_map(placed).unzip();
    Report {
        notice: Some(notice),
        details,
        undo,
        select,
        moved: if mode == ClipMode::Cut {
            moved
        } else {
            Vec::new()
        },
        cancelled: transferred.cancelled,
        ..Report::default()
    }
}

/// The toast and its details for a finished transfer.
pub(crate) fn transfer_summary(
    mode: ClipMode,
    dest: &Path,
    report: &TransferReport,
    replaced: &[PathBuf],
) -> (Notice, Vec<String>) {
    let mut done: Vec<&Path> = Vec::new();
    let mut replaced_ok = 0usize;
    let mut skipped = 0usize;
    let mut failures: Vec<(&Path, &TransferError)> = Vec::new();
    let mut details = Vec::new();
    for item in &report.items {
        match &item.outcome {
            ItemOutcome::Ok { dest, .. } => {
                done.push(&item.source);
                if replaced.contains(dest) {
                    replaced_ok += 1;
                }
            }
            ItemOutcome::Skipped => {
                skipped += 1;
                details.push(format!("{}: skipped", quote_path(&item.source)));
            }
            // What the user cancelled is not a failure.
            ItemOutcome::Failed(TransferError::Cancelled) => {}
            ItemOutcome::Failed(error) => {
                failures.push((&item.source, error));
                details.push(failure_line(&item.source, error));
            }
        }
    }
    let failed = failures.len();
    let place = folder_label(dest);
    let skipped_tail = if skipped > 0 {
        format!("; {skipped} skipped")
    } else {
        String::new()
    };

    let (severity, text) = if !done.is_empty() {
        let what = match done.as_slice() {
            [one] => quote_path(one),
            many => plural_items(many.len()),
        };
        let mut text = format!("{} {what} to {place}", mode.past());
        if report.cancelled {
            text = format!("Cancelled: {}", lowercase_first(&text));
        }
        text.push_str(&skipped_tail);
        if failed > 0 {
            text.push_str(&format!("; {failed} failed"));
        }
        if replaced_ok > 0 {
            text.push_str(&format!(
                "; {replaced_ok} replaced (not restorable by Undo)"
            ));
        }
        let severity = if failed > 0 {
            Severity::Warning
        } else if report.cancelled {
            Severity::Info
        } else {
            Severity::Success
        };
        (severity, text)
    } else if report.cancelled {
        (
            Severity::Info,
            format!("Cancelled: nothing was {}", mode.past().to_lowercase()),
        )
    } else if let [(source, error)] = failures.as_slice() {
        (
            Severity::Error,
            format!(
                "Could not {} {} \u{2014} {error}{skipped_tail}",
                mode.bare(),
                quote_path(source)
            ),
        )
    } else if failed > 0 {
        (
            Severity::Error,
            format!(
                "Could not {} {}{skipped_tail}",
                mode.bare(),
                items_label(failed)
            ),
        )
    } else {
        (
            Severity::Info,
            format!("Nothing was {}{skipped_tail}", mode.past().to_lowercase()),
        )
    };
    let (severity, text) = with_leftovers(report, severity, text, &mut details);
    details.extend(report.notes.iter().cloned());
    (Notice { severity, text }, details)
}

/// A Replace whose old original could not be deleted afterwards still left
/// data on disk (hidden): say so instead of reporting a clean success.
fn with_leftovers(
    report: &TransferReport,
    severity: Severity,
    mut text: String,
    details: &mut Vec<String>,
) -> (Severity, String) {
    if report.leftovers.is_empty() {
        return (severity, text);
    }
    let count = report.leftovers.len();
    text.push_str(&format!(
        "; {count} replaced original{} could not be removed",
        if count == 1 { "" } else { "s" }
    ));
    for (path, error) in &report.leftovers {
        details.push(format!(
            "{}: the replaced original is still there ({error})",
            quote_path(path)
        ));
    }
    let severity = if severity == Severity::Error {
        severity
    } else {
        Severity::Warning
    };
    (severity, text)
}

/// What undoing a finished transfer would do. An item that replaced
/// something is never part of it (the replaced one cannot come back, so
/// moving the new one away would only lose more), and neither is one whose
/// identity could not be read.
pub(crate) fn undo_plan(
    mode: ClipMode,
    report: &TransferReport,
    replaced: &[PathBuf],
    identity: impl Fn(&Path) -> Option<Identity>,
) -> Option<UndoAction> {
    let items: Vec<PlacedEntry> = report
        .items
        .iter()
        .filter_map(|item| match &item.outcome {
            ItemOutcome::Ok { dest, .. } if !replaced.contains(dest) => Some(PlacedEntry {
                from: item.source.clone(),
                to: dest.clone(),
                identity: identity(dest)?,
            }),
            _ => None,
        })
        .collect();
    if items.is_empty() {
        return None;
    }
    Some(match mode {
        ClipMode::Copy => UndoAction::Copy { items },
        ClipMode::Cut => UndoAction::Move { items },
    })
}

// --- undo ---------------------------------------------------------------------------------

/// What an undo says about its items.
struct Wording {
    /// "Moved 2 copies to Trash" for what was undone.
    done: fn(&[PathBuf]) -> String,
    /// What follows "Cancelled: nothing was ".
    nothing: &'static str,
    /// Whether the undone items are selected afterwards.
    select: bool,
}

/// Undoes item by item: a problem with one never stops the others, and the
/// cancel flag is asked before each. What was not undone and still can be
/// stays undoable through the report's undo.
fn run_steps(
    items: &[PlacedEntry],
    sink: &Sink,
    wording: &Wording,
    rebuild: fn(Vec<PlacedEntry>) -> UndoAction,
    mut step: impl FnMut(&PlacedEntry) -> Step,
) -> Report {
    sink.set_total(items.len() as u64);
    let mut done: Vec<PathBuf> = Vec::new();
    let mut remaining: Vec<PlacedEntry> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let (mut skipped, mut failed) = (0usize, 0usize);
    let mut cancelled = false;
    for (index, entry) in items.iter().enumerate() {
        if sink.cancelled() {
            cancelled = true;
            let rest = items.get(index..).unwrap_or_default();
            skipped += rest.len();
            remaining.extend(rest.iter().cloned());
            break;
        }
        sink.advance(
            index as u64,
            &entry.to.file_name().map_or_else(
                || entry.to.to_string_lossy().into_owned(),
                |name| name.to_string_lossy().into_owned(),
            ),
        );
        match step(entry) {
            Step::Restored(path) => done.push(path),
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
    sink.advance(items.len() as u64, "");

    let mut report = if done.is_empty() {
        if cancelled {
            Report::say(
                if problems.is_empty() {
                    Severity::Info
                } else {
                    Severity::Warning
                },
                format!("Cancelled: nothing was {}", wording.nothing),
            )
        } else {
            Report::error(format!("Can't undo: {}", problems.join("; ")))
        }
    } else {
        let said = (wording.done)(&done);
        let mut text = if cancelled {
            format!("Cancelled: {}", lowercase_first(&said))
        } else {
            said
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
    };
    if !done.is_empty() || cancelled {
        report.details = problems;
    }
    if wording.select {
        report.select = done;
    }
    report.cancelled = cancelled;
    report.undo = (!remaining.is_empty()).then(|| rebuild(remaining));
    report
}

/// Undo of a copy: the created copies go to the Trash (never deleted), each
/// only if it is still the file that was created.
pub(crate) fn undo_copy(items: &[PlacedEntry], ctx: &JobCtx, sink: &Sink) -> Report {
    let wording = Wording {
        done: |done| match done {
            [one] => format!("Moved the copy {} to Trash", quote_path(one)),
            many => format!("Moved {} copies to Trash", many.len()),
        },
        nothing: "moved to the Trash",
        select: false,
    };
    run_steps(
        items,
        sink,
        &wording,
        |items| UndoAction::Copy { items },
        |entry| trash_copy(entry, ctx),
    )
}

fn trash_copy(entry: &PlacedEntry, ctx: &JobCtx) -> Step {
    if let Err(reason) = same_item(&entry.to, entry.identity, "copied") {
        return Step::Gone(reason);
    }
    let target = match (ctx.target_for)(&entry.to) {
        Ok(target) => target,
        Err(TrashError::NoTrash(_)) => {
            return Step::Gone(format!("there is no Trash for {}", quote_path(&entry.to)));
        }
        Err(error) => return Step::Failed(format!("{}: {error}", quote_path(&entry.to))),
    };
    match trash::put(&entry.to, &target, chrono::Local::now().naive_local()) {
        Ok(_) => Step::Restored(entry.to.clone()),
        Err(error) => Step::Failed(format!("{}: {error}", quote_path(&entry.to))),
    }
}

/// Undo of a move: every item goes back to where it came from, each only if
/// it is still the item that was moved, and never replacing anything.
pub(crate) fn undo_move(items: &[PlacedEntry], sink: &Sink) -> Report {
    let wording = Wording {
        done: |done| match done {
            [one] => format!("Moved {} back", quote_path(one)),
            many => format!("Moved {} back", plural_items(many.len())),
        },
        nothing: "moved back",
        select: true,
    };
    run_steps(
        items,
        sink,
        &wording,
        |items| UndoAction::Move { items },
        return_one,
    )
}

fn return_one(entry: &PlacedEntry) -> Step {
    let name = quote_path(&entry.from);
    if let Err(reason) = same_item(&entry.to, entry.identity, "moved") {
        return Step::Gone(reason);
    }
    let taken = || Step::Blocked(format!("{name} already exists where it came from"));
    match presence(&entry.from) {
        Ok(Some(_)) => return taken(),
        Ok(None) => {}
        Err(reason) => return Step::Failed(reason),
    }
    let Some(parent) = entry.from.parent() else {
        return Step::Gone(format!("{name}: its original place is unknown"));
    };
    match fs::metadata(parent) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Step::Blocked(format!("{name}: its original folder is not a folder")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Step::Blocked(format!("{name}: its original folder no longer exists"));
        }
        Err(error) => return Step::Failed(format!("{name}: {error}")),
    }
    match rename_noreplace(&entry.to, &entry.from) {
        Ok(()) => Step::Restored(entry.from.clone()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => taken(),
        Err(error) if error.raw_os_error() == Some(Errno::XDEV.raw_os_error()) => {
            return_across_devices(entry, &name)
        }
        Err(error) => Step::Failed(format!("{name}: {error}")),
    }
}

/// The item went to another device, so it comes back the way it left: the
/// core transfer copies, verifies and only then removes it. A different item
/// under the same name in the original folder is never replaced.
fn return_across_devices(entry: &PlacedEntry, name: &str) -> Step {
    let Some(parent) = entry.from.parent() else {
        return Step::Gone(format!("{name}: its original place is unknown"));
    };
    let never = AtomicBool::new(false);
    let report = core_ops::transfer(
        std::slice::from_ref(&entry.to),
        parent,
        Mode::Move,
        |_| Conflict::Skip,
        |_| {},
        &never,
    );
    match report.items.into_iter().next().map(|item| item.outcome) {
        Some(ItemOutcome::Ok { dest, .. }) if dest == entry.from => Step::Restored(dest),
        // It came back under another name (the moved item had been given a
        // numbered one): put the original name on it, still never replacing.
        Some(ItemOutcome::Ok { dest, .. }) => match rename_noreplace(&dest, &entry.from) {
            Ok(()) => Step::Restored(entry.from.clone()),
            Err(error) => Step::Failed(format!(
                "{name} came back as {} but could not be renamed: {error}",
                quote(dest.file_name().unwrap_or_default())
            )),
        },
        Some(ItemOutcome::Skipped) => Step::Blocked(format!(
            "an item named {} is already in the folder it came from",
            quote(entry.to.file_name().unwrap_or_default())
        )),
        Some(ItemOutcome::Failed(error)) => Step::Failed(format!("{name}: {error}")),
        None => Step::Failed(format!("{name}: nothing was returned")),
    }
}

#[cfg(test)]
#[path = "paste_tests.rs"]
mod tests;
