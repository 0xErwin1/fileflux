//! The explorer's file operations on the UI side: starting jobs off the UI
//! thread, the inline rename, the context menu, the confirmation dialogs,
//! undo, opening files and the terminal, and the toasts that report it all.
//!
//! The engine and its safety rules live in [`super::ops`]; this module only
//! decides *when* to run what, keeps the transient UI state (rename editor,
//! menu, dialog) and applies a finished [`Report`]: record the undo entry,
//! show the summary toast, queue the selection, and re-read the shown
//! location so the listing reflects what happened.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use bolt::controls::{InputEvent, InputState};
use bolt::toast::{Toast, ToastAction};
use fileflux::core::Location;
use gpui::{
    AnyWindowHandle, AppContext as _, ClipboardItem, Context, Entity, Pixels, Point, Subscription,
    Window,
};

use fileflux::core::apps::{self, App, OpenWith};
use fileflux::core::images::is_image_path;

use super::arrange::ArrangeCommand;
use super::arrange_menu::page_rows;
use super::devices::{device_menu_rows, job_paths, with_device_rows};
use super::ops::paste::{ClipMode, PasteInto, answer_for};
use super::ops::{
    ARM_DELAY, DialogAction, DialogChoice, DialogKind, DialogState, Job, JobCtx, MenuCommand,
    MenuPage, MenuRow, MenuTarget, NewKind, Notice, OpenTarget, RenameCheck, RenameStep, Report,
    Severity, Unavailable, UndoAction, UndoEntry, add_viewer_row, check_rename, dismissed_notice,
    menu_rows, move_selection, next_menu_index, no_trash_dialog, open_target, open_with_command,
    rename_seed, rename_step, stem_range,
};
use super::ops::{SidebarTarget, menu_title, recent_menu_rows, sidebar_menu_rows};
use super::progress::{Sink, Unit};
use super::state::Transition;
use super::tabs::{Affected, Placement, tabs_to_refresh};
use super::toasts::{make_room, toast_timeout};
use super::view::{Explorer, OpContext, TabId};

/// The inline rename in progress.
pub(crate) struct Renaming {
    pub(crate) path: PathBuf,
    /// The text the editor was seeded with: committing it unchanged renames
    /// nothing, even for a name that is not valid UTF-8.
    seed: String,
    /// The entry's real name, to tell "unchanged" from "renamed".
    original: OsString,
    pub(crate) input: Entity<InputState>,
    /// Why the typed name was refused; shown under the field.
    pub(crate) error: Option<String>,
    _events: Subscription,
}

/// What the entry views need to draw the rename editor.
#[derive(Clone, PartialEq)]
pub(crate) struct RenameView {
    pub(crate) path: PathBuf,
    pub(crate) input: Entity<InputState>,
    pub(crate) error: Option<String>,
}

impl Renaming {
    pub(crate) fn view(&self) -> RenameView {
        RenameView {
            path: self.path.clone(),
            input: self.input.clone(),
            error: self.error.clone(),
        }
    }
}

/// What an open context menu was opened on, so the thing it acts on can be
/// outlined for as long as the menu is up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MenuOn {
    /// A toolbar button's Sort or Filter menu: nothing to outline.
    Nothing,
    /// The selected entries of the shown folder.
    Entries,
    /// The empty space of the entries area: the folder itself.
    EmptySpace,
    /// One sidebar row, by its element id, and the place it stands for; for
    /// a mounted device also its block device (`/dev/sdb1`).
    Sidebar {
        row: String,
        location: Location,
        device: Option<PathBuf>,
    },
    /// The row of a device that is not mounted, by its element id and block
    /// device.
    Device { row: String, device: PathBuf },
}

/// The open context menu.
pub(crate) struct MenuState {
    /// What the menu acts on.
    pub(crate) on: MenuOn,
    /// The header naming what the menu acts on, when it has one.
    pub(crate) title: Option<String>,
    pub(crate) position: Point<Pixels>,
    pub(crate) rows: Vec<MenuRow>,
    /// The highlighted row (keyboard or hover).
    pub(crate) selected: Option<usize>,
    /// Which page the rows are.
    pub(crate) page: MenuPage,
    /// The context menu's rows while a Sort or Filter page drilled into it,
    /// restored by "Back".
    pub(crate) parent: Option<Vec<MenuRow>>,
}

/// What to select (and whether to start renaming) once the listing that
/// shows the result arrives.
pub(crate) struct PendingSelect {
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) rename: bool,
}

fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

fn quoted(path: &Path) -> String {
    format!("\u{201c}{}\u{201d}", display_name(path))
}

fn count_label(count: usize) -> String {
    format!("{count} items")
}

impl Explorer {
    pub(crate) fn in_trash(&self) -> bool {
        *self.tab().state.current() == Location::Trash
    }

    /// The selected entries that are on screen under the current filter.
    /// Operations act on these only: a selection the filter hides is never
    /// trashed or deleted behind the user's back.
    pub(crate) fn selected_visible_paths(&self) -> Vec<PathBuf> {
        self.tab()
            .state
            .visible_selected()
            .into_iter()
            .map(|entry| entry.path.clone())
            .collect()
    }

    // --- jobs ------------------------------------------------------------------

    pub(crate) fn job_ctx(&self) -> JobCtx {
        JobCtx::for_places(self.places.as_ref(), self.trash_root.clone())
    }

    /// Runs `job` on the background executor and applies its report when it
    /// finishes. The operation registers for the progress card (shown only
    /// when it takes a while) and the status-bar indicator.
    pub(crate) fn start_job(
        &mut self,
        title: impl Into<String>,
        job: Job,
        cancellable: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ctx = self.job_ctx();
        let affected = Affected::of_job(&job);
        let touches = job_paths(&job, &ctx.trash_root);
        let title = title.into();
        // Nothing starts on a device that is being ejected.
        self.when_not_ejecting(touches.clone(), window, cx, move |this, window, cx| {
            this.start_work(
                title,
                Unit::Items,
                cancellable,
                (affected, touches),
                move |sink| super::ops::run_job(job, &ctx, &sink),
                window,
                cx,
            );
        });
    }

    /// Runs `work` on the background executor with the progress sink of a
    /// new registered operation, and applies its [`Report`] when it ends.
    /// Returns the operation's id.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_work(
        &mut self,
        title: impl Into<String>,
        unit: Unit,
        cancellable: bool,
        (affected, touches): (Affected, Vec<PathBuf>),
        work: impl FnOnce(Sink) -> Report + Send + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> u64 {
        let (id, sink) = self.progress.start(title, unit, cancellable);
        // The result belongs to the tab it was started from, and refreshes
        // every tab showing a folder it changed, wherever the user is by then.
        self.op_contexts.insert(
            id,
            OpContext {
                origin: self.active_id(),
                affected,
                touches,
            },
        );
        let work = cx.background_executor().spawn(async move { work(sink) });

        // Repaints the card while the operation runs; dropped with it.
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let running = this
                    .update(cx, |this, cx| {
                        let running = this.progress.is_running(id);
                        if running {
                            cx.notify();
                        }
                        running
                    })
                    .unwrap_or(false);
                if !running {
                    break;
                }
            }
        });
        self.progress.attach_ticker(id, ticker);

        cx.spawn_in(window, async move |this, cx| {
            let report = work.await;
            if let Err(error) = this.update_in(cx, |this, window, cx| {
                this.finish_job(id, report, window, cx);
            }) {
                log::debug!("the explorer closed before an operation finished: {error}");
            }
        })
        .detach();
        cx.notify();
        id
    }

    /// Asks a running operation to stop (a card's Cancel button). A conflict
    /// question it is waiting on is withdrawn: nobody is listening for the
    /// answer any more.
    pub(crate) fn cancel_operation(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.progress.cancel(id);
        if self.dialogs.discard_conflicts_of(id, Instant::now()) {
            self.dialog_closed(window, cx);
        }
        cx.notify();
    }

    /// Applies a finished job: undo entry, toast, confirmation for what
    /// had no Trash, selection, and a re-read of the shown location.
    fn finish_job(
        &mut self,
        id: u64,
        mut report: Report,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.progress.finish(id);
        self.finish_paste(id, &report, window, cx);
        let undo_id = report.undo.take().map(|action| self.undo.push(action));
        if let Some(notice) = report.notice.take() {
            let toast = self.push_toast(notice, &report.details, undo_id, window, cx);
            if let (Some(undo_id), Some(toast)) = (undo_id, toast) {
                self.undo.attach_toast(undo_id, toast);
            }
        }
        // A cancelled job already said what it did not delete; otherwise
        // the follow-up dialog queues behind any dialog that is open now.
        if !report.needs_permanent.is_empty() && !report.cancelled {
            let dialog = no_trash_dialog(std::mem::take(&mut report.needs_permanent));
            self.offer_dialog(dialog, window, cx);
        }
        let context = self.op_contexts.remove(&id);
        let origin = context.as_ref().map(|context| context.origin);
        if !report.select.is_empty()
            && let Some(tab) = origin.and_then(|origin| self.tabs.by_id_mut(origin))
        {
            tab.pending = Some(PendingSelect {
                paths: std::mem::take(&mut report.select),
                rename: report.begin_rename,
            });
        }
        // The watcher would catch up within a moment; this makes our own
        // result show right away, and covers the Trash, which is not
        // watched.
        self.refresh_after_operation(context, window, cx);
        cx.notify();
    }

    /// Re-reads every tab showing a folder the finished operation changed,
    /// and always the tab it was started from, which waits for its result.
    fn refresh_after_operation(
        &mut self,
        context: Option<OpContext>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let affected = context
            .as_ref()
            .map_or(Affected::Everything, |c| c.affected.clone());
        let origin = context.and_then(|context| self.tabs.position(|tab| tab.id == context.origin));
        let shown: Vec<&Location> = self.tabs.iter().map(|tab| tab.state.current()).collect();
        let targets = tabs_to_refresh(&shown, &affected, origin);
        let ids: Vec<TabId> = targets
            .into_iter()
            .filter_map(|index| self.tabs.get(index).map(|tab| tab.id))
            .collect();
        log::debug!("operation finished: re-reading tabs {ids:?} ({affected:?})");
        for id in ids {
            let scan = self
                .tabs
                .by_id_mut(id)
                .and_then(|tab| tab.state.begin_op_refresh());
            self.start_scan_in(id, scan, window, cx);
            // Results of a search are looked up again, not re-read.
            self.restart_search_in(id, cx);
        }
    }

    /// Shows a result as a toast: severity-toned, with the failures as
    /// "Details" and, when undoable, an Undo button that undoes exactly
    /// this operation.
    pub(crate) fn push_toast(
        &self,
        notice: Notice,
        details: &[String],
        undo_id: Option<u64>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let severity = notice.severity;
        let mut toast = match severity {
            Severity::Success => Toast::success(notice.text),
            Severity::Info => Toast::info(notice.text),
            Severity::Warning => Toast::warning(notice.text),
            Severity::Error => Toast::error(notice.text),
        }
        .auto_dismiss_after(toast_timeout(severity, undo_id.is_some()));
        if !details.is_empty() {
            toast = toast.details(details.join("\n")).collapsible();
        }
        if let Some(id) = undo_id {
            let weak = cx.entity().downgrade();
            let handle: AnyWindowHandle = window.window_handle();
            toast = toast.action(
                ToastAction::new(format!("explorer-undo-{id}"), "Undo")
                    .primary()
                    .dismisses()
                    .key_hint("ctrl-z")
                    .on_click(move |cx| {
                        let weak = weak.clone();
                        // The click runs inside the toast's own update: leave
                        // it before touching the window again.
                        cx.defer(move |cx| {
                            let result = handle.update(cx, |_root, window, cx| {
                                if let Err(error) = weak.update(cx, |this, cx| {
                                    this.undo_entry(id, window, cx);
                                }) {
                                    log::debug!("undo after the explorer closed: {error}");
                                }
                            });
                            if let Err(error) = result {
                                log::debug!("undo after the window closed: {error}");
                            }
                        });
                    }),
            );
        }
        // Ids only grow, so a different newest id is this toast; the same
        // one means the host dropped it (throttled).
        let mut open = self.toast_ids.borrow_mut();
        let host = self.toast_host.clone();
        open.retain(|(id, _)| host.read(cx).toast_controls(*id).is_some());
        for id in make_room(&open) {
            host.update(cx, |host, cx| host.dismiss(id, cx));
            open.retain(|(known, _)| *known != id);
        }
        let before = host.read(cx).latest_toast_id();
        toast.push(cx);
        let after = host.read(cx).latest_toast_id();
        if after == before {
            return None;
        }
        if let Some(id) = after {
            open.push((id, severity));
        }
        after
    }

    /// A plain message toast.
    pub(crate) fn say(
        &self,
        severity: Severity,
        text: impl Into<String>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let notice = Notice {
            severity,
            text: text.into(),
        };
        self.push_toast(notice, &[], None, window, cx);
    }

    /// After tab `id`'s listing changed: drop a rename whose entry vanished
    /// (the rename belongs to the active tab), then select what the last
    /// operation produced there (and start renaming it, when the tab is the
    /// one in front).
    pub(crate) fn after_listing_changed(
        &mut self,
        id: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active = id == self.active_id();
        if active
            && let Some(renaming) = &self.renaming
            && !self
                .tabs
                .active()
                .state
                .listing()
                .entries
                .iter()
                .any(|entry| entry.path == renaming.path)
        {
            self.renaming = None;
        }
        let Some(tab) = self.tabs.by_id_mut(id) else {
            return;
        };
        let Some(pending) = tab.pending.take() else {
            return;
        };
        let present: Vec<PathBuf> = pending
            .paths
            .into_iter()
            .filter(|path| tab.state.visible_position_of(path).is_some())
            .collect();
        let Some((first, rest)) = present.split_first() else {
            return;
        };
        tab.state.select_only(first);
        for path in rest {
            tab.state.toggle(path);
        }
        let position = tab.state.visible_position_of(first);
        if !active {
            return;
        }
        if let Some(position) = position {
            self.scroll_selected_row(position, cx);
        }
        if pending.rename {
            self.start_rename(window, cx);
        }
    }

    // --- create ------------------------------------------------------------------

    /// New folder / New file in the shown folder, then rename it in place.
    pub(crate) fn create_new(
        &mut self,
        kind: NewKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Location::Directory(dir) = self.tab().state.current().clone() else {
            return;
        };
        // A filter could hide the new entry; the new item must be visible
        // to be renamed.
        if self.tab().state.is_filtering() {
            self.tab_mut().state.clear_query();
            self.tab_mut().state.arrange(ArrangeCommand::ClearFilters);
            let id = self.active_id();
            self.arrange_tab(id, cx);
            self.tab()
                .search_input
                .update(cx, |input, cx| input.set_value(String::new(), window, cx));
        }
        let title = match kind {
            NewKind::Folder => "Creating folder",
            NewKind::File => "Creating file",
        };
        self.start_job(title, Job::Create { dir, kind }, false, window, cx);
    }

    // --- inline rename ---------------------------------------------------------------

    /// F2: renames the single selected entry in place.
    pub(crate) fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.in_trash() || self.renaming.is_some() {
            return;
        }
        let selected = self.selected_visible_paths();
        let [path] = selected.as_slice() else {
            return;
        };
        let Some(entry) = self
            .tab()
            .snapshot
            .iter()
            .find(|entry| entry.path == *path)
            .cloned()
        else {
            return;
        };
        let is_dir = entry.is_directory() && !entry.is_symlink;
        let seed = match rename_seed(&entry.name) {
            Ok(seed) => seed,
            Err(message) => {
                self.say(Severity::Warning, message, window, cx);
                return;
            }
        };
        let range = stem_range(&seed, is_dir);

        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |input, cx| input.set_value(seed.clone(), window, cx));
        let events = cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| {
                // Only the live editor may act: a committed one is gone.
                if !this
                    .renaming
                    .as_ref()
                    .is_some_and(|renaming| renaming.input == *input)
                {
                    return;
                }
                match event {
                    InputEvent::PressEnter { .. } => this.commit_rename(false, window, cx),
                    InputEvent::Blur => this.commit_rename(true, window, cx),
                    InputEvent::Change => {
                        if let Some(renaming) = this.renaming.as_mut() {
                            renaming.error = None;
                        }
                        cx.notify();
                    }
                    InputEvent::Focus => {}
                }
            },
        );
        self.renaming = Some(Renaming {
            path: entry.path.clone(),
            seed,
            original: entry.name.clone(),
            input: input.clone(),
            error: None,
            _events: events,
        });
        cx.defer_in(window, move |_this, window, cx| {
            input.update(cx, |input, cx| {
                input.focus(window, cx);
                input.set_selected_range(range, cx);
            });
        });
        cx.notify();
    }

    /// Enter commits and keeps the editor open on an invalid name; blur
    /// commits too, but a name it cannot accept cancels the rename with a
    /// message instead of leaving an editor nobody is typing in.
    pub(crate) fn commit_rename(
        &mut self,
        from_blur: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(renaming) = self.renaming.as_ref() else {
            return;
        };
        let typed = renaming.input.read(cx).value().to_string();
        // Exactly as typed: no trimming, so "unchanged" and the new name
        // are judged on the same string.
        let check = if typed == renaming.seed {
            RenameCheck::Unchanged
        } else {
            check_rename(
                &renaming.original,
                &typed,
                self.tab()
                    .state
                    .listing()
                    .entries
                    .iter()
                    .map(|entry| entry.name.as_os_str()),
            )
        };
        match rename_step(check, from_blur) {
            RenameStep::Close => self.finish_rename(from_blur, window, cx),
            RenameStep::Cancel(message) => {
                self.finish_rename(true, window, cx);
                self.say(Severity::Warning, message, window, cx);
            }
            RenameStep::KeepOpen(message) => {
                if let Some(renaming) = self.renaming.as_mut() {
                    renaming.error = Some(message);
                }
                cx.notify();
            }
            RenameStep::Run(new_name) => {
                let path = renaming.path.clone();
                self.finish_rename(from_blur, window, cx);
                let title = format!("Renaming {}", quoted(&path));
                self.start_job(title, Job::Rename { path, new_name }, false, window, cx);
            }
        }
    }

    /// Closes the editor; the keyboard goes back to the entries unless the
    /// editor closed because the focus already went elsewhere.
    fn finish_rename(&mut self, from_blur: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.renaming = None;
        if !from_blur {
            self.entries_focus.focus(window, cx);
        }
        cx.notify();
    }

    /// Escape in the editor: the name stays as it was.
    pub(crate) fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.renaming.is_some() {
            self.finish_rename(false, window, cx);
        }
    }

    // --- trash, delete, restore ------------------------------------------------------

    /// Del: moves the selection to the Trash (in the Trash view, asks to
    /// delete it for good instead).
    pub(crate) fn trash_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.in_trash() {
            self.delete_selected(window, cx);
            return;
        }
        let paths = self.selected_visible_paths();
        self.trash_paths(paths, window, cx);
    }

    /// Moves `paths` to the Trash through the undoable trash job: Del on the
    /// selection and a drop on the sidebar's Trash row.
    pub(crate) fn trash_paths(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = match paths.as_slice() {
            [] => return,
            [one] => format!("Moving {} to Trash", quoted(one)),
            many => format!("Moving {} to Trash", count_label(many.len())),
        };
        self.start_job(title, Job::Trash(paths), true, window, cx);
    }

    /// Shift+Del: asks before deleting the selection for good.
    pub(crate) fn delete_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = self.selected_visible_paths();
        if paths.is_empty() {
            return;
        }
        if self.in_trash() {
            let names: Vec<String> = self
                .tab()
                .snapshot
                .iter()
                .filter(|entry| paths.contains(&entry.path))
                .map(|entry| entry.display_name().into_owned())
                .collect();
            let trash_names: Vec<OsString> = paths
                .iter()
                .filter_map(|path| path.file_name().map(OsString::from))
                .collect();
            self.open_dialog(
                DialogKind::DeleteFromTrash,
                &names,
                DialogAction::DeleteFromTrash(trash_names),
                window,
                cx,
            );
        } else {
            let names: Vec<String> = paths.iter().map(|path| display_name(path)).collect();
            self.open_dialog(
                DialogKind::DeletePermanently,
                &names,
                DialogAction::DeletePermanently(paths),
                window,
                cx,
            );
        }
    }

    /// Trash view: puts the selection back where it came from.
    pub(crate) fn restore_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.in_trash() {
            return;
        }
        let names: Vec<OsString> = self
            .selected_visible_paths()
            .iter()
            .filter_map(|path| path.file_name().map(OsString::from))
            .collect();
        let title = match names.as_slice() {
            [] => return,
            [one] => format!("Restoring \u{201c}{}\u{201d}", one.to_string_lossy()),
            many => format!("Restoring {}", count_label(many.len())),
        };
        self.start_job(title, Job::Restore(names), true, window, cx);
    }

    /// Trash view: asks before emptying the Trash.
    pub(crate) fn request_empty_trash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.in_trash() {
            return;
        }
        let names: Vec<OsString> = self
            .tab()
            .state
            .listing()
            .entries
            .iter()
            .filter_map(|entry| entry.path.file_name().map(OsString::from))
            .collect();
        if names.is_empty() {
            return;
        }
        self.open_dialog(
            DialogKind::EmptyTrash { count: names.len() },
            &[],
            DialogAction::EmptyTrash(names),
            window,
            cx,
        );
    }

    // --- dialogs ---------------------------------------------------------------------

    pub(crate) fn open_dialog(
        &mut self,
        kind: DialogKind,
        names: &[String],
        action: DialogAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.offer_dialog(DialogState::new(kind, names, action), window, cx);
    }

    /// Opens `dialog`, or queues it behind the one that is open: nothing
    /// that finishes in the background may replace a confirmation the user
    /// is reading.
    pub(crate) fn offer_dialog(
        &mut self,
        dialog: DialogState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_menu(window, cx);
        self.dialogs.offer(dialog);
        self.focus_dialog_default(window, cx);
        cx.notify();
    }

    /// The safe button is the default: Enter alone never deletes. The queue
    /// decides which one, for a dialog that opened at once and for one that
    /// was promoted from the line alike. Also repaints once the dialog's
    /// danger button arms.
    fn focus_dialog_default(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(choice) = self.dialogs.default_focus() else {
            return;
        };
        cx.defer_in(window, move |this, window, cx| {
            this.dialog_focus_handle(choice).focus(window, cx);
        });
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(ARM_DELAY + Duration::from_millis(30))
                .await;
            if let Err(error) = this.update(cx, |_, cx| cx.notify()) {
                log::debug!("the explorer closed before its dialog armed: {error}");
            }
        })
        .detach();
    }

    fn dialog_focus_handle(&self, choice: DialogChoice) -> &gpui::FocusHandle {
        match choice {
            DialogChoice::Cancel => &self.dialog_cancel_focus,
            DialogChoice::Confirm => &self.dialog_confirm_focus,
            DialogChoice::Skip => &self.dialog_skip_focus,
            DialogChoice::KeepBoth => &self.dialog_keep_focus,
        }
    }

    /// After a dialog closed: the next one in line takes the keyboard at
    /// its safe button, else the entries get it back.
    pub(crate) fn dialog_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialogs.is_open() {
            self.focus_dialog_default(window, cx);
        } else {
            self.entries_focus.focus(window, cx);
        }
        cx.notify();
    }

    /// A press of one of the dialog's two buttons.
    pub(crate) fn choose_dialog(
        &mut self,
        choice: DialogChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match choice {
            DialogChoice::Cancel => self.cancel_dialog(window, cx),
            DialogChoice::Confirm => self.confirm_dialog(window, cx),
            DialogChoice::Skip | DialogChoice::KeepBoth => {
                self.answer_open_conflict(choice, window, cx);
            }
        }
    }

    /// The checkbox "Apply to all remaining conflicts" of the open question.
    pub(crate) fn set_apply_all(&mut self, checked: bool, cx: &mut Context<Self>) {
        if let Some(DialogState {
            action: DialogAction::Conflict { apply_all, .. },
            ..
        }) = self.dialogs.current_mut()
        {
            *apply_all = checked;
            cx.notify();
        }
    }

    /// Skip or Keep both: closes the open conflict question and answers the
    /// waiting paste. A dialog that is not a conflict question ignores them.
    fn answer_open_conflict(
        &mut self,
        choice: DialogChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(DialogAction::Conflict { job, apply_all }) =
            self.dialogs.current().map(|dialog| dialog.action.clone())
        else {
            return;
        };
        self.dialogs.close();
        self.dialog_closed(window, cx);
        self.answer_conflict(job, answer_for(choice, apply_all), window, cx);
    }

    /// The danger button: runs what the open dialog asked for, bound to the
    /// paths that dialog showed.
    pub(crate) fn confirm_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A dialog that has just appeared ignores its danger button: the
        // trailing click of a double-click on the dialog before it must not
        // confirm this one.
        if !self.dialogs.accepts(DialogChoice::Confirm, Instant::now()) {
            return;
        }
        let Some(dialog) = self.dialogs.close() else {
            return;
        };
        self.dialog_closed(window, cx);
        let (title, job) = match dialog.action {
            DialogAction::DeletePermanently(paths) => (
                match paths.as_slice() {
                    [one] => format!("Deleting {}", quoted(one)),
                    many => format!("Deleting {}", count_label(many.len())),
                },
                Job::DeletePermanently(paths),
            ),
            DialogAction::DeleteFromTrash(names) => (
                format!("Deleting {} from the Trash", count_label(names.len())),
                Job::DeleteFromTrash(names),
            ),
            DialogAction::EmptyTrash(names) => {
                ("Emptying the Trash".to_owned(), Job::EmptyTrash(names))
            }
            DialogAction::OpenWith {
                path,
                apps,
                selected,
            } => {
                if let Some(app) = apps.get(selected) {
                    self.run_open_with(&path, app, cx);
                }
                return;
            }
            DialogAction::DeviceWhenIdle(plan) => {
                self.queue_device_plan(plan, window, cx);
                return;
            }
            DialogAction::Conflict { job, apply_all } => {
                // Replace: the one destructive answer.
                self.answer_conflict(
                    job,
                    answer_for(DialogChoice::Confirm, apply_all),
                    window,
                    cx,
                );
                return;
            }
        };
        self.start_job(title, job, true, window, cx);
        cx.notify();
    }

    /// Cancel, Escape or a click on the scrim: nothing is deleted. What the
    /// dialog was about to delete for lack of a Trash is reported, not
    /// silently dropped.
    pub(crate) fn cancel_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.dialogs.close() else {
            return;
        };
        self.dialog_closed(window, cx);
        if let DialogAction::Conflict { job, apply_all } = dialog.action {
            // Escape or Cancel on a conflict question cancels the whole job.
            self.answer_conflict(job, answer_for(DialogChoice::Cancel, apply_all), window, cx);
            return;
        }
        if let Some(notice) = dismissed_notice(&dialog) {
            self.push_toast(notice, &[], None, window, cx);
        }
    }

    // --- undo ------------------------------------------------------------------------

    /// Ctrl+Z: undoes the newest operation of the session.
    pub(crate) fn undo_last(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.undo.newest().map(|entry| entry.id) {
            Some(id) => self.undo_entry(id, window, cx),
            None => self.say(Severity::Info, "Nothing to undo", window, cx),
        }
    }

    /// A toast's Undo button: undoes exactly that operation. The entry
    /// leaves the history only once the final device check lets the undo
    /// start: an undo refused because its device is being ejected stays
    /// undoable, and while its check is pending no other press can start
    /// the same entry.
    pub(crate) fn undo_entry(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.undo.get(id) else {
            return self.say(Severity::Info, "Already undone", window, cx);
        };
        let touches = job_paths(&Job::Undo(entry.action.clone()), &self.job_ctx().trash_root);
        match self.undo.begin(id) {
            Ok(()) => {}
            Err(Unavailable::Gone) => {
                return self.say(Severity::Info, "Already undone", window, cx);
            }
            Err(Unavailable::InFlight) => {
                return self.say(Severity::Info, "Already undoing that", window, cx);
            }
        }
        // With no device going away the check answers at once, in this call.
        self.check_not_ejecting(touches, window, cx, move |this, refused, window, cx| {
            match this.undo.settle(id, refused.is_none()) {
                Some(entry) => {
                    // Its toast's Undo button would now do nothing: close it.
                    if let Some(toast) = entry.toast {
                        this.toast_host
                            .update(cx, |host, cx| host.dismiss(toast, cx));
                    }
                    this.run_undo(entry, window, cx);
                }
                None => match refused {
                    Some(text) => this.say(Severity::Info, text, window, cx),
                    None => this.say(Severity::Info, "Already undone", window, cx),
                },
            }
        });
    }

    /// Starts the undo of an entry that has passed the device check: the
    /// job is not gated a second time, because a refusal now would drop an
    /// entry that already left the history.
    fn run_undo(&mut self, entry: UndoEntry, window: &mut Window, cx: &mut Context<Self>) {
        // Restoring several items takes a while: it can be cancelled, and
        // what was not restored stays undoable.
        let cancellable = !matches!(
            entry.action,
            UndoAction::Rename { .. } | UndoAction::Create { .. }
        );
        let job = Job::Undo(entry.action);
        let ctx = self.job_ctx();
        let affected = Affected::of_job(&job);
        let touches = job_paths(&job, &ctx.trash_root);
        self.start_work(
            "Undoing",
            Unit::Items,
            cancellable,
            (affected, touches),
            move |sink| super::ops::run_job(job, &ctx, &sink),
            window,
            cx,
        );
    }

    // --- context menu ----------------------------------------------------------------

    /// Opens the menu at `position`. `on` is the entry under the pointer: it
    /// becomes the selection unless it already is part of it; `None` is the
    /// empty space, which clears the selection.
    pub(crate) fn open_context_menu(
        &mut self,
        position: Point<Pixels>,
        on: Option<&Path>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_rename(window, cx);
        match on {
            Some(path) if !self.tab().state.selection().contains(path) => {
                self.tab_mut().state.select_only(path)
            }
            Some(_) => {}
            None => self.tab_mut().state.clear_selection(),
        }
        let selected = self.selected_with_kind();
        let target = MenuTarget {
            in_trash: self.in_trash(),
            selected: selected.len(),
            folder_is_empty: self.tab().state.listing().entries.is_empty(),
            terminal: self.terminal.is_some(),
            can_paste: self.can_paste(cx),
            folder_selected: matches!(selected.as_slice(), [(_, true)]),
            single_file: matches!(selected.as_slice(), [(_, false)]),
            folder_favorite: matches!(
                selected.as_slice(),
                [(path, true)] if self.favorites.contains(&Location::Directory(path.clone()))
            ),
        };
        let mut rows = menu_rows(&target);
        if self.tab().state.current().lists_files_from_elsewhere() {
            rows = recent_menu_rows(rows, target.selected);
        }
        let names: Vec<String> = self
            .selected_visible_paths()
            .iter()
            .map(|path| display_name(path))
            .collect();
        let title = menu_title(&names, &self.current_label());
        let menu_on = if on.is_some() {
            MenuOn::Entries
        } else {
            MenuOn::EmptySpace
        };
        let one_image =
            matches!(self.selected_visible_paths().as_slice(), [path] if is_image_path(path));
        if target.single_file && one_image && !target.in_trash {
            add_viewer_row(&mut rows);
        }
        if rows.is_empty() {
            cx.notify();
            return;
        }
        self.show_menu(
            position,
            rows,
            MenuPage::Context,
            (menu_on, Some(title)),
            window,
            cx,
        );
    }

    /// Opens the menu of one sidebar row, named after it, and outlines the
    /// row until the menu closes. The Trash can neither be pinned nor copied.
    pub(crate) fn open_sidebar_menu(
        &mut self,
        position: Point<Pixels>,
        row: String,
        name: String,
        (location, device): (Location, Option<PathBuf>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_rename(window, cx);
        let mut rows = sidebar_menu_rows(&SidebarTarget {
            trash: location == Location::Trash,
            recent: location == Location::Recent,
            favorite: self.favorites.contains(&location),
            terminal: self.terminal.is_some(),
        });
        if let Some(actions) = device
            .as_deref()
            .and_then(|device| self.device_actions(device))
        {
            rows = with_device_rows(rows, actions);
        }
        self.show_menu(
            position,
            rows,
            MenuPage::Context,
            (
                MenuOn::Sidebar {
                    row,
                    location,
                    device,
                },
                Some(name),
            ),
            window,
            cx,
        );
    }

    /// Opens the menu of the row of a device that is not mounted.
    pub(crate) fn open_device_menu(
        &mut self,
        position: Point<Pixels>,
        row: String,
        name: String,
        device: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_rename(window, cx);
        let actions = self.device_actions(&device).unwrap_or_default();
        self.show_menu(
            position,
            device_menu_rows(actions),
            MenuPage::Context,
            (MenuOn::Device { row, device }, Some(name)),
            window,
            cx,
        );
    }

    /// Opens the menu with `rows` and gives it the keyboard.
    fn show_menu(
        &mut self,
        position: Point<Pixels>,
        rows: Vec<MenuRow>,
        page: MenuPage,
        (on, title): (MenuOn, Option<String>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu = Some(MenuState {
            on,
            title,
            position,
            rows,
            selected: None,
            page,
            parent: None,
        });
        cx.defer_in(window, |this, window, cx| {
            if this.menu.is_some() {
                this.menu_focus.focus(window, cx);
            }
        });
        cx.notify();
    }

    /// The Sort or Filter menu a subbar button opens, on this tab's
    /// arrangement.
    pub(crate) fn open_arrange_menu(
        &mut self,
        page: MenuPage,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_rename(window, cx);
        let rows = page_rows(page, self.tab().state.arrangement(), false);
        self.show_menu(position, rows, page, (MenuOn::Nothing, None), window, cx);
    }

    /// Changes the active tab's sort or filters (menus, chips, Ctrl+H).
    /// Selection stays; the grid, list, keyboard and viewer follow the new
    /// order on the next frame.
    pub(crate) fn arrange(&mut self, command: ArrangeCommand, cx: &mut Context<Self>) {
        let hidden_before = self.tab().state.arrangement().filters.hide_hidden;
        self.tab_mut().state.arrange(command);
        let id = self.active_id();
        self.arrange_tab(id, cx);
        // A search skips hidden files while they are hidden, so showing or
        // hiding them searches again.
        if self.tab().state.arrangement().filters.hide_hidden != hidden_before {
            self.restart_search_in(id, cx);
        }
        // The open Sort/Filter menu redraws its check marks.
        let arrangement = self.tab().state.arrangement().clone();
        if let Some(menu) = self.menu.as_mut()
            && menu.page != MenuPage::Context
        {
            menu.rows = page_rows(menu.page, &arrangement, menu.parent.is_some());
        }
        cx.notify();
    }

    /// Shows `page` in the open menu: drilling into Sort/Filter from the
    /// context menu, or "Back" out of it.
    fn show_menu_page(&mut self, page: MenuPage, cx: &mut Context<Self>) {
        let arrangement = self.tab().state.arrangement().clone();
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        match page {
            MenuPage::Context => {
                let Some(rows) = menu.parent.take() else {
                    return;
                };
                menu.rows = rows;
            }
            MenuPage::Sort | MenuPage::Filter => {
                if menu.page == MenuPage::Context {
                    menu.parent = Some(std::mem::take(&mut menu.rows));
                }
                menu.rows = page_rows(page, &arrangement, menu.parent.is_some());
            }
        }
        menu.page = page;
        menu.selected = None;
        cx.notify();
    }

    pub(crate) fn close_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            self.entries_focus.focus(window, cx);
            cx.notify();
        }
    }

    /// Up/Down in the menu.
    pub(crate) fn move_menu(&mut self, forward: bool, cx: &mut Context<Self>) {
        if let Some(menu) = self.menu.as_mut() {
            menu.selected = next_menu_index(&menu.rows, menu.selected, forward);
            cx.notify();
        }
    }

    /// Enter in the menu: runs the highlighted item.
    pub(crate) fn confirm_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let command = self
            .menu
            .as_ref()
            .and_then(|menu| menu.selected.and_then(|at| menu.rows.get(at)))
            .and_then(|row| match row {
                MenuRow::Item(item) if item.enabled => Some(item.command),
                MenuRow::Item(_) | MenuRow::Header(_) | MenuRow::Separator => None,
            });
        if let Some(command) = command {
            self.run_menu_command(command, window, cx);
        }
    }

    /// Closes the menu, then does what the item says.
    pub(crate) fn run_menu_command(
        &mut self,
        command: MenuCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Sort and filter rows keep the menu open: several categories can
        // be ticked in a row, and the checks redraw.
        match command {
            MenuCommand::Arrange(change) => return self.arrange(change, cx),
            MenuCommand::Page(page) => return self.show_menu_page(page, cx),
            _ => {}
        }
        // A sidebar row's menu acts on that row's place, not on the entries.
        let (sidebar, device_row) = match self.menu.as_ref().map(|menu| &menu.on) {
            Some(MenuOn::Sidebar {
                location, device, ..
            }) => (Some((location.clone(), device.clone())), None),
            Some(MenuOn::Device { device, .. }) => (None, Some(device.clone())),
            _ => (None, None),
        };
        self.close_menu(window, cx);
        if let Some((location, device)) = sidebar {
            return self.run_sidebar_command(command, (location, device), window, cx);
        }
        if let Some(device) = device_row {
            return self.run_device_command(command, device, window, cx);
        }
        match command {
            MenuCommand::AddFavorite | MenuCommand::RemoveFavorite => {
                if let Some(folder) = self.single_selected_folder() {
                    self.set_favorite(
                        &Location::Directory(folder),
                        command == MenuCommand::AddFavorite,
                        cx,
                    );
                }
            }
            MenuCommand::Mount | MenuCommand::Unmount | MenuCommand::Eject => {}
            MenuCommand::Arrange(_) | MenuCommand::Page(_) => {}
            MenuCommand::Open => self.open_selected(window, cx),
            MenuCommand::OpenInViewer => self.open_in_viewer(window, cx),
            MenuCommand::OpenContainingFolder => self.open_containing_folder(window, cx),
            MenuCommand::OpenInNewTab => self.open_selected_in_new_tab(window, cx),
            MenuCommand::OpenWith => self.open_with(window, cx),
            MenuCommand::Rename => self.start_rename(window, cx),
            MenuCommand::CopyPath => self.copy_selected_paths(cx),
            MenuCommand::Properties => {
                self.inspector_open = !self.inspector_open;
                cx.notify();
            }
            MenuCommand::MoveToTrash => self.trash_selected(window, cx),
            MenuCommand::DeletePermanently => self.delete_selected(window, cx),
            MenuCommand::NewFolder => self.create_new(NewKind::Folder, window, cx),
            MenuCommand::NewFile => self.create_new(NewKind::File, window, cx),
            MenuCommand::OpenTerminal => {
                let folder = self.single_selected_folder();
                self.open_terminal(folder, cx);
            }
            MenuCommand::CopyFolderPath => self.copy_folder_path(window, cx),
            MenuCommand::Restore => self.restore_selected(window, cx),
            MenuCommand::EmptyTrash => self.request_empty_trash(window, cx),
            MenuCommand::Cut => self.copy_selection(ClipMode::Cut, cx),
            MenuCommand::Copy => self.copy_selection(ClipMode::Copy, cx),
            MenuCommand::Paste => self.paste(PasteInto::CurrentFolder, window, cx),
            MenuCommand::PasteIntoFolder => self.paste(PasteInto::SelectedFolder, window, cx),
        }
    }

    /// What a sidebar row's menu item does to the row's `location`, reusing
    /// the commands the toolbar, the tabs and the Trash already have.
    fn run_sidebar_command(
        &mut self,
        command: MenuCommand,
        (location, device): (Location, Option<PathBuf>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            MenuCommand::Unmount | MenuCommand::Eject => {
                if let Some(device) = device {
                    self.run_device_command(command, device, window, cx);
                }
            }
            MenuCommand::Open => {
                self.navigate(Transition::Visit(location), window, cx);
            }
            MenuCommand::OpenInNewTab => {
                self.open_tab(location, Placement::Background, window, cx);
            }
            MenuCommand::AddFavorite => self.set_favorite(&location, true, cx),
            MenuCommand::RemoveFavorite => self.set_favorite(&location, false, cx),
            MenuCommand::CopyPath => {
                if let Location::Directory(path) = &location {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        path.to_string_lossy().into_owned(),
                    ));
                    self.tab_mut()
                        .state
                        .set_notice("Copied the path".to_owned());
                    cx.notify();
                }
            }
            // Properties: show the folder, with the details panel open.
            MenuCommand::Properties => {
                self.navigate(Transition::Visit(location), window, cx);
                self.inspector_open = true;
                cx.notify();
            }
            MenuCommand::OpenTerminal => {
                if let Location::Directory(folder) = location {
                    self.open_terminal(Some(folder), cx);
                }
            }
            MenuCommand::EmptyTrash => self.request_empty_trash_anywhere(window, cx),
            _ => {}
        }
    }

    /// "Empty Trash…" from the sidebar: the Trash need not be the shown
    /// location, so its content is read off the UI thread first, then the
    /// usual confirmation opens.
    fn request_empty_trash_anywhere(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.trash_root.clone() else {
            return;
        };
        let read = cx
            .background_executor()
            .spawn(async move { fileflux::core::trash::read_trash(&root) });
        cx.spawn_in(window, async move |this, cx| {
            let listing = read.await;
            if let Err(error) = this.update_in(cx, |this, window, cx| match listing {
                Ok(listing) => {
                    let names: Vec<OsString> = listing
                        .entries
                        .iter()
                        .filter_map(|entry| entry.path.file_name().map(OsString::from))
                        .collect();
                    if names.is_empty() {
                        this.say(Severity::Info, "The Trash is already empty", window, cx);
                    } else {
                        this.open_dialog(
                            DialogKind::EmptyTrash { count: names.len() },
                            &[],
                            DialogAction::EmptyTrash(names),
                            window,
                            cx,
                        );
                    }
                }
                Err(error) => {
                    let text = format!("Could not read the Trash: {error}");
                    this.say(Severity::Error, text, window, cx);
                }
            }) {
                log::debug!("the explorer closed before the Trash was read: {error}");
            }
        })
        .detach();
    }

    // --- clipboard, open, terminal -------------------------------------------------------

    /// "Open with…": looks up the applications for the single selected
    /// file off the UI thread, then asks which one to run.
    pub(crate) fn open_with(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.in_trash() {
            return;
        }
        let selected = self.selected_visible_paths();
        let [path] = selected.as_slice() else {
            return;
        };
        let path = path.clone();
        let lookup = {
            let path = path.clone();
            cx.background_executor()
                .spawn(async move { apps::open_with_candidates(&path) })
        };
        cx.spawn_in(window, async move |this, cx| {
            let found = lookup.await;
            if let Err(error) = this.update_in(cx, |this, window, cx| {
                this.offer_open_with(&path, &found, window, cx);
            }) {
                log::debug!("the explorer closed before the applications were found: {error}");
            }
        })
        .detach();
    }

    fn offer_open_with(
        &mut self,
        path: &Path,
        found: &OpenWith,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if found.apps.is_empty() {
            let text = format!("No application found for {} ({})", quoted(path), found.mime);
            self.say(Severity::Info, text, window, cx);
            return;
        }
        self.offer_dialog(DialogState::open_with(path, found), window, cx);
    }

    /// Up / Down in the "Open with…" list.
    pub(crate) fn move_open_with(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(DialogState {
            action: DialogAction::OpenWith { apps, selected, .. },
            ..
        }) = self.dialogs.current_mut()
        {
            *selected = move_selection(*selected, apps.len(), delta);
            cx.notify();
        }
    }

    /// A click on one application of the "Open with…" list: choose it and
    /// run it.
    pub(crate) fn choose_open_with(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(DialogState {
            action: DialogAction::OpenWith { apps, selected, .. },
            ..
        }) = self.dialogs.current_mut()
            && index < apps.len()
        {
            *selected = index;
        }
        self.confirm_dialog(window, cx);
    }

    /// Runs the chosen application of a closed "Open with…" dialog.
    fn run_open_with(&mut self, path: &Path, app: &App, cx: &mut Context<Self>) {
        match open_with_command(path, app) {
            Ok(command) => {
                let failure = format!("Could not open {} with {}", quoted(path), app.name);
                self.launch(
                    command.program,
                    command.args,
                    Some(command.cwd),
                    failure,
                    cx,
                );
            }
            Err(message) => {
                self.tab_mut().state.set_notice(message);
                cx.notify();
            }
        }
    }

    /// Ctrl+Shift+C: the selected paths, one per line.
    pub(crate) fn copy_selected_paths(&mut self, cx: &mut Context<Self>) {
        if self.in_trash() {
            return;
        }
        let paths = self.selected_visible_paths();
        if paths.is_empty() {
            return;
        }
        let text = paths
            .iter()
            .map(|path| path.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n");
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.tab_mut().state.set_notice(match paths.as_slice() {
            [_] => "Copied the path".to_owned(),
            many => format!("Copied {} paths", many.len()),
        });
        cx.notify();
    }

    fn copy_folder_path(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Location::Directory(dir) = self.tab().state.current() else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(
            dir.to_string_lossy().into_owned(),
        ));
        self.tab_mut()
            .state
            .set_notice("Copied the folder path".to_owned());
        cx.notify();
    }

    /// Opens a file: an image in the viewer window (walking the images of
    /// the folder as displayed), anything else with the default
    /// application: `xdg-open <path>`, detached, no shell. A trashed file
    /// opens only after it is restored.
    pub(crate) fn open_file(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.in_trash() {
            self.tab_mut()
                .state
                .set_notice(format!("Restore {} to open it.", quoted(path)));
            cx.notify();
            return;
        }
        let target = open_target(path, &self.tab().state.visible_entries());
        match target {
            OpenTarget::Viewer { list, index } => self.show_in_viewer(&list, index, path, cx),
            OpenTarget::External => {
                let failure = format!("Could not open {}", quoted(path));
                self.launch(
                    PathBuf::from("xdg-open"),
                    vec![path.as_os_str().to_owned()],
                    None,
                    failure,
                    cx,
                );
            }
        }
    }

    /// "Open containing folder" in the Recent view and in search results: goes
    /// to the folder of
    /// the single selected file and selects the file there.
    pub(crate) fn open_containing_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.selected_visible_paths();
        let [path] = selected.as_slice() else {
            return;
        };
        let Some(folder) = path.parent().map(Path::to_path_buf) else {
            return;
        };
        self.tab_mut().pending = Some(PendingSelect {
            paths: vec![path.clone()],
            rename: false,
        });
        self.navigate(Transition::Visit(Location::Directory(folder)), window, cx);
    }

    /// "Open in viewer": the single selected image, even when it is not
    /// what Enter would open.
    pub(crate) fn open_in_viewer(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.in_trash() {
            return;
        }
        let selected = self.selected_visible_paths();
        let [path] = selected.as_slice() else {
            return;
        };
        let path = path.clone();
        self.open_file(&path, cx);
    }

    fn show_in_viewer(
        &mut self,
        list: &[PathBuf],
        index: usize,
        path: &Path,
        cx: &mut Context<Self>,
    ) {
        if let Err(error) = crate::viewer::open_list(list.to_vec(), index, cx) {
            log::warn!("cannot open the viewer: {error}");
            self.tab_mut()
                .state
                .set_notice(format!("Could not open {} in the viewer", quoted(path)));
            cx.notify();
        }
    }

    /// The one selected entry when it is a real folder (not a link to one).
    fn single_selected_folder(&self) -> Option<PathBuf> {
        match self.selected_with_kind().as_slice() {
            [(path, true)] => Some(path.clone()),
            _ => None,
        }
    }

    /// "Open in terminal": the resolved terminal (see
    /// [`fileflux::core::terminal`]) started in `folder`, else in the shown
    /// folder. The Trash is no folder to open.
    fn open_terminal(&mut self, folder: Option<PathBuf>, cx: &mut Context<Self>) {
        let Some(terminal) = self.terminal.clone() else {
            return;
        };
        let folder = match (folder, self.tab().state.current()) {
            (Some(folder), _) => folder,
            (None, Location::Directory(folder) | Location::Search { root: folder, .. }) => {
                folder.clone()
            }
            (None, Location::Trash | Location::Recent) => return,
        };
        let command = terminal.command(&folder);
        self.launch(
            command.program,
            command.args,
            Some(command.cwd),
            "Could not open a terminal".to_owned(),
            cx,
        );
    }

    /// Starts a detached program off the UI thread and reports a failure to
    /// start it. The child is reaped on a thread of its own: a terminal may
    /// outlive the call by hours.
    fn launch(
        &mut self,
        program: PathBuf,
        args: Vec<OsString>,
        cwd: Option<PathBuf>,
        failure: String,
        cx: &mut Context<Self>,
    ) {
        let started = cx.background_executor().spawn(async move {
            let mut command = Command::new(&program);
            command
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if let Some(cwd) = cwd {
                command.current_dir(cwd);
            }
            let mut child = command.spawn().map_err(|error| {
                io::Error::new(error.kind(), format!("{}: {error}", program.display()))
            })?;
            std::thread::Builder::new()
                .name("fileflux-reap".to_owned())
                .spawn(move || {
                    if let Err(error) = child.wait() {
                        log::debug!("waiting for a launched program failed: {error}");
                    }
                })?;
            Ok::<(), io::Error>(())
        });
        cx.spawn(async move |this, cx| {
            if let Err(error) = started.await
                && let Err(closed) = this.update(cx, |this, cx| {
                    this.tab_mut()
                        .state
                        .set_notice(format!("{failure}: {error}"));
                    cx.notify();
                })
            {
                log::debug!("the explorer closed before the launch result: {closed}");
            }
        })
        .detach();
    }
}
