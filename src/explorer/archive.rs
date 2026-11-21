//! Compress and extract on the UI side: the context-menu rows, the name and
//! destination dialog, and the jobs that run `bsdtar` off the UI thread.
//!
//! The mechanics and every safety rule live in [`fileflux::core::archive`]
//! (unique names, never overwriting, refusing `..` and absolute members,
//! removing links that point outside, cleaning up on failure and cancel).
//! This module decides what is offered and words the results. Both jobs are
//! cancellable: Cancel kills the child process and removes what it made.
//! A created archive or folder is undone by moving it to the Trash.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use bolt::controls::{InputEvent, InputState};
use fileflux::core::Location;
use fileflux::core::archive::{self, ArchiveError, Format};
use gpui::{AppContext as _, Context, Entity, Window};

use super::ops::{
    DialogAction, DialogKind, DialogState, Identity, MenuCommand, MenuItemSpec, MenuRow, Notice,
    Report, Severity, UndoAction,
};
use super::progress::{Sink, Unit};
use super::tabs::Affected;
use super::view::Explorer;

/// What the menu says (and a toast repeats) when `bsdtar` is not installed.
pub(crate) const MISSING_TOOL: &str = "Install libarchive (bsdtar) to compress and extract";

/// What a dialog's confirmation does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveAction {
    /// Write a new archive of `members` (names inside `folder`).
    Compress {
        folder: PathBuf,
        members: Vec<OsString>,
        format: Format,
    },
    /// Extract `archive` into a new folder inside the typed destination.
    ExtractTo { archive: PathBuf },
}

/// The open "Compress…" or "Extract to…" dialog: what it will do and the
/// field that holds the archive name or the destination folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ArchiveDialog {
    pub(crate) action: ArchiveAction,
    pub(crate) input: Entity<InputState>,
}

/// Which archive rows the menu of a selection shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ArchiveOffer {
    pub(crate) compress: bool,
    pub(crate) extract: bool,
    /// `bsdtar` is installed; without it the rows are shown disabled.
    pub(crate) tool: bool,
}

/// What a selection in a folder is offered. `selected` holds each selected
/// entry's path and whether it is a folder. Only real folders are offered
/// anything: not the Trash, Recent or search results.
pub(crate) fn offer_for(in_folder: bool, selected: &[(PathBuf, bool)], tool: bool) -> ArchiveOffer {
    let extract = matches!(
        selected,
        [(path, false)] if path.file_name().is_some_and(archive::is_archive_name)
    );
    ArchiveOffer {
        compress: in_folder && !selected.is_empty(),
        extract: in_folder && extract,
        tool,
    }
}

/// Adds the archive rows after the Paste group of a selection's menu.
pub(crate) fn add_rows(rows: &mut Vec<MenuRow>, offer: ArchiveOffer) {
    if !offer.compress && !offer.extract {
        return;
    }
    let Some(paste) = rows.iter().position(|row| {
        matches!(row, MenuRow::Item(spec)
            if matches!(spec.command, MenuCommand::Paste | MenuCommand::PasteIntoFolder))
    }) else {
        return;
    };
    let row = |command, label: &'static str, missing: &'static str| {
        MenuRow::Item(MenuItemSpec {
            enabled: offer.tool,
            ..MenuItemSpec::new(command, if offer.tool { label } else { missing })
        })
    };
    let mut added = Vec::new();
    if offer.extract {
        added.push(row(
            MenuCommand::ExtractHere,
            "Extract here",
            "Extract here (needs bsdtar)",
        ));
        added.push(row(
            MenuCommand::ExtractTo,
            "Extract to\u{2026}",
            "Extract to\u{2026} (needs bsdtar)",
        ));
    }
    if offer.compress {
        added.push(row(
            MenuCommand::Compress,
            "Compress\u{2026}",
            "Compress\u{2026} (needs bsdtar)",
        ));
    }
    added.push(MenuRow::Separator);
    let at = (paste + 2).min(rows.len());
    rows.splice(at..at, added);
}

/// The folder a typed destination means: `~` and `~/…` are the home folder,
/// a relative path is relative to `current`.
pub(crate) fn resolve_destination(text: &str, home: Option<&Path>, current: &Path) -> PathBuf {
    let text = text.trim();
    if text == "~" {
        return home.map_or_else(|| current.to_path_buf(), Path::to_path_buf);
    }
    if let (Some(rest), Some(home)) = (text.strip_prefix("~/"), home) {
        return home.join(rest);
    }
    current.join(text)
}

fn quoted(name: &OsStr) -> String {
    format!("\u{201c}{}\u{201d}", name.to_string_lossy())
}

fn name_of(path: &Path) -> String {
    quoted(path.file_name().unwrap_or(path.as_os_str()))
}

fn items(count: usize) -> String {
    if count == 1 {
        "1 item".to_owned()
    } else {
        format!("{count} items")
    }
}

/// The report of a cancelled run: nothing was left behind.
fn cancelled(what: &str) -> Report {
    Report {
        notice: Some(Notice {
            severity: Severity::Info,
            text: format!("{what} cancelled. Nothing was created."),
        }),
        cancelled: true,
        ..Report::default()
    }
}

/// A created item is selected afterwards and undone by trashing it.
fn created(path: PathBuf, notice: Notice, details: Vec<String>) -> Report {
    Report {
        notice: Some(notice),
        undo: Identity::of(&path)
            .ok()
            .map(|identity| UndoAction::Archive {
                path: path.clone(),
                identity,
            }),
        select: vec![path],
        details,
        ..Report::default()
    }
}

/// Runs "Compress…". Blocking: call it on the background executor.
pub(crate) fn run_compress(
    tool: &Path,
    folder: &Path,
    members: &[OsString],
    base: &OsStr,
    format: Format,
    sink: &Sink,
) -> Report {
    sink.advance(0, &base.to_string_lossy());
    match archive::compress(tool, folder, members, base, format, sink.cancel_flag()) {
        Ok(path) => {
            let text = format!("Created {} from {}", name_of(&path), items(members.len()));
            created(
                path,
                Notice {
                    severity: Severity::Success,
                    text,
                },
                Vec::new(),
            )
        }
        Err(ArchiveError::Cancelled) => cancelled("Compressing"),
        Err(error) => Report::error(format!("Could not compress: {error}")),
    }
}

/// Runs "Extract here" and "Extract to…". Blocking.
pub(crate) fn run_extract(
    tool: &Path,
    archive_path: &Path,
    dest_dir: &Path,
    sink: &Sink,
) -> Report {
    sink.advance(
        0,
        &archive_path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
    );
    match archive::extract(tool, archive_path, dest_dir, sink.cancel_flag()) {
        Ok(done) => {
            let mut text = format!(
                "Extracted {} to {} ({})",
                name_of(archive_path),
                name_of(&done.path),
                items(done.members)
            );
            let mut severity = Severity::Success;
            let mut details = Vec::new();
            if !done.removed_links.is_empty() {
                severity = Severity::Warning;
                let count = done.removed_links.len();
                text.push_str(&format!(
                    ". Removed {count} link{} pointing outside the folder",
                    if count == 1 { "" } else { "s" }
                ));
                details = done
                    .removed_links
                    .iter()
                    .map(|link| format!("Removed link {}", link.display()))
                    .collect();
            }
            created(done.path, Notice { severity, text }, details)
        }
        Err(ArchiveError::Cancelled) => cancelled("Extracting"),
        Err(error) => Report::error(format!(
            "Could not extract {}: {error}",
            name_of(archive_path)
        )),
    }
}

impl DialogState {
    /// The "Compress…" or "Extract to…" dialog.
    pub(crate) fn archive(names: &[String], dialog: ArchiveDialog) -> DialogState {
        let kind = DialogKind::Archive {
            extract: matches!(dialog.action, ArchiveAction::ExtractTo { .. }),
        };
        DialogState::new(kind, names, DialogAction::Archive(dialog))
    }
}

impl Explorer {
    /// The `bsdtar` to run, or a toast saying how to get it.
    fn archive_tool(&self, window: &Window, cx: &mut Context<Self>) -> Option<PathBuf> {
        let tool = archive::tool().map(Path::to_path_buf);
        if tool.is_none() {
            self.say(Severity::Info, MISSING_TOOL, window, cx);
        }
        tool
    }

    /// The folder shown, when it is a real one.
    fn shown_folder(&self) -> Option<PathBuf> {
        match self.tab().state.current() {
            Location::Directory(dir) => Some(dir.clone()),
            _ => None,
        }
    }

    /// The one input of the dialog, seeded with `seed`, whose Enter confirms.
    fn archive_input(
        &mut self,
        seed: String,
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let input = cx.new(|cx| {
            let mut input = InputState::new(window, cx);
            input.set_placeholder(placeholder, window, cx);
            input.set_value(seed, window, cx);
            input
        });
        cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| {
                // Only the live dialog's own field may confirm.
                let live = this.dialogs.current().is_some_and(|dialog| {
                    matches!(&dialog.action, DialogAction::Archive(state) if state.input == *input)
                });
                if live && matches!(event, InputEvent::PressEnter { .. }) {
                    this.confirm_dialog(window, cx);
                }
            },
        )
        .detach();
        input
    }

    fn offer_archive_dialog(
        &mut self,
        names: &[String],
        dialog: ArchiveDialog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = dialog.input.clone();
        self.offer_dialog(DialogState::archive(names, dialog), window, cx);
        cx.defer_in(window, move |_this, window, cx| {
            input.update(cx, |input, cx| input.focus(window, cx));
        });
    }

    /// "Compress…": asks for the archive's name and format.
    pub(crate) fn compress_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(folder) = self.shown_folder() else {
            return;
        };
        let members: Vec<OsString> = self
            .selected_visible_paths()
            .iter()
            .filter_map(|path| path.file_name().map(OsStr::to_owned))
            .collect();
        if members.is_empty() || self.archive_tool(window, cx).is_none() {
            return;
        }
        let names: Vec<String> = members
            .iter()
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        let base = archive::default_archive_base(&members);
        let input = self.archive_input(
            base.to_string_lossy().into_owned(),
            "Archive name",
            window,
            cx,
        );
        let action = ArchiveAction::Compress {
            folder,
            members,
            format: Format::Zip,
        };
        self.offer_archive_dialog(&names, ArchiveDialog { action, input }, window, cx);
    }

    /// The selected archive, when exactly one archive is selected.
    fn selected_archive(&self) -> Option<PathBuf> {
        match self.selected_visible_paths().as_slice() {
            [path] if path.file_name().is_some_and(archive::is_archive_name) => Some(path.clone()),
            _ => None,
        }
    }

    /// "Extract here": into a new folder named after the archive, next to it.
    pub(crate) fn extract_here(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(folder), Some(archive)) = (self.shown_folder(), self.selected_archive()) else {
            return;
        };
        if let Some(tool) = self.archive_tool(window, cx) {
            self.start_extract(tool, archive, folder, window, cx);
        }
    }

    /// "Extract to…": asks for the destination folder, prefilled with the
    /// other pane's folder while the view is split, else the shown folder.
    pub(crate) fn extract_to(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(folder), Some(archive)) = (self.shown_folder(), self.selected_archive()) else {
            return;
        };
        if self.archive_tool(window, cx).is_none() {
            return;
        }
        let other = self
            .tabs
            .other()
            .and_then(|list| match list.active().state.current() {
                Location::Directory(dir) => Some(dir.clone()),
                _ => None,
            });
        let seed = other.unwrap_or(folder);
        let name = archive
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        let input = self.archive_input(
            seed.to_string_lossy().into_owned(),
            "Destination folder",
            window,
            cx,
        );
        let action = ArchiveAction::ExtractTo { archive };
        self.offer_archive_dialog(&[name], ArchiveDialog { action, input }, window, cx);
    }

    /// A format button of the "Compress…" dialog.
    pub(crate) fn set_archive_format(&mut self, chosen: Format, cx: &mut Context<Self>) {
        if let Some(DialogState {
            action:
                DialogAction::Archive(ArchiveDialog {
                    action: ArchiveAction::Compress { format, .. },
                    ..
                }),
            ..
        }) = self.dialogs.current_mut()
        {
            *format = chosen;
            cx.notify();
        }
    }

    /// Confirming the dialog: validates the typed text (a refusal keeps the
    /// dialog open) and starts the job.
    pub(crate) fn confirm_archive(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(DialogAction::Archive(dialog)) =
            self.dialogs.current().map(|dialog| dialog.action.clone())
        else {
            return;
        };
        let text = dialog.input.read(cx).value().to_string();
        let Some(tool) = self.archive_tool(window, cx) else {
            return;
        };
        match dialog.action {
            ArchiveAction::Compress {
                folder,
                members,
                format,
            } => {
                let base = OsString::from(text.trim());
                if let Err(error) = archive::archive_file_name(&base, format) {
                    self.say(
                        Severity::Warning,
                        format!("That archive name won't do: {error}"),
                        window,
                        cx,
                    );
                    return;
                }
                self.dialogs.close();
                self.dialog_closed(window, cx);
                self.start_compress(tool, folder, members, base, format, window, cx);
            }
            ArchiveAction::ExtractTo { archive } => {
                if text.trim().is_empty() {
                    self.say(Severity::Warning, "Type a destination folder", window, cx);
                    return;
                }
                let current = self.shown_folder().unwrap_or_default();
                let dest = resolve_destination(&text, self.home.as_deref(), &current);
                self.dialogs.close();
                self.dialog_closed(window, cx);
                self.start_extract(tool, archive, dest, window, cx);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn start_compress(
        &mut self,
        tool: PathBuf,
        folder: PathBuf,
        members: Vec<OsString>,
        base: OsString,
        format: Format,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = match members.as_slice() {
            [one] => format!("Compressing {}", quoted(one)),
            many => format!("Compressing {}", items(many.len())),
        };
        let touches = vec![folder.clone()];
        let affected = Affected::Folders(vec![Location::Directory(folder.clone())]);
        let touched = touches.clone();
        self.when_not_ejecting(touches, window, cx, move |this, window, cx| {
            this.start_work(
                title,
                Unit::Items,
                true,
                (affected, touched),
                move |sink| run_compress(&tool, &folder, &members, &base, format, &sink),
                window,
                cx,
            );
        });
    }

    fn start_extract(
        &mut self,
        tool: PathBuf,
        archive_path: PathBuf,
        dest: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = format!("Extracting {}", name_of(&archive_path));
        let touches = vec![archive_path.clone(), dest.clone()];
        let affected = Affected::Folders(vec![Location::Directory(dest.clone())]);
        let touched = touches.clone();
        self.when_not_ejecting(touches, window, cx, move |this, window, cx| {
            this.start_work(
                title,
                Unit::Items,
                true,
                (affected, touched),
                move |sink| run_extract(&tool, &archive_path, &dest, &sink),
                window,
                cx,
            );
        });
    }
}

#[cfg(test)]
#[path = "archive_tests.rs"]
mod tests;
