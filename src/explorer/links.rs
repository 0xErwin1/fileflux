//! Symbolic links on the UI side: the context-menu rows, the jobs that
//! create them off the UI thread, and their Undo.
//!
//! The mechanics (naming, never overwriting, absolute targets) live in
//! [`fileflux::core::links`]. A link is made through the same job path as
//! every other operation: progress card, one summary notice, per-item
//! errors under "Details". Undo unlinks exactly the links that were made,
//! each only if it is still the symlink that was created, and never follows
//! it: what a link pointed at is not touched.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use fileflux::core::Location;
use fileflux::core::links::{self, Naming};
use gpui::{Context, Window};

use super::ops::paste::{PasteInto, paste_target};
use super::ops::{
    Identity, MenuCommand, MenuItemSpec, MenuRow, Notice, Report, Severity, UndoAction,
};
use super::progress::{Sink, Unit};
use super::tabs::Affected;
use super::view::Explorer;

/// Which link rows a menu shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LinkOffer {
    /// "Make Link": a link beside each selected entry.
    pub(crate) make: bool,
    /// "Paste as link": links to the clipboard in the folder or the shown
    /// folder.
    pub(crate) paste_as: bool,
    /// The clipboard holds something; without it "Paste as link" is shown
    /// disabled.
    pub(crate) can_paste: bool,
}

/// What a menu is offered. `selected` holds each selected entry's path and
/// whether it is a folder. Only a real folder takes links: not the Trash,
/// Recent or search results. "Paste as link" goes into the one selected
/// folder, or into the shown folder when nothing is selected.
pub(crate) fn offer_for(
    in_folder: bool,
    selected: &[(PathBuf, bool)],
    can_paste: bool,
) -> LinkOffer {
    LinkOffer {
        make: in_folder && !selected.is_empty(),
        paste_as: in_folder && matches!(selected, [] | [(_, true)]),
        can_paste,
    }
}

/// Adds the link rows: "Paste as link" right after Paste, "Make Link"
/// before "Copy path".
pub(crate) fn add_rows(rows: &mut Vec<MenuRow>, offer: LinkOffer) {
    if offer.paste_as {
        let paste = rows.iter().position(|row| {
            matches!(row, MenuRow::Item(spec)
                if matches!(spec.command, MenuCommand::Paste | MenuCommand::PasteIntoFolder))
        });
        if let Some(paste) = paste {
            rows.insert(
                paste + 1,
                MenuRow::Item(MenuItemSpec {
                    enabled: offer.can_paste,
                    ..MenuItemSpec::new(MenuCommand::PasteAsLink, "Paste as link")
                }),
            );
        }
    }
    if offer.make {
        let at = rows
            .iter()
            .position(
                |row| matches!(row, MenuRow::Item(spec) if spec.command == MenuCommand::CopyPath),
            )
            .unwrap_or(rows.len());
        rows.insert(
            at,
            MenuRow::Item(MenuItemSpec::new(MenuCommand::MakeLink, "Make Link")),
        );
    }
}

/// Why `location` takes no links, or `None` for a real folder.
pub(crate) fn refusal(location: &Location) -> Option<&'static str> {
    match location {
        Location::Directory(_) => None,
        Location::Trash => Some("Can't make links in the Trash"),
        Location::Recent => Some("Can't make links in Recent. Open a folder first"),
        Location::Search { .. } => Some("Can't make links in search results. Open a folder first"),
    }
}

/// One link that was created, with which file it was, for the Undo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LinkedEntry {
    pub(crate) link: PathBuf,
    pub(crate) identity: Identity,
}

fn quoted(path: &Path) -> String {
    format!(
        "\u{201c}{}\u{201d}",
        path.file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
    )
}

fn links_label(count: usize) -> String {
    if count == 1 {
        "1 link".to_owned()
    } else {
        format!("{count} links")
    }
}

/// Runs a link job: a link to each of `sources`, beside it (`into` is
/// `None`) or inside `into`. Blocking: call it on the background executor.
pub(crate) fn run_links(
    sources: &[PathBuf],
    into: Option<&Path>,
    naming: Naming,
    sink: &Sink,
) -> Report {
    sink.set_total(sources.len() as u64);
    let outcome = links::make_links(
        sources,
        into,
        naming,
        |index, source| {
            let name = source
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
            sink.advance(index as u64, &name);
        },
        sink.cancel_flag(),
    );
    sink.advance(sources.len() as u64, "");
    let details: Vec<String> = outcome
        .failed
        .iter()
        .map(|(source, error)| format!("{}: {error}", quoted(source)))
        .collect();
    let made = outcome.made.len();
    let failed = outcome.failed.len();
    let (severity, text) = if made == 0 {
        match (outcome.cancelled, outcome.failed.first()) {
            (true, _) => (
                Severity::Info,
                "Cancelled. No links were created.".to_owned(),
            ),
            (false, Some((_, error))) if failed == 1 => {
                (Severity::Error, format!("Could not create a link: {error}"))
            }
            (false, _) => (
                Severity::Error,
                format!("Could not create links ({failed} failed)"),
            ),
        }
    } else {
        let mut text = match outcome.made.as_slice() {
            [one] => format!("Created link {}", quoted(&one.link)),
            _ => format!("Created {}", links_label(made)),
        };
        if failed > 0 {
            text.push_str(&format!("; {failed} failed"));
        }
        if outcome.cancelled {
            text.push_str("; cancelled");
        }
        let severity = if failed > 0 {
            Severity::Warning
        } else if outcome.cancelled {
            Severity::Info
        } else {
            Severity::Success
        };
        (severity, text)
    };
    let items: Vec<LinkedEntry> = outcome
        .made
        .iter()
        .filter_map(|made| {
            Identity::of(&made.link).ok().map(|identity| LinkedEntry {
                link: made.link.clone(),
                identity,
            })
        })
        .collect();
    Report {
        notice: Some(Notice { severity, text }),
        undo: (!items.is_empty()).then_some(UndoAction::Links { items }),
        select: outcome.made.into_iter().map(|made| made.link).collect(),
        details,
        cancelled: outcome.cancelled,
        ..Report::default()
    }
}

/// Undo: unlinks each created link that is still the symlink that was
/// created. `remove_file` is `unlink(2)`: it removes the link itself and
/// never follows it, so a link to a folder leaves the folder and its
/// contents alone. What could not be unlinked stays undoable.
pub(crate) fn undo(items: &[LinkedEntry], sink: &Sink) -> Report {
    sink.set_total(items.len() as u64);
    let mut done: Vec<&Path> = Vec::new();
    let mut remaining: Vec<LinkedEntry> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let (mut skipped, mut failed) = (0usize, 0usize);
    for (index, item) in items.iter().enumerate() {
        sink.advance(index as u64, &quoted(&item.link));
        let name = quoted(&item.link);
        match fs::symlink_metadata(&item.link) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                skipped += 1;
                problems.push(format!("{name} no longer exists"));
            }
            Err(error) => {
                failed += 1;
                problems.push(format!("{name}: {error}"));
                remaining.push(item.clone());
            }
            Ok(meta)
                if !meta.file_type().is_symlink()
                    || Identity::from_meta(&meta) != item.identity =>
            {
                skipped += 1;
                problems.push(format!("{name} changed since it was created"));
            }
            Ok(_) => match fs::remove_file(&item.link) {
                Ok(()) => done.push(&item.link),
                Err(error) => {
                    failed += 1;
                    problems.push(format!("{name}: {error}"));
                    remaining.push(item.clone());
                }
            },
        }
    }
    sink.advance(items.len() as u64, "");
    let mut report = if done.is_empty() {
        Report::error(format!("Can't undo: {}", problems.join("; ")))
    } else {
        let mut text = match done.as_slice() {
            [one] => format!("Removed link {}", quoted(one)),
            many => format!("Removed {}", links_label(many.len())),
        };
        if skipped > 0 {
            text.push_str(&format!("; {skipped} skipped"));
        }
        if failed > 0 {
            text.push_str(&format!("; {failed} failed"));
        }
        let severity = if problems.is_empty() {
            Severity::Success
        } else {
            Severity::Warning
        };
        let mut report = Report::say(severity, text);
        report.details = problems;
        report
    };
    report.undo = (!remaining.is_empty()).then_some(UndoAction::Links { items: remaining });
    report
}

impl Explorer {
    /// The shown folder, or a toast saying why links cannot be made here.
    fn link_folder(&self, window: &Window, cx: &mut Context<Self>) -> Option<PathBuf> {
        let location = self.tab().state.current();
        if let Some(reason) = refusal(location) {
            self.say(Severity::Info, reason, window, cx);
            return None;
        }
        match location {
            Location::Directory(dir) => Some(dir.clone()),
            _ => None,
        }
    }

    /// "Make Link": a link beside each selected entry.
    pub(crate) fn make_links_of_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.link_folder(window, cx).is_none() {
            return;
        }
        let sources = self.selected_visible_paths();
        self.start_links(sources, None, Naming::LinkTo, window, cx);
    }

    /// "Paste as link": links to the clipboard's entries in the selected
    /// folder, else in the shown one. The clipboard is left as it is: a cut
    /// is still waiting for a real paste.
    pub(crate) fn paste_as_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reconcile_clipboard(cx);
        let Some(current) = self.link_folder(window, cx) else {
            return;
        };
        let Some(clip) = self.clipboard.get().cloned() else {
            self.say(Severity::Info, "Nothing to paste", window, cx);
            return;
        };
        let selected = self.selected_with_kind();
        let into = match selected.as_slice() {
            [(_, true)] => PasteInto::SelectedFolder,
            _ => PasteInto::CurrentFolder,
        };
        let Some(dest) = paste_target(into, &current, &selected) else {
            return;
        };
        self.start_links(clip.paths, Some(dest), Naming::Source, window, cx);
    }

    /// Starts a link job: what the menu rows and a Ctrl+Shift drop share.
    /// Nothing is read from or written to a device that is being ejected.
    pub(crate) fn start_links(
        &mut self,
        sources: Vec<PathBuf>,
        into: Option<PathBuf>,
        naming: Naming,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if sources.is_empty() {
            return;
        }
        let title = match sources.as_slice() {
            [one] => format!("Linking {}", quoted(one)),
            many => format!("Linking {} items", many.len()),
        };
        let mut folders: Vec<Location> = Vec::new();
        let places = match &into {
            Some(dir) => vec![dir.as_path()],
            None => sources.iter().filter_map(|path| path.parent()).collect(),
        };
        for dir in places {
            let folder = Location::Directory(dir.to_path_buf());
            if !folders.contains(&folder) {
                folders.push(folder);
            }
        }
        let mut touches = sources.clone();
        touches.extend(into.clone());
        let affected = Affected::Folders(folders);
        let touched = touches.clone();
        self.when_not_ejecting(touches, window, cx, move |this, window, cx| {
            this.start_work(
                title,
                Unit::Items,
                true,
                (affected, touched),
                move |sink| run_links(&sources, into.as_deref(), naming, &sink),
                window,
                cx,
            );
        });
    }
}

#[cfg(test)]
#[path = "links_tests.rs"]
mod tests;
