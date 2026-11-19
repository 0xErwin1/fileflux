//! Copy, Cut and Paste on the UI side.
//!
//! Copy and Cut keep the paths in the app (see [`ClipboardState`]) and also
//! write them, one per line, to the system clipboard, so pasting into a
//! terminal gives paths. Paste runs `core::ops::transfer` off the UI thread
//! as a byte-based operation behind the progress card. When the worker meets
//! an existing target it blocks and sends a question over a channel; the
//! pump below turns it into a modal that waits its turn in the dialog queue
//! and sends the answer back.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use fileflux::core::Location;
use fileflux::core::ops::Conflict;
use gpui::{ClipboardItem, Context, Task, Window};

use super::devices::job_paths;
use super::ops::paste::{
    ClipMode, ConflictAnswer, ConflictLink, ConflictRequest, PasteInto, PasteJob, PastePlan,
    clip_notice, clipboard_is_stale, clipboard_text, paste_target, paste_title, plan_paste,
};
use super::ops::{DialogState, Job, Report, Severity};
use super::progress::Unit;
use super::tabs::Affected;
use super::view::Explorer;

/// How often the UI looks for a question from a running paste.
const PUMP_EVERY: Duration = Duration::from_millis(50);

/// The UI's end of a running paste.
pub(crate) struct PasteHandle {
    pub(crate) mode: ClipMode,
    /// The clipboard this paste started from; a drop has none.
    pub(crate) generation: Option<u64>,
    requests: Receiver<ConflictRequest>,
    replies: Sender<ConflictAnswer>,
    /// Looks for questions while the paste runs; dropped with the handle.
    _pump: Task<()>,
}

impl Explorer {
    /// The visible selection with whether each entry is a real folder (a
    /// link to one is not: pasting "into" it would be a surprise).
    pub(crate) fn selected_with_kind(&self) -> Vec<(std::path::PathBuf, bool)> {
        let selected = self.selected_visible_paths();
        self.tab()
            .snapshot
            .iter()
            .filter(|entry| selected.contains(&entry.path))
            .map(|entry| {
                (
                    entry.path.clone(),
                    entry.is_directory() && !entry.is_symlink,
                )
            })
            .collect()
    }

    /// Ctrl+C / Ctrl+X: remembers the selection, and writes its paths to
    /// the system clipboard as text.
    pub(crate) fn copy_selection(&mut self, mode: ClipMode, cx: &mut Context<Self>) {
        if self.in_trash() {
            return;
        }
        let paths = self.selected_visible_paths();
        if paths.is_empty() {
            return;
        }
        let text = clipboard_text(&paths);
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        self.tab_mut().state.set_notice(clip_notice(mode, &paths));
        self.clipboard.set(mode, paths);
        self.clipboard_text = Some(text);
        cx.notify();
    }

    /// Forgets the internal clipboard when something else was put on the
    /// system clipboard since: "the clipboard changed".
    fn reconcile_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(expected) = self.clipboard_text.as_deref() else {
            return;
        };
        let system = cx.read_from_clipboard().and_then(|item| item.text());
        if clipboard_is_stale(expected, system.as_deref()) {
            self.clipboard.clear();
            self.clipboard_text = None;
            cx.notify();
        }
    }

    /// Whether Paste has something to paste.
    pub(crate) fn can_paste(&mut self, cx: &mut Context<Self>) -> bool {
        self.reconcile_clipboard(cx);
        !self.in_trash()
            && *self.tab().state.current() != Location::Recent
            && self.clipboard.get().is_some()
    }

    /// Ctrl+V and the menu's Paste / Paste into folder.
    pub(crate) fn paste(&mut self, into: PasteInto, window: &mut Window, cx: &mut Context<Self>) {
        self.reconcile_clipboard(cx);
        let Some(clip) = self.clipboard.get().cloned() else {
            self.say(Severity::Info, "Nothing to paste", window, cx);
            return;
        };
        let Location::Directory(current) = self.tab().state.current().clone() else {
            let reason = if self.tab().state.current() == &Location::Recent {
                "Can't paste into Recent. Open a folder to paste into"
            } else {
                "Can't paste into the Trash. Restore items instead"
            };
            self.say(Severity::Info, reason, window, cx);
            return;
        };
        if clip.mode == ClipMode::Cut
            && self
                .pastes
                .values()
                .any(|running| running.generation == Some(clip.generation))
        {
            self.say(
                Severity::Info,
                "These items are already being moved",
                window,
                cx,
            );
            return;
        }
        let selected = self.selected_with_kind();
        let Some(dest) = paste_target(into, &current, &selected) else {
            self.say(
                Severity::Info,
                "Select exactly one folder to paste into it",
                window,
                cx,
            );
            return;
        };
        self.start_paste(
            clip.paths,
            dest,
            clip.mode,
            Some(clip.generation),
            window,
            cx,
        );
    }

    /// Starts a transfer of `sources` into `dest`: what Ctrl+V does with the
    /// clipboard and what a drop does with the dragged entries. The one
    /// path for both: the conflict dialog, the progress card and the undo
    /// all come from here. `generation` is the clipboard a cut came from.
    pub(crate) fn start_paste(
        &mut self,
        sources: Vec<std::path::PathBuf>,
        dest: std::path::PathBuf,
        mode: ClipMode,
        generation: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Nothing is read from or written to a device that is being ejected.
        let mut touched = sources.clone();
        touched.push(dest.clone());
        self.when_not_ejecting(touched, window, cx, move |this, window, cx| {
            this.run_paste(sources, dest, mode, generation, window, cx);
        });
    }

    fn run_paste(
        &mut self,
        sources: Vec<std::path::PathBuf>,
        dest: std::path::PathBuf,
        mode: ClipMode,
        generation: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match plan_paste(mode, &sources, &dest) {
            PastePlan::Nothing(notice) => {
                self.push_toast(notice, &[], None, window, cx);
            }
            PastePlan::Run => {
                let title = paste_title(mode, &sources, &dest);
                let (request_tx, request_rx) = channel();
                let (reply_tx, reply_rx) = channel();
                let job = Job::Paste(PasteJob {
                    sources,
                    dest,
                    mode,
                    link: ConflictLink {
                        requests: request_tx,
                        replies: reply_rx,
                    },
                });
                let ctx = self.job_ctx();
                let touches = job_paths(&job, &ctx.trash_root);
                let id = self.start_work(
                    title,
                    Unit::Bytes,
                    true,
                    (Affected::Everything, touches),
                    move |sink| super::ops::run_job(job, &ctx, &sink),
                    window,
                    cx,
                );
                let pump = cx.spawn_in(window, async move |this, cx| {
                    loop {
                        cx.background_executor().timer(PUMP_EVERY).await;
                        let keep_going = this
                            .update_in(cx, |this, window, cx| this.pump_questions(id, window, cx))
                            .unwrap_or(false);
                        if !keep_going {
                            break;
                        }
                    }
                });
                self.pastes.insert(
                    id,
                    PasteHandle {
                        mode,
                        generation,
                        requests: request_rx,
                        replies: reply_tx,
                        _pump: pump,
                    },
                );
            }
        }
    }

    /// Turns the questions a paste's worker sent into dialogs. `false` once
    /// the paste is over.
    fn pump_questions(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(handle) = self.pastes.get(&id) else {
            return false;
        };
        let questions: Vec<ConflictRequest> = handle.requests.try_iter().collect();
        for question in questions {
            self.offer_dialog(DialogState::conflict(id, question), window, cx);
        }
        true
    }

    /// Hands the user's answer to the waiting worker. Cancel also stops the
    /// job, so nothing after the question runs.
    pub(crate) fn answer_conflict(
        &mut self,
        job: u64,
        answer: ConflictAnswer,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(handle) = self.pastes.get(&job)
            && handle.replies.send(answer).is_err()
        {
            log::debug!("the paste ended before its question was answered");
        }
        if answer.choice == Conflict::Cancel {
            self.progress.cancel(job);
        }
        cx.notify();
    }

    /// A paste ended: its channel goes, a cut that finished empties the
    /// clipboard it started from, and a question nobody waits for is dropped.
    pub(crate) fn finish_paste(
        &mut self,
        id: u64,
        report: &Report,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(paste) = self.pastes.remove(&id) else {
            return;
        };
        if paste.mode == ClipMode::Cut
            && let Some(generation) = paste.generation
        {
            self.clipboard
                .finish_cut(generation, &report.moved, report.cancelled);
            if self.clipboard.get().is_none() {
                self.clipboard_text = None;
            }
        }
        if self.dialogs.discard_conflicts_of(id, Instant::now()) {
            self.dialog_closed(window, cx);
        }
    }
}
