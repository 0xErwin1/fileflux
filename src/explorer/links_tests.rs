use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use fileflux::core::Location;

use super::super::arrange::Arrangement;
use super::super::ops::{MenuTarget, menu_rows};
use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

static SEQ: AtomicU64 = AtomicU64::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> std::io::Result<Scratch> {
        let dir = std::env::temp_dir().join(format!(
            "fileflux-explorer-links-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir)?;
        Ok(Scratch(dir))
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn p(text: &str) -> PathBuf {
    PathBuf::from(text)
}

fn selected(items: &[(&str, bool)]) -> Vec<(PathBuf, bool)> {
    items.iter().map(|(name, dir)| (p(name), *dir)).collect()
}

fn spec(rows: &[MenuRow], command: MenuCommand) -> Option<&MenuItemSpec> {
    rows.iter().find_map(|row| match row {
        MenuRow::Item(spec) if spec.command == command => Some(spec),
        _ => None,
    })
}

/// The menu of the real folder view for `selection`, with the link rows.
fn folder_menu(selection: &[(&str, bool)], in_folder: bool, can_paste: bool) -> Vec<MenuRow> {
    let selection = selected(selection);
    let target = MenuTarget {
        in_trash: !in_folder,
        selected: selection.len(),
        folder_is_empty: false,
        terminal: false,
        can_paste,
        folder_selected: matches!(selection.as_slice(), [(_, true)]),
        only_files: matches!(selection.as_slice(), [(_, false)]),
        folder_favorite: false,
    };
    let mut rows = menu_rows(&target, &Arrangement::default());
    add_rows(&mut rows, offer_for(in_folder, &selection, can_paste));
    rows
}

#[test]
fn the_offer_follows_the_selection_in_a_real_folder() {
    let make = |selection: &[(&str, bool)]| offer_for(true, &selected(selection), true);
    let file = make(&[("/d/a", false)]);
    assert!(file.make && !file.paste_as);
    let folder = make(&[("/d/a", true)]);
    assert!(folder.make && folder.paste_as);
    let several = make(&[("/d/a", true), ("/d/b", true)]);
    assert!(several.make && !several.paste_as);
    let empty = make(&[]);
    assert!(!empty.make && empty.paste_as);
}

#[test]
fn nothing_is_offered_outside_a_real_folder() {
    for selection in [
        selected(&[]),
        selected(&[("/d/a", false)]),
        selected(&[("/d/a", true)]),
    ] {
        let offer = offer_for(false, &selection, true);
        assert!(!offer.make && !offer.paste_as);
    }
}

#[test]
fn a_file_menu_has_make_link_and_no_paste_as_link() {
    let rows = folder_menu(&[("/d/a.txt", false)], true, true);
    let make = spec(&rows, MenuCommand::MakeLink);
    assert_eq!(
        make.map(|s| (s.label, s.enabled)),
        Some(("Make Link", true))
    );
    assert!(spec(&rows, MenuCommand::PasteAsLink).is_none());
}

#[test]
fn a_folder_menu_has_both_and_paste_as_link_follows_the_clipboard() {
    let rows = folder_menu(&[("/d/a", true)], true, true);
    assert!(spec(&rows, MenuCommand::MakeLink).is_some());
    let paste = spec(&rows, MenuCommand::PasteAsLink);
    assert_eq!(
        paste.map(|s| (s.label, s.enabled)),
        Some(("Paste as link", true))
    );
    let empty_clipboard = folder_menu(&[("/d/a", true)], true, false);
    assert_eq!(
        spec(&empty_clipboard, MenuCommand::PasteAsLink).map(|s| s.enabled),
        Some(false)
    );
}

#[test]
fn paste_as_link_sits_right_after_paste() {
    let rows = folder_menu(&[("/d/a", true)], true, true);
    let at = |command| {
        rows.iter()
            .position(|row| matches!(row, MenuRow::Item(spec) if spec.command == command))
    };
    assert_eq!(
        at(MenuCommand::PasteAsLink),
        at(MenuCommand::PasteIntoFolder).map(|paste| paste + 1)
    );
}

#[test]
fn several_entries_get_make_link_only() {
    let rows = folder_menu(&[("/d/a", true), ("/d/b", false)], true, true);
    assert!(spec(&rows, MenuCommand::MakeLink).is_some());
    assert!(spec(&rows, MenuCommand::PasteAsLink).is_none());
}

#[test]
fn the_empty_space_menu_has_paste_as_link_only() {
    let rows = folder_menu(&[], true, true);
    assert!(spec(&rows, MenuCommand::MakeLink).is_none());
    let paste = spec(&rows, MenuCommand::PasteAsLink);
    assert_eq!(paste.map(|s| s.enabled), Some(true));
    let disabled = folder_menu(&[], true, false);
    assert_eq!(
        spec(&disabled, MenuCommand::PasteAsLink).map(|s| s.enabled),
        Some(false)
    );
}

#[test]
fn the_trash_menu_has_no_link_rows() {
    for selection in [&[][..], &[("/t/a", false)][..]] {
        let rows = folder_menu(selection, false, true);
        assert!(spec(&rows, MenuCommand::MakeLink).is_none());
        assert!(spec(&rows, MenuCommand::PasteAsLink).is_none());
    }
}

#[test]
fn only_a_real_folder_takes_links() {
    assert_eq!(refusal(&Location::Directory(p("/d"))), None);
    for (location, text) in [
        (Location::Trash, "Can't make links in the Trash"),
        (
            Location::Recent,
            "Can't make links in Recent. Open a folder first",
        ),
        (
            Location::Search {
                root: p("/d"),
                query: "a".to_owned(),
            },
            "Can't make links in search results. Open a folder first",
        ),
    ] {
        assert_eq!(refusal(&location), Some(text));
    }
}

fn run(sources: &[PathBuf], into: Option<&Path>, naming: Naming) -> Report {
    run_links(sources, into, naming, &Sink::new(Unit::Items))
}

#[test]
fn one_link_is_selected_and_undoable() -> TestResult {
    let s = Scratch::new()?;
    fs::write(s.path("a.txt"), b"x")?;
    let report = run(&[s.path("a.txt")], None, Naming::LinkTo);
    let notice = report.notice.as_ref().ok_or("a notice")?;
    assert_eq!(notice.severity, Severity::Success);
    assert_eq!(notice.text, "Created link \u{201c}Link to a.txt\u{201d}");
    assert_eq!(report.select, vec![s.path("Link to a.txt")]);
    let Some(UndoAction::Links { items }) = &report.undo else {
        return Err("an undo entry".into());
    };
    assert_eq!(items.len(), 1);
    assert_eq!(
        items.first().map(|i| i.link.clone()),
        Some(s.path("Link to a.txt"))
    );
    assert!(report.details.is_empty());
    Ok(())
}

#[test]
fn several_links_say_how_many() -> TestResult {
    let s = Scratch::new()?;
    fs::write(s.path("a"), b"")?;
    fs::write(s.path("b"), b"")?;
    let report = run(&[s.path("a"), s.path("b")], None, Naming::LinkTo);
    assert_eq!(
        report.notice.map(|n| n.text),
        Some("Created 2 links".to_owned())
    );
    assert_eq!(report.select.len(), 2);
    Ok(())
}

#[test]
fn a_failed_item_is_reported_and_the_rest_still_linked() -> TestResult {
    let s = Scratch::new()?;
    fs::write(s.path("a"), b"")?;
    let report = run(&[s.path("gone"), s.path("a")], None, Naming::LinkTo);
    let notice = report.notice.as_ref().ok_or("a notice")?;
    assert_eq!(notice.severity, Severity::Warning);
    assert_eq!(
        notice.text,
        "Created link \u{201c}Link to a\u{201d}; 1 failed: it no longer exists"
    );
    assert_eq!(
        report.details,
        ["\u{201c}gone\u{201d}: it no longer exists"]
    );
    assert!(report.undo.is_some());
    Ok(())
}

#[test]
fn when_nothing_could_be_linked_it_is_an_error_without_undo() -> TestResult {
    let s = Scratch::new()?;
    let report = run(&[s.path("gone")], None, Naming::LinkTo);
    let notice = report.notice.as_ref().ok_or("a notice")?;
    assert_eq!(notice.severity, Severity::Error);
    assert_eq!(
        notice.text,
        "Could not create a link to \u{201c}gone\u{201d}: it no longer exists"
    );
    let many = run(&[s.path("gone"), s.path("lost")], None, Naming::LinkTo);
    assert_eq!(
        many.notice.map(|n| n.text),
        Some("Could not create 2 links: it no longer exists".to_owned())
    );
    assert_eq!(many.details.len(), 2);
    assert!(report.undo.is_none() && report.select.is_empty());
    Ok(())
}

#[test]
fn paste_as_link_never_overwrites_and_does_not_touch_the_sources() -> TestResult {
    let s = Scratch::new()?;
    fs::create_dir_all(s.path("dest"))?;
    fs::write(s.path("data.txt"), b"source")?;
    fs::write(s.path("dest/data.txt"), b"mine")?;
    let report = run(&[s.path("data.txt")], Some(&s.path("dest")), Naming::Source);
    assert_eq!(report.select, vec![s.path("dest/data (2).txt")]);
    assert_eq!(fs::read(s.path("dest/data.txt"))?, b"mine");
    assert_eq!(fs::read(s.path("data.txt"))?, b"source");
    assert!(fs::read_link(s.path("dest/data (2).txt"))?.is_absolute());
    Ok(())
}

fn undo_of(report: Report) -> Result<Vec<LinkedEntry>, Box<dyn std::error::Error>> {
    match report.undo {
        Some(UndoAction::Links { items }) => Ok(items),
        _ => Err("an undo entry".into()),
    }
}

#[test]
fn undo_removes_only_the_link_and_leaves_a_directory_target_intact() -> TestResult {
    let s = Scratch::new()?;
    fs::create_dir_all(s.path("Photos"))?;
    fs::write(s.path("Photos/keep.txt"), b"keep")?;
    let items = undo_of(run(&[s.path("Photos")], None, Naming::LinkTo))?;
    let report = undo(&items, &Sink::new(Unit::Items));
    let notice = report.notice.as_ref().ok_or("a notice")?;
    assert_eq!(notice.severity, Severity::Success);
    assert_eq!(notice.text, "Removed link \u{201c}Link to Photos\u{201d}");
    assert!(fs::symlink_metadata(s.path("Link to Photos")).is_err());
    assert_eq!(fs::read(s.path("Photos/keep.txt"))?, b"keep");
    assert!(report.undo.is_none());
    Ok(())
}

#[test]
fn undo_refuses_a_link_that_was_replaced_by_something_else() -> TestResult {
    let s = Scratch::new()?;
    fs::write(s.path("a"), b"")?;
    let items = undo_of(run(&[s.path("a")], None, Naming::LinkTo))?;
    fs::remove_file(s.path("Link to a"))?;
    fs::write(s.path("Link to a"), b"someone else's file")?;
    let report = undo(&items, &Sink::new(Unit::Items));
    let notice = report.notice.as_ref().ok_or("a notice")?;
    assert_eq!(notice.severity, Severity::Error);
    assert!(notice.text.starts_with("Can't undo: "));
    assert_eq!(fs::read(s.path("Link to a"))?, b"someone else's file");
    Ok(())
}

#[test]
fn undo_refuses_a_different_link_under_the_same_name() -> TestResult {
    let s = Scratch::new()?;
    fs::write(s.path("a"), b"")?;
    fs::write(s.path("other"), b"")?;
    let items = undo_of(run(&[s.path("a")], None, Naming::LinkTo))?;
    fs::remove_file(s.path("Link to a"))?;
    symlink(s.path("other"), s.path("Link to a"))?;
    let report = undo(&items, &Sink::new(Unit::Items));
    assert_eq!(report.notice.map(|n| n.severity), Some(Severity::Error));
    assert_eq!(fs::read_link(s.path("Link to a"))?, s.path("other"));
    Ok(())
}

#[test]
fn undo_skips_a_link_that_is_already_gone_and_removes_the_rest() -> TestResult {
    let s = Scratch::new()?;
    fs::write(s.path("a"), b"")?;
    fs::write(s.path("b"), b"")?;
    let items = undo_of(run(&[s.path("a"), s.path("b")], None, Naming::LinkTo))?;
    fs::remove_file(s.path("Link to a"))?;
    let report = undo(&items, &Sink::new(Unit::Items));
    let notice = report.notice.as_ref().ok_or("a notice")?;
    assert_eq!(notice.severity, Severity::Warning);
    assert_eq!(
        notice.text,
        "Removed link \u{201c}Link to b\u{201d}; 1 skipped"
    );
    assert_eq!(report.details.len(), 1);
    assert!(fs::symlink_metadata(s.path("Link to b")).is_err());
    assert!(s.path("a").exists() && s.path("b").exists());
    Ok(())
}
