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
use std::time::Duration;

use bolt::controls::{InputEvent, InputState};
use bolt::toast::{Toast, ToastAction};
use fileflux::core::Location;
use gpui::{
    AnyWindowHandle, AppContext as _, ClipboardItem, Context, Entity, Pixels, Point, Subscription,
    Window,
};

use super::ops::{
    DialogKind, Job, JobCtx, MenuCommand, MenuRow, MenuTarget, NewKind, Notice, RenameCheck,
    Report, Severity, UndoEntry, check_rename, dialog_text, menu_rows, next_menu_index, stem_range,
};
use super::progress::Unit;
use super::view::Explorer;

/// How long a toast with an Undo button waits before it goes away.
const UNDO_TOAST: Duration = Duration::from_secs(8);
/// How long an error toast stays.
const ERROR_TOAST: Duration = Duration::from_secs(12);

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

/// The open context menu.
pub(crate) struct MenuState {
    pub(crate) position: Point<Pixels>,
    pub(crate) rows: Vec<MenuRow>,
    /// The highlighted row (keyboard or hover).
    pub(crate) selected: Option<usize>,
}

/// What a confirmed dialog does.
pub(crate) enum DialogAction {
    DeletePermanently(Vec<PathBuf>),
    DeleteFromTrash(Vec<OsString>),
    /// The Trash names the dialog counted; nothing else is deleted.
    EmptyTrash(Vec<OsString>),
}

/// The open confirmation dialog.
pub(crate) struct DialogState {
    pub(crate) text: super::ops::DialogText,
    pub(crate) action: DialogAction,
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
    fn in_trash(&self) -> bool {
        *self.state.current() == Location::Trash
    }

    /// The selected entries that are on screen under the current filter.
    /// Operations act on these only: a selection the filter hides is never
    /// trashed or deleted behind the user's back.
    fn selected_visible_paths(&self) -> Vec<PathBuf> {
        let selection = self.state.selection();
        self.state
            .visible_entries()
            .into_iter()
            .filter(|entry| selection.contains(&entry.path))
            .map(|entry| entry.path.clone())
            .collect()
    }

    // --- jobs ------------------------------------------------------------------

    fn job_ctx(&self) -> JobCtx {
        let mounts = self.places.as_ref().map_or_else(Vec::new, |places| {
            places
                .volumes
                .iter()
                .map(|volume| volume.path.clone())
                .collect()
        });
        // Without discovered places the root is empty, which no trash can
        // be chosen under: deletes then ask for confirmation instead.
        JobCtx::from_env(self.trash_root.clone().unwrap_or_default(), mounts)
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
        let (id, sink) = self.progress.start(title, Unit::Items, cancellable);
        let ctx = self.job_ctx();
        let work = cx
            .background_executor()
            .spawn(async move { super::ops::run_job(job, &ctx, &sink) });

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
        let undo_id = report.undo.take().map(|action| self.undo.push(action));
        if let Some(notice) = report.notice.take() {
            self.push_toast(notice, &report.details, undo_id, window, cx);
        }
        if !report.needs_permanent.is_empty() && !report.cancelled {
            let names: Vec<String> = report
                .needs_permanent
                .iter()
                .map(|path| display_name(path))
                .collect();
            self.open_dialog(
                DialogKind::NoTrash,
                &names,
                DialogAction::DeletePermanently(std::mem::take(&mut report.needs_permanent)),
                window,
                cx,
            );
        }
        if !report.select.is_empty() {
            self.pending = Some(PendingSelect {
                paths: std::mem::take(&mut report.select),
                rename: report.begin_rename,
            });
        }
        // The watcher would catch up within a moment; this makes our own
        // result show right away, and covers the unwatched Trash.
        let scan = self.state.begin_op_refresh();
        self.start_scan(scan, window, cx);
        cx.notify();
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
    ) {
        let mut toast = match notice.severity {
            Severity::Success => Toast::success(notice.text),
            Severity::Info => Toast::info(notice.text),
            Severity::Warning => Toast::warning(notice.text),
            Severity::Error => Toast::error(notice.text).auto_dismiss_after(ERROR_TOAST),
        };
        if !details.is_empty() {
            toast = toast.details(details.join("\n")).collapsible();
        }
        if let Some(id) = undo_id {
            let weak = cx.entity().downgrade();
            let handle: AnyWindowHandle = window.window_handle();
            toast = toast.auto_dismiss_after(UNDO_TOAST).action(
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
        toast.push(cx);
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

    /// After the listing changed: drop a rename whose entry vanished, then
    /// select what the last operation produced (and start renaming it).
    pub(crate) fn after_listing_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(renaming) = &self.renaming
            && !self
                .state
                .listing()
                .entries
                .iter()
                .any(|entry| entry.path == renaming.path)
        {
            self.renaming = None;
        }
        let Some(pending) = self.pending.take() else {
            return;
        };
        let present: Vec<PathBuf> = pending
            .paths
            .into_iter()
            .filter(|path| self.state.visible_position_of(path).is_some())
            .collect();
        let Some((first, rest)) = present.split_first() else {
            return;
        };
        self.state.select_only(first);
        for path in rest {
            self.state.toggle(path);
        }
        if let Some(position) = self.state.visible_position_of(first) {
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
        let Location::Directory(dir) = self.state.current().clone() else {
            return;
        };
        // A filter could hide the new entry; the new item must be visible
        // to be renamed.
        if !self.state.query().is_empty() {
            self.state.clear_query();
            self.search_input
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
            .snapshot
            .iter()
            .find(|entry| entry.path == *path)
            .cloned()
        else {
            return;
        };
        let is_dir = entry.is_directory() && !entry.is_symlink;
        let seed = entry.name.to_string_lossy().into_owned();
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
        let check = if typed == renaming.seed {
            RenameCheck::Unchanged
        } else {
            check_rename(
                &renaming.original,
                &typed,
                self.state
                    .listing()
                    .entries
                    .iter()
                    .map(|entry| entry.name.as_os_str()),
            )
        };
        match check {
            RenameCheck::Unchanged => self.finish_rename(from_blur, window, cx),
            RenameCheck::Invalid(message) if from_blur => {
                self.finish_rename(true, window, cx);
                self.say(
                    Severity::Warning,
                    format!("Rename canceled: {message}"),
                    window,
                    cx,
                );
            }
            RenameCheck::Invalid(message) => {
                if let Some(renaming) = self.renaming.as_mut() {
                    renaming.error = Some(message);
                }
                cx.notify();
            }
            RenameCheck::Rename(new_name) => {
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

    fn open_dialog(
        &mut self,
        kind: DialogKind,
        names: &[String],
        action: DialogAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_menu(window, cx);
        self.dialog = Some(DialogState {
            text: dialog_text(&kind, names),
            action,
        });
        // The safe button is the default: Enter alone never deletes.
        cx.defer_in(window, |this, window, cx| {
            this.dialog_cancel_focus.focus(window, cx);
        });
        cx.notify();
    }

    /// The danger button: runs what the dialog asked for.
    pub(crate) fn confirm_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        self.entries_focus.focus(window, cx);
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
        };
        self.start_job(title, job, true, window, cx);
        cx.notify();
    }

    /// Cancel, Escape or a click on the scrim: nothing happens.
    pub(crate) fn cancel_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.take().is_some() {
            self.entries_focus.focus(window, cx);
            cx.notify();
        }
    }

    // --- undo ------------------------------------------------------------------------

    /// Ctrl+Z: undoes the newest operation of the session.
    pub(crate) fn undo_last(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.undo.pop() {
            Some(entry) => self.run_undo(entry, window, cx),
            None => self.say(Severity::Info, "Nothing to undo", window, cx),
        }
    }

    /// A toast's Undo button: undoes exactly that operation.
    pub(crate) fn undo_entry(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        match self.undo.take(id) {
            Some(entry) => self.run_undo(entry, window, cx),
            None => self.say(Severity::Info, "Already undone", window, cx),
        }
    }

    fn run_undo(&mut self, entry: UndoEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.start_job("Undoing", Job::Undo(entry.action), false, window, cx);
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
            Some(path) if !self.state.selection().contains(path) => self.state.select_only(path),
            Some(_) => {}
            None => self.state.clear_selection(),
        }
        let target = MenuTarget {
            in_trash: self.in_trash(),
            selected: self.selected_visible_paths().len(),
            folder_is_empty: self.state.listing().entries.is_empty(),
            terminal: self.terminal.is_some(),
        };
        let rows = menu_rows(&target);
        if rows.is_empty() {
            cx.notify();
            return;
        }
        self.menu = Some(MenuState {
            position,
            rows,
            selected: None,
        });
        cx.defer_in(window, |this, window, cx| {
            if this.menu.is_some() {
                this.menu_focus.focus(window, cx);
            }
        });
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
                MenuRow::Item(item) => Some(item.command),
                MenuRow::Separator => None,
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
        self.close_menu(window, cx);
        match command {
            MenuCommand::Open => self.open_selected(window, cx),
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
            MenuCommand::OpenTerminal => self.open_terminal(window, cx),
            MenuCommand::CopyFolderPath => self.copy_folder_path(window, cx),
            MenuCommand::Restore => self.restore_selected(window, cx),
            MenuCommand::EmptyTrash => self.request_empty_trash(window, cx),
        }
    }

    // --- clipboard, open, terminal -------------------------------------------------------

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
        self.state.set_notice(match paths.as_slice() {
            [_] => "Copied the path".to_owned(),
            many => format!("Copied {} paths", many.len()),
        });
        cx.notify();
    }

    fn copy_folder_path(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Location::Directory(dir) = self.state.current() else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(
            dir.to_string_lossy().into_owned(),
        ));
        self.state.set_notice("Copied the folder path".to_owned());
        cx.notify();
    }

    /// Opens a file with the default application: `xdg-open <path>`,
    /// detached, no shell. A trashed file opens only after it is restored.
    pub(crate) fn open_file(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.in_trash() {
            self.state
                .set_notice(format!("Restore {} to open it.", quoted(path)));
            cx.notify();
            return;
        }
        let failure = format!("Could not open {}", quoted(path));
        self.launch(
            PathBuf::from("xdg-open"),
            vec![path.as_os_str().to_owned()],
            None,
            failure,
            cx,
        );
    }

    /// "Open terminal here": `$TERMINAL` or `xdg-terminal-exec`, started in
    /// the shown folder.
    fn open_terminal(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let (Some(program), Location::Directory(dir)) =
            (self.terminal.clone(), self.state.current().clone())
        else {
            return;
        };
        self.launch(
            program,
            Vec::new(),
            Some(dir),
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
                    this.state.set_notice(format!("{failure}: {error}"));
                    cx.notify();
                })
            {
                log::debug!("the explorer closed before the launch result: {closed}");
            }
        })
        .detach();
    }
}
