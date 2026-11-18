use std::error::Error;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::channel;
use std::time::{Duration, UNIX_EPOCH};

use chrono::{TimeZone, Utc};
use fileflux::core::ops::{ItemOutcome, ItemReport, TransferError};

use super::super::{
    DialogState, Job, MenuCommand, MenuItemSpec, MenuRow, MenuTarget, menu_rows, next_menu_index,
    run_job,
};
use super::*;
use crate::explorer::progress::Unit;

type TestResult = Result<(), Box<dyn Error>>;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A unique scratch directory under the system temp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> io::Result<Scratch> {
        let dir = std::env::temp_dir().join(format!(
            "fileflux-paste-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir)?;
        Ok(Scratch(dir))
    }

    fn dir(&self, rel: &str) -> io::Result<PathBuf> {
        let path = self.0.join(rel);
        fs::create_dir_all(&path)?;
        Ok(path)
    }

    fn file(&self, rel: &str, content: &str) -> io::Result<PathBuf> {
        let path = self.0.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, content)?;
        Ok(path)
    }

    fn ctx(&self) -> JobCtx {
        JobCtx::from_env(self.0.join("trash"), Vec::new())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn p(path: &str) -> PathBuf {
    PathBuf::from(path)
}

fn paths(list: &[&str]) -> Vec<PathBuf> {
    list.iter().map(|path| p(path)).collect()
}

// --- clipboard ------------------------------------------------------------------------

#[test]
fn every_copy_or_cut_replaces_the_clipboard_under_a_new_generation() {
    let mut clip = ClipboardState::default();
    assert!(clip.get().is_none());
    let first = clip.set(ClipMode::Copy, paths(&["/x/a"]));
    let second = clip.set(ClipMode::Cut, paths(&["/x/b", "/x/c"]));
    assert!(second > first, "{second} should be newer than {first}");
    assert_eq!(clip.last_generation(), second);
    let held = clip.get().cloned();
    assert_eq!(
        held,
        Some(Clipboard {
            mode: ClipMode::Cut,
            paths: paths(&["/x/b", "/x/c"]),
            generation: second,
        })
    );
    clip.clear();
    assert!(clip.get().is_none());
}

#[test]
fn only_cut_items_are_drawn_dimmed() {
    let mut clip = ClipboardState::default();
    clip.set(ClipMode::Copy, paths(&["/x/a"]));
    assert!(clip.cut_paths().is_empty(), "a copy dims nothing");
    clip.set(ClipMode::Cut, paths(&["/x/b", "/x/c"]));
    assert_eq!(
        clip.cut_paths(),
        paths(&["/x/b", "/x/c"]).into_iter().collect()
    );
    // Copying something else changes the clipboard: nothing stays dimmed.
    clip.set(ClipMode::Copy, paths(&["/x/d"]));
    assert!(clip.cut_paths().is_empty());
}

#[test]
fn a_finished_cut_paste_empties_the_clipboard_and_a_copy_keeps_it() {
    let mut clip = ClipboardState::default();
    let generation = clip.set(ClipMode::Cut, paths(&["/x/a", "/x/b"]));
    clip.finish_cut(generation, &paths(&["/x/a", "/x/b"]), false);
    assert!(clip.get().is_none());

    let generation = clip.set(ClipMode::Copy, paths(&["/x/a"]));
    clip.finish_cut(generation, &paths(&["/x/a"]), false);
    assert!(clip.get().is_some(), "after a copy the clipboard stays");
}

#[test]
fn a_cancelled_cut_keeps_only_what_did_not_move() {
    let mut clip = ClipboardState::default();
    let generation = clip.set(ClipMode::Cut, paths(&["/x/a", "/x/b", "/x/c"]));
    clip.finish_cut(generation, &paths(&["/x/a"]), true);
    assert_eq!(
        clip.get().map(|held| held.paths.clone()),
        Some(paths(&["/x/b", "/x/c"]))
    );
    clip.finish_cut(generation, &paths(&["/x/b", "/x/c"]), true);
    assert!(clip.get().is_none(), "nothing left to wait for");
}

#[test]
fn a_paste_that_finishes_after_the_clipboard_changed_leaves_it_alone() {
    let mut clip = ClipboardState::default();
    let old = clip.set(ClipMode::Cut, paths(&["/x/a"]));
    clip.set(ClipMode::Cut, paths(&["/x/z"]));
    clip.finish_cut(old, &paths(&["/x/a"]), false);
    assert_eq!(
        clip.get().map(|held| held.paths.clone()),
        Some(paths(&["/x/z"]))
    );
}

#[test]
fn the_system_clipboard_gets_the_paths_one_per_line() {
    assert_eq!(
        clipboard_text(&paths(&["/x/a b.txt", "/x/c"])),
        "/x/a b.txt\n/x/c"
    );
    assert_eq!(clipboard_text(&[]), "");
}

#[test]
fn a_clipboard_someone_else_wrote_makes_ours_stale() {
    assert!(!clipboard_is_stale("/x/a", Some("/x/a")));
    assert!(clipboard_is_stale("/x/a", Some("something else")));
    // No readable clipboard says nothing about ours.
    assert!(!clipboard_is_stale("/x/a", None));
}

#[test]
fn copy_and_cut_say_what_they_took() {
    assert_eq!(
        clip_notice(ClipMode::Copy, &paths(&["/x/a.txt"])),
        "Copied \u{201c}a.txt\u{201d} to the clipboard"
    );
    assert_eq!(
        clip_notice(ClipMode::Copy, &paths(&["/x/a", "/x/b"])),
        "Copied 2 items to the clipboard"
    );
    assert_eq!(
        clip_notice(ClipMode::Cut, &paths(&["/x/a.txt"])),
        "Cut \u{201c}a.txt\u{201d}; Paste moves it"
    );
    assert_eq!(
        clip_notice(ClipMode::Cut, &paths(&["/x/a", "/x/b", "/x/c"])),
        "Cut 3 items; Paste moves them"
    );
}

// --- menu states and paste target -------------------------------------------------------

fn target(selected: usize, can_paste: bool, folder_selected: bool) -> MenuTarget {
    MenuTarget {
        in_trash: false,
        selected,
        folder_is_empty: false,
        terminal: false,
        can_paste,
        folder_selected,
    }
}

fn spec(rows: &[MenuRow], command: MenuCommand) -> Option<&MenuItemSpec> {
    rows.iter().find_map(|row| match row {
        MenuRow::Item(item) if item.command == command => Some(item),
        _ => None,
    })
}

#[test]
fn copy_and_cut_are_enabled_on_a_selection_and_paste_follows_the_clipboard() {
    let empty = menu_rows(&target(2, false, false));
    let full = menu_rows(&target(2, true, false));
    for rows in [&empty, &full] {
        let enabled = |command| spec(rows, command).map(|item| item.enabled);
        assert_eq!(enabled(MenuCommand::Cut), Some(true));
        assert_eq!(enabled(MenuCommand::Copy), Some(true));
        assert!(spec(rows, MenuCommand::PasteIntoFolder).is_none());
    }
    assert_eq!(
        spec(&empty, MenuCommand::Paste).map(|item| (item.enabled, item.label)),
        Some((false, "Paste"))
    );
    assert_eq!(
        spec(&full, MenuCommand::Paste).map(|item| (item.enabled, item.shortcut)),
        Some((true, Some("Ctrl+V")))
    );
    let shortcut = |command| spec(&full, command).and_then(|item| item.shortcut);
    assert_eq!(shortcut(MenuCommand::Cut), Some("Ctrl+X"));
    assert_eq!(shortcut(MenuCommand::Copy), Some("Ctrl+C"));
}

#[test]
fn one_selected_folder_offers_paste_into_folder() {
    let rows = menu_rows(&target(1, true, true));
    let into = spec(&rows, MenuCommand::PasteIntoFolder);
    assert_eq!(
        into.map(|item| (item.label, item.enabled)),
        Some(("Paste into folder", true))
    );
    assert!(spec(&rows, MenuCommand::Paste).is_none());
    let disabled = menu_rows(&target(1, false, true));
    assert_eq!(
        spec(&disabled, MenuCommand::PasteIntoFolder).map(|item| item.enabled),
        Some(false)
    );
}

#[test]
fn the_menu_of_empty_space_offers_paste_but_no_cut_or_copy() {
    let rows = menu_rows(&target(0, true, false));
    assert_eq!(
        spec(&rows, MenuCommand::Paste).map(|item| item.enabled),
        Some(true)
    );
    assert!(spec(&rows, MenuCommand::Cut).is_none());
    assert!(spec(&rows, MenuCommand::Copy).is_none());
    let none = menu_rows(&target(0, false, false));
    assert_eq!(
        spec(&none, MenuCommand::Paste).map(|item| item.enabled),
        Some(false)
    );
}

#[test]
fn the_trash_view_never_offers_the_clipboard() {
    for selected in [0, 2] {
        let rows = menu_rows(&MenuTarget {
            in_trash: true,
            selected,
            folder_is_empty: false,
            terminal: false,
            can_paste: true,
            folder_selected: false,
        });
        for command in [
            MenuCommand::Cut,
            MenuCommand::Copy,
            MenuCommand::Paste,
            MenuCommand::PasteIntoFolder,
        ] {
            assert!(spec(&rows, command).is_none(), "{command:?} in the Trash");
        }
    }
}

#[test]
fn the_arrow_keys_skip_a_disabled_paste() {
    let rows = menu_rows(&target(0, false, false));
    let mut at = None;
    let mut visited = Vec::new();
    for _ in 0..10 {
        at = next_menu_index(&rows, at, true);
        if let Some(MenuRow::Item(item)) = at.and_then(|index| rows.get(index)) {
            visited.push(item.command);
        }
    }
    assert!(!visited.contains(&MenuCommand::Paste), "{visited:?}");
    assert!(visited.contains(&MenuCommand::NewFolder));
}

#[test]
fn ctrl_v_pastes_into_the_shown_folder_and_the_menu_into_the_selected_folder() {
    let current = p("/home/u/Docs");
    let one_folder = [(p("/home/u/Docs/sub"), true)];
    assert_eq!(
        paste_target(PasteInto::CurrentFolder, &current, &one_folder),
        Some(current.clone()),
        "Ctrl+V never goes into the selected folder"
    );
    assert_eq!(
        paste_target(PasteInto::SelectedFolder, &current, &one_folder),
        Some(p("/home/u/Docs/sub"))
    );
    assert_eq!(paste_target(PasteInto::SelectedFolder, &current, &[]), None);
    assert_eq!(
        paste_target(
            PasteInto::SelectedFolder,
            &current,
            &[(p("/home/u/Docs/a.txt"), false)]
        ),
        None,
        "a file is not a paste target"
    );
    assert_eq!(
        paste_target(
            PasteInto::SelectedFolder,
            &current,
            &[(p("/home/u/Docs/a"), true), (p("/home/u/Docs/b"), true)]
        ),
        None,
        "two folders are ambiguous"
    );
}

// --- plan and texts ---------------------------------------------------------------------

#[test]
fn pasting_nothing_says_so() {
    assert_eq!(
        plan_paste(ClipMode::Copy, &[], &p("/x/Pictures")),
        PastePlan::Nothing(Notice {
            severity: Severity::Info,
            text: "Nothing to paste".to_owned()
        })
    );
}

#[test]
fn moving_into_the_folder_the_items_are_in_is_a_noop_with_a_notice() {
    let plan = plan_paste(
        ClipMode::Cut,
        &paths(&["/x/Pictures/a.png", "/x/Pictures/b.png"]),
        &p("/x/Pictures"),
    );
    assert_eq!(
        plan,
        PastePlan::Nothing(Notice {
            severity: Severity::Info,
            text: "Already in Pictures \u{2014} nothing to move".to_owned()
        })
    );
}

#[test]
fn only_a_move_that_changes_nothing_is_a_noop() {
    // A copy into the same folder runs (it keeps both); so does a move of
    // items that are not all there.
    let same = paths(&["/x/Pictures/a.png"]);
    assert_eq!(
        plan_paste(ClipMode::Copy, &same, &p("/x/Pictures")),
        PastePlan::Run
    );
    let mixed = paths(&["/x/Pictures/a.png", "/x/Other/b.png"]);
    assert_eq!(
        plan_paste(ClipMode::Cut, &mixed, &p("/x/Pictures")),
        PastePlan::Run
    );
}

#[test]
fn the_progress_title_names_the_count_and_the_destination() {
    let many: Vec<PathBuf> = (0..12).map(|n| p(&format!("/x/f{n}"))).collect();
    assert_eq!(
        paste_title(ClipMode::Copy, &many, &p("/home/u/Pictures")),
        "Copying 12 items to Pictures"
    );
    assert_eq!(
        paste_title(ClipMode::Cut, &many, &p("/home/u/Pictures")),
        "Moving 12 items to Pictures"
    );
    assert_eq!(
        paste_title(ClipMode::Copy, &paths(&["/x/report.pdf"]), &p("/d/Docs")),
        "Copying \u{201c}report.pdf\u{201d} to Docs"
    );
    assert_eq!(
        paste_title(ClipMode::Cut, &paths(&["/x/a"]), &p("/")),
        "Moving \u{201c}a\u{201d} to /"
    );
}

fn ok(source: &str, dest: &str) -> ItemReport {
    ItemReport {
        source: p(source),
        outcome: ItemOutcome::Ok {
            dest: p(dest),
            bytes: 10,
        },
    }
}

fn skipped(source: &str) -> ItemReport {
    ItemReport {
        source: p(source),
        outcome: ItemOutcome::Skipped,
    }
}

fn failed(source: &str, error: TransferError) -> ItemReport {
    ItemReport {
        source: p(source),
        outcome: ItemOutcome::Failed(error),
    }
}

fn report_of(items: Vec<ItemReport>, cancelled: bool) -> TransferReport {
    TransferReport {
        items,
        bytes: 0,
        cancelled,
    }
}

fn summary(mode: ClipMode, items: Vec<ItemReport>, cancelled: bool) -> (Notice, Vec<String>) {
    transfer_summary(mode, &p("/d/Pictures"), &report_of(items, cancelled), &[])
}

#[test]
fn a_clean_transfer_says_what_was_copied_or_moved_and_where() {
    let (one, details) = summary(
        ClipMode::Copy,
        vec![ok("/x/a.png", "/d/Pictures/a.png")],
        false,
    );
    assert_eq!(one.text, "Copied \u{201c}a.png\u{201d} to Pictures");
    assert_eq!(one.severity, Severity::Success);
    assert!(details.is_empty());

    let items = vec![
        ok("/x/a", "/d/Pictures/a"),
        ok("/x/b", "/d/Pictures/b"),
        ok("/x/c", "/d/Pictures/c"),
    ];
    let (many, _) = summary(ClipMode::Cut, items, false);
    assert_eq!(many.text, "Moved 3 items to Pictures");
}

#[test]
fn skipped_and_failed_items_are_counted_with_details() {
    let items = vec![
        ok("/x/a", "/d/Pictures/a"),
        skipped("/x/b"),
        failed(
            "/x/c",
            TransferError::Io(io::Error::from(io::ErrorKind::PermissionDenied)),
        ),
    ];
    let (notice, details) = summary(ClipMode::Copy, items, false);
    assert_eq!(
        notice.text,
        "Copied \u{201c}a\u{201d} to Pictures; 1 skipped; 1 failed"
    );
    assert_eq!(notice.severity, Severity::Warning);
    assert_eq!(details.len(), 2);
    assert!(
        details
            .iter()
            .any(|line| line == "\u{201c}b\u{201d}: skipped")
    );
    assert!(
        details
            .iter()
            .any(|line| line.starts_with("\u{201c}c\u{201d}: ") && line.contains("permission"))
    );
}

#[test]
fn skipping_everything_is_a_notice_not_a_success() {
    let (notice, _) = summary(
        ClipMode::Copy,
        vec![skipped("/x/a"), skipped("/x/b")],
        false,
    );
    assert_eq!(notice.text, "Nothing was copied; 2 skipped");
    assert_eq!(notice.severity, Severity::Info);
}

#[test]
fn a_folder_into_itself_is_an_error_toast() {
    let (notice, details) = summary(
        ClipMode::Copy,
        vec![failed("/x/Docs", TransferError::IntoItself)],
        false,
    );
    assert_eq!(notice.severity, Severity::Error);
    assert_eq!(
        notice.text,
        "Could not copy \u{201c}Docs\u{201d} \u{2014} cannot place a folder inside itself"
    );
    assert_eq!(details.len(), 1);
    let (several, _) = summary(
        ClipMode::Cut,
        vec![
            failed("/x/a", TransferError::IntoItself),
            failed("/x/b", TransferError::IntoItself),
        ],
        false,
    );
    assert_eq!(several.text, "Could not move 2 items");
}

#[test]
fn a_cancelled_transfer_says_what_was_done_first() {
    let items = vec![
        ok("/x/a", "/d/Pictures/a"),
        failed("/x/b", TransferError::Cancelled),
        failed("/x/c", TransferError::Cancelled),
    ];
    let (notice, details) = summary(ClipMode::Copy, items, true);
    assert_eq!(
        notice.text,
        "Cancelled: copied \u{201c}a\u{201d} to Pictures"
    );
    assert_eq!(notice.severity, Severity::Info);
    assert!(details.is_empty(), "cancelled items are not failures");
    let (none, _) = summary(
        ClipMode::Cut,
        vec![failed("/x/a", TransferError::Cancelled)],
        true,
    );
    assert_eq!(none.text, "Cancelled: nothing was moved");
}

#[test]
fn replacing_says_that_undo_cannot_bring_the_old_item_back() {
    let report = report_of(vec![ok("/x/a", "/d/Pictures/a")], false);
    let (notice, _) = transfer_summary(
        ClipMode::Copy,
        &p("/d/Pictures"),
        &report,
        &[p("/d/Pictures/a")],
    );
    assert_eq!(
        notice.text,
        "Copied \u{201c}a\u{201d} to Pictures; 1 replaced (not restorable by Undo)"
    );
    // A replace that did not end in a placed item is not claimed.
    let report = report_of(vec![skipped("/x/a")], false);
    let (notice, _) = transfer_summary(
        ClipMode::Copy,
        &p("/d/Pictures"),
        &report,
        &[p("/d/Pictures/a")],
    );
    assert!(!notice.text.contains("replaced"), "{}", notice.text);
}

// --- conflict decisions ---------------------------------------------------------------------

fn answer(choice: Conflict, apply_all: bool) -> Option<ConflictAnswer> {
    Some(ConflictAnswer { choice, apply_all })
}

#[test]
fn each_dialog_button_gives_its_answer() {
    let cases = [
        (DialogChoice::Cancel, Conflict::Cancel),
        (DialogChoice::Confirm, Conflict::Replace),
        (DialogChoice::Skip, Conflict::Skip),
        (DialogChoice::KeepBoth, Conflict::KeepBoth),
    ];
    for (button, expected) in cases {
        assert_eq!(
            answer_for(button, false),
            ConflictAnswer {
                choice: expected,
                apply_all: false
            }
        );
        assert!(answer_for(button, true).apply_all);
    }
}

#[test]
fn without_apply_to_all_every_conflict_is_asked() {
    let mut policy = ConflictPolicy::default();
    let mut asked = 0;
    for _ in 0..3 {
        let choice = policy.resolve(|| {
            asked += 1;
            answer(Conflict::Skip, false)
        });
        assert_eq!(choice, Conflict::Skip);
    }
    assert_eq!(asked, 3);
}

#[test]
fn apply_to_all_answers_the_remaining_conflicts_without_asking() {
    let mut policy = ConflictPolicy::default();
    let mut asked = 0;
    let first = policy.resolve(|| {
        asked += 1;
        answer(Conflict::KeepBoth, true)
    });
    assert_eq!(first, Conflict::KeepBoth);
    for _ in 0..4 {
        let next = policy.resolve(|| {
            asked += 1;
            answer(Conflict::Replace, false)
        });
        assert_eq!(next, Conflict::KeepBoth);
    }
    assert_eq!(asked, 1);
}

#[test]
fn cancel_is_never_remembered_and_a_lost_answer_cancels() {
    let mut policy = ConflictPolicy::default();
    assert_eq!(
        policy.resolve(|| answer(Conflict::Cancel, true)),
        Conflict::Cancel
    );
    // The job is over after Cancel; if it were asked again it asks again.
    let mut asked = false;
    policy.resolve(|| {
        asked = true;
        answer(Conflict::Skip, false)
    });
    assert!(asked);
    assert_eq!(policy.resolve(|| None), Conflict::Cancel);
}

#[test]
fn the_conflict_dialog_describes_both_items_and_defaults_to_skip() {
    let now = Utc.with_ymd_and_hms(2026, 11, 18, 12, 0, 0).single();
    let Some(now) = now else {
        return;
    };
    let existing = ItemFacts {
        is_dir: false,
        size: 12_000,
        modified: Some(UNIX_EPOCH + Duration::from_secs(1_793_494_800)),
    };
    let incoming = ItemFacts {
        is_dir: true,
        size: 0,
        modified: None,
    };
    assert_eq!(
        describe_item(&existing, &now),
        "12 KB \u{b7} modified 1 Nov"
    );
    assert_eq!(
        describe_item(&incoming, &now),
        "Folder \u{b7} modified \u{2014}"
    );

    let dialog = DialogState::conflict(
        7,
        ConflictRequest {
            kind: DialogKind::Conflict {
                name: "report.pdf".to_owned(),
                folder: "Documents".to_owned(),
                existing: describe_item(&existing, &now),
                incoming: describe_item(&incoming, &now),
            },
        },
    );
    assert_eq!(
        dialog.text.title,
        "\u{201c}report.pdf\u{201d} already exists in Documents"
    );
    assert_eq!(dialog.text.confirm, "Replace");
    assert_eq!(
        dialog.text.names,
        [
            "Existing  12 KB \u{b7} modified 1 Nov".to_owned(),
            "Incoming  Folder \u{b7} modified \u{2014}".to_owned()
        ]
    );
    assert_eq!(dialog.conflict_job(), Some(7));
    assert_eq!(dialog.initial_focus(), DialogChoice::Skip);
    assert!(!dialog.initial_focus().is_danger());
}

// --- undo plan ----------------------------------------------------------------------------------

fn identity_of_all(_: &Path) -> Option<Identity> {
    Some(Identity { dev: 1, ino: 2 })
}

#[test]
fn undoing_a_copy_targets_the_created_copies() -> TestResult {
    let report = report_of(
        vec![
            ok("/x/a", "/d/a"),
            skipped("/x/b"),
            failed("/x/c", TransferError::Cancelled),
            ok("/x/d", "/d/d (2)"),
        ],
        true,
    );
    let plan = undo_plan(ClipMode::Copy, &report, &[], identity_of_all);
    let Some(UndoAction::Copy { items }) = plan else {
        return Err(format!("expected a copy undo, got {plan:?}").into());
    };
    let made: Vec<(&Path, &Path)> = items
        .iter()
        .map(|entry| (entry.from.as_path(), entry.to.as_path()))
        .collect();
    assert_eq!(
        made,
        [
            (Path::new("/x/a"), Path::new("/d/a")),
            (Path::new("/x/d"), Path::new("/d/d (2)"))
        ]
    );
    Ok(())
}

#[test]
fn undoing_a_move_returns_the_items_to_where_they_came_from() {
    let report = report_of(vec![ok("/x/a", "/d/a")], false);
    let plan = undo_plan(ClipMode::Cut, &report, &[], identity_of_all);
    assert!(
        matches!(&plan, Some(UndoAction::Move { items })
            if items.len() == 1
                && items.first().map(|entry| entry.from.as_path()) == Some(Path::new("/x/a"))
                && items.first().map(|entry| entry.to.as_path()) == Some(Path::new("/d/a"))),
        "{plan:?}"
    );
}

#[test]
fn replaced_items_are_never_part_of_an_undo() -> TestResult {
    let report = report_of(vec![ok("/x/a", "/d/a"), ok("/x/b", "/d/b")], false);
    for mode in [ClipMode::Copy, ClipMode::Cut] {
        let plan = undo_plan(mode, &report, &[p("/d/a")], identity_of_all);
        let items = match plan {
            Some(UndoAction::Copy { items } | UndoAction::Move { items }) => items,
            other => return Err(format!("expected an undo, got {other:?}").into()),
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items.first().map(|entry| entry.to.clone()), Some(p("/d/b")));
    }
    let only = report_of(vec![ok("/x/a", "/d/a")], false);
    assert_eq!(
        undo_plan(ClipMode::Copy, &only, &[p("/d/a")], identity_of_all),
        None
    );
    Ok(())
}

#[test]
fn an_item_without_an_identity_is_left_out_and_nothing_placed_means_no_undo() {
    let report = report_of(vec![ok("/x/a", "/d/a")], false);
    assert_eq!(undo_plan(ClipMode::Copy, &report, &[], |_| None), None);
    let nothing = report_of(vec![skipped("/x/a")], false);
    assert_eq!(
        undo_plan(ClipMode::Copy, &nothing, &[], identity_of_all),
        None
    );
}

// --- the worker, on disk --------------------------------------------------------------------------

struct Ran {
    report: Report,
    /// The kinds of the questions the worker asked, in order.
    asked: Vec<DialogKind>,
    sink: Sink,
}

/// Runs a paste with a thread playing the UI: it answers each question
/// from `answers`, and goes away (dropping the channel) when they run out.
fn paste(
    sources: Vec<PathBuf>,
    dest: &Path,
    mode: ClipMode,
    answers: Vec<ConflictAnswer>,
) -> Result<Ran, Box<dyn Error>> {
    let (request_tx, request_rx) = channel();
    let (reply_tx, reply_rx) = channel();
    let job = PasteJob {
        sources,
        dest: dest.to_path_buf(),
        mode,
        link: ConflictLink {
            requests: request_tx,
            replies: reply_rx,
        },
    };
    let sink = Sink::new(Unit::Bytes);
    let worker_sink = sink.clone();
    let worker = std::thread::spawn(move || run_paste(job, &worker_sink));
    let mut answers = answers.into_iter();
    let mut asked = Vec::new();
    while let Ok(request) = request_rx.recv() {
        asked.push(request.kind);
        match answers.next() {
            Some(next) => reply_tx.send(next)?,
            None => break,
        }
    }
    drop(reply_tx);
    drop(request_rx);
    let report = worker.join().map_err(|_| "the paste worker panicked")?;
    Ok(Ran {
        report,
        asked,
        sink,
    })
}

fn say(choice: Conflict, apply_all: bool) -> ConflictAnswer {
    ConflictAnswer { choice, apply_all }
}

fn text_of(report: &Report) -> &str {
    report
        .notice
        .as_ref()
        .map_or("", |notice| notice.text.as_str())
}

/// Three files and a folder with a nested file, under `src/`.
fn source_set(scratch: &Scratch) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    Ok(vec![
        scratch.file("src/a.txt", "alpha")?,
        scratch.file("src/b.bin", "bravo-bravo")?,
        scratch.file("src/c.md", "charlie")?,
        scratch.file("src/dir/inner/deep.txt", "deep")?,
    ])
}

#[test]
fn copying_files_and_a_folder_keeps_the_sources_and_the_contents() -> TestResult {
    let scratch = Scratch::new("copy")?;
    let sources = source_set(&scratch)?;
    let dest = scratch.dir("dest")?;
    // `dir` is the folder; `deep.txt` is inside it, not a fourth source.
    let top: Vec<PathBuf> = sources
        .iter()
        .take(3)
        .cloned()
        .chain([scratch.0.join("src/dir")])
        .collect();

    let ran = paste(top.clone(), &dest, ClipMode::Copy, Vec::new())?;
    assert_eq!(text_of(&ran.report), "Copied 4 items to dest");
    assert_eq!(fs::read_to_string(dest.join("a.txt"))?, "alpha");
    assert_eq!(fs::read_to_string(dest.join("b.bin"))?, "bravo-bravo");
    assert_eq!(fs::read_to_string(dest.join("c.md"))?, "charlie");
    assert_eq!(fs::read_to_string(dest.join("dir/inner/deep.txt"))?, "deep");
    for source in &top {
        assert!(source.exists(), "{} must still exist", source.display());
    }
    assert!(ran.asked.is_empty());
    let selected: Vec<PathBuf> = ["a.txt", "b.bin", "c.md", "dir"]
        .iter()
        .map(|name| dest.join(name))
        .collect();
    assert_eq!(ran.report.select, selected);
    assert!(matches!(&ran.report.undo, Some(UndoAction::Copy { items }) if items.len() == 4));
    assert!(ran.report.moved.is_empty(), "a copy moves nothing");
    // Byte progress ran to the end.
    let state = ran.sink.snapshot();
    assert_eq!(state.unit, Unit::Bytes);
    assert_eq!(state.total, 5 + 11 + 7 + 4);
    assert_eq!(state.done, state.total);
    Ok(())
}

#[test]
fn moving_keeps_the_inode_and_removes_the_sources() -> TestResult {
    let scratch = Scratch::new("move")?;
    let top = vec![scratch.file("src/a.txt", "alpha")?, {
        scratch.file("src/dir/deep.txt", "deep")?;
        scratch.0.join("src/dir")
    }];
    let dest = scratch.dir("dest")?;
    let before: Vec<u64> = top
        .iter()
        .map(|path| fs::metadata(path).map(|meta| meta.ino()))
        .collect::<Result<_, _>>()?;

    let ran = paste(top.clone(), &dest, ClipMode::Cut, Vec::new())?;
    assert_eq!(text_of(&ran.report), "Moved 2 items to dest");
    for source in &top {
        assert!(!source.exists(), "{} should be gone", source.display());
    }
    assert_eq!(fs::read_to_string(dest.join("a.txt"))?, "alpha");
    assert_eq!(fs::read_to_string(dest.join("dir/deep.txt"))?, "deep");
    let after = [
        fs::metadata(dest.join("a.txt"))?.ino(),
        fs::metadata(dest.join("dir"))?.ino(),
    ];
    assert_eq!(before, after, "a same-device move is a rename");
    assert_eq!(ran.report.moved, top);
    assert!(matches!(&ran.report.undo, Some(UndoAction::Move { items }) if items.len() == 2));
    Ok(())
}

#[test]
fn a_conflict_is_asked_with_both_items_described() -> TestResult {
    let scratch = Scratch::new("ask")?;
    let source = scratch.file("src/report.pdf", "new contents!")?;
    let dest = scratch.dir("Documents")?;
    scratch.file("Documents/report.pdf", "old")?;

    let ran = paste(
        vec![source],
        &dest,
        ClipMode::Copy,
        vec![say(Conflict::Skip, false)],
    )?;
    let [
        DialogKind::Conflict {
            name,
            folder,
            existing,
            incoming,
        },
    ] = ran.asked.as_slice()
    else {
        return Err(format!("expected one conflict question, got {:?}", ran.asked).into());
    };
    assert_eq!(name, "report.pdf");
    assert_eq!(folder, "Documents");
    assert!(existing.starts_with("3 B \u{b7} modified "), "{existing}");
    assert!(incoming.starts_with("13 B \u{b7} modified "), "{incoming}");
    Ok(())
}

#[test]
fn skip_leaves_both_the_target_and_the_source_alone() -> TestResult {
    let scratch = Scratch::new("skip")?;
    let source = scratch.file("src/report.pdf", "new")?;
    let dest = scratch.dir("dest")?;
    scratch.file("dest/report.pdf", "old")?;

    let ran = paste(
        vec![source.clone()],
        &dest,
        ClipMode::Cut,
        vec![say(Conflict::Skip, false)],
    )?;
    assert_eq!(fs::read_to_string(dest.join("report.pdf"))?, "old");
    assert_eq!(
        fs::read_to_string(&source)?,
        "new",
        "a skipped move keeps the source"
    );
    assert_eq!(text_of(&ran.report), "Nothing was moved; 1 skipped");
    assert!(ran.report.undo.is_none());
    assert!(ran.report.moved.is_empty());
    assert_eq!(fs::read_dir(&dest)?.count(), 1, "no stray temp files");
    Ok(())
}

#[test]
fn keep_both_places_the_item_under_a_new_name() -> TestResult {
    let scratch = Scratch::new("both")?;
    let source = scratch.file("src/report.pdf", "new")?;
    let dest = scratch.dir("dest")?;
    scratch.file("dest/report.pdf", "old")?;

    let ran = paste(
        vec![source],
        &dest,
        ClipMode::Copy,
        vec![say(Conflict::KeepBoth, false)],
    )?;
    assert_eq!(fs::read_to_string(dest.join("report.pdf"))?, "old");
    assert_eq!(fs::read_to_string(dest.join("report (2).pdf"))?, "new");
    assert_eq!(ran.report.select, [dest.join("report (2).pdf")]);
    assert!(matches!(&ran.report.undo, Some(UndoAction::Copy { items }) if items.len() == 1));
    Ok(())
}

#[test]
fn replace_replaces_says_so_and_cannot_be_undone() -> TestResult {
    let scratch = Scratch::new("replace")?;
    let source = scratch.file("src/report.pdf", "new")?;
    let other = scratch.file("src/keep.txt", "k")?;
    let dest = scratch.dir("dest")?;
    scratch.file("dest/report.pdf", "old")?;

    let ran = paste(
        vec![source, other],
        &dest,
        ClipMode::Copy,
        vec![say(Conflict::Replace, false)],
    )?;
    assert_eq!(fs::read_to_string(dest.join("report.pdf"))?, "new");
    assert_eq!(
        text_of(&ran.report),
        "Copied 2 items to dest; 1 replaced (not restorable by Undo)"
    );
    let Some(UndoAction::Copy { items }) = &ran.report.undo else {
        return Err("the copy that replaced nothing is still undoable".into());
    };
    let undoable: Vec<&Path> = items.iter().map(|entry| entry.to.as_path()).collect();
    assert_eq!(undoable, [dest.join("keep.txt")]);
    assert_eq!(fs::read_dir(&dest)?.count(), 2, "the aside copy is gone");
    Ok(())
}

#[test]
fn apply_to_all_asks_once_and_covers_the_rest() -> TestResult {
    let scratch = Scratch::new("all")?;
    let sources = vec![
        scratch.file("src/a.txt", "A")?,
        scratch.file("src/b.txt", "B")?,
        scratch.file("src/c.txt", "C")?,
    ];
    let dest = scratch.dir("dest")?;
    for name in ["a.txt", "b.txt", "c.txt"] {
        scratch.file(&format!("dest/{name}"), "old")?;
    }
    let ran = paste(
        sources,
        &dest,
        ClipMode::Copy,
        vec![say(Conflict::Skip, true)],
    )?;
    assert_eq!(ran.asked.len(), 1, "apply to all asks once");
    for name in ["a.txt", "b.txt", "c.txt"] {
        assert_eq!(fs::read_to_string(dest.join(name))?, "old");
    }
    assert_eq!(text_of(&ran.report), "Nothing was copied; 3 skipped");

    let scratch = Scratch::new("all-both")?;
    let sources = vec![
        scratch.file("src/a.txt", "A")?,
        scratch.file("src/b.txt", "B")?,
    ];
    let dest = scratch.dir("dest")?;
    scratch.file("dest/a.txt", "old")?;
    scratch.file("dest/b.txt", "old")?;
    let ran = paste(
        sources,
        &dest,
        ClipMode::Copy,
        vec![say(Conflict::KeepBoth, true)],
    )?;
    assert_eq!(ran.asked.len(), 1);
    assert_eq!(fs::read_to_string(dest.join("a (2).txt"))?, "A");
    assert_eq!(fs::read_to_string(dest.join("b (2).txt"))?, "B");
    Ok(())
}

#[test]
fn cancelling_the_question_stops_the_whole_job_and_touches_nothing_more() -> TestResult {
    let scratch = Scratch::new("cancel")?;
    let sources = vec![
        scratch.file("src/1-free.txt", "free")?,
        scratch.file("src/2-taken.txt", "new")?,
        scratch.file("src/3-free.txt", "free too")?,
    ];
    let dest = scratch.dir("dest")?;
    scratch.file("dest/2-taken.txt", "old")?;

    let ran = paste(
        sources.clone(),
        &dest,
        ClipMode::Cut,
        vec![say(Conflict::Cancel, false)],
    )?;
    assert!(ran.report.cancelled);
    assert_eq!(
        fs::read_to_string(dest.join("1-free.txt"))?,
        "free",
        "done stays done"
    );
    assert_eq!(fs::read_to_string(dest.join("2-taken.txt"))?, "old");
    assert!(
        !dest.join("3-free.txt").exists(),
        "nothing after the cancel"
    );
    assert!(sources.get(2).is_some_and(|path| path.exists()));
    assert!(sources.get(1).is_some_and(|path| path.exists()));
    assert_eq!(
        ran.report.moved,
        [sources.first().cloned().ok_or("source")?]
    );
    assert_eq!(
        text_of(&ran.report),
        "Cancelled: moved \u{201c}1-free.txt\u{201d} to dest"
    );
    for entry in fs::read_dir(&dest)? {
        let name = entry?.file_name();
        assert!(
            !name.to_string_lossy().starts_with(".fileflux-"),
            "{name:?}"
        );
    }
    Ok(())
}

#[test]
fn a_ui_that_goes_away_cancels_instead_of_guessing() -> TestResult {
    let scratch = Scratch::new("gone")?;
    let source = scratch.file("src/a.txt", "new")?;
    let dest = scratch.dir("dest")?;
    scratch.file("dest/a.txt", "old")?;
    // No answers: the "UI" drops its end after the first question.
    let ran = paste(vec![source.clone()], &dest, ClipMode::Cut, Vec::new())?;
    assert!(ran.report.cancelled);
    assert_eq!(fs::read_to_string(dest.join("a.txt"))?, "old");
    assert!(source.exists());
    Ok(())
}

#[test]
fn cancelling_from_the_progress_card_unblocks_a_waiting_worker() -> TestResult {
    let scratch = Scratch::new("card-cancel")?;
    let source = scratch.file("src/a.txt", "new")?;
    let dest = scratch.dir("dest")?;
    scratch.file("dest/a.txt", "old")?;

    let (request_tx, request_rx) = channel();
    let (reply_tx, reply_rx) = channel::<ConflictAnswer>();
    let job = PasteJob {
        sources: vec![source.clone()],
        dest: dest.clone(),
        mode: ClipMode::Copy,
        link: ConflictLink {
            requests: request_tx,
            replies: reply_rx,
        },
    };
    let sink = Sink::new(Unit::Bytes);
    let worker_sink = sink.clone();
    let worker = std::thread::spawn(move || run_paste(job, &worker_sink));
    let question = request_rx.recv_timeout(Duration::from_secs(10))?;
    assert!(matches!(question.kind, DialogKind::Conflict { .. }));
    // The UI is still thinking (the reply sender is alive); Cancel is
    // pressed on the card.
    sink.request_cancel();
    let report = worker.join().map_err(|_| "worker panicked")?;
    drop(reply_tx);
    assert!(report.cancelled);
    assert_eq!(fs::read_to_string(dest.join("a.txt"))?, "old");
    Ok(())
}

#[test]
fn pasting_a_copy_into_its_own_folder_keeps_both_without_asking() -> TestResult {
    let scratch = Scratch::new("same-copy")?;
    let source = scratch.file("here/report.pdf", "data")?;
    let folder = scratch.dir("here")?;
    let ran = paste(vec![source], &folder, ClipMode::Copy, Vec::new())?;
    assert!(ran.asked.is_empty(), "no question for the same folder");
    assert_eq!(fs::read_to_string(folder.join("report.pdf"))?, "data");
    assert_eq!(fs::read_to_string(folder.join("report (2).pdf"))?, "data");
    assert_eq!(
        text_of(&ran.report),
        "Copied \u{201c}report.pdf\u{201d} to here"
    );
    Ok(())
}

#[test]
fn a_folder_pasted_into_itself_is_an_error_and_nothing_changes() -> TestResult {
    let scratch = Scratch::new("into-self")?;
    scratch.file("Docs/inner/a.txt", "a")?;
    let docs = scratch.0.join("Docs");
    for (mode, verb) in [(ClipMode::Copy, "copy"), (ClipMode::Cut, "move")] {
        let ran = paste(vec![docs.clone()], &docs, mode, Vec::new())?;
        assert_eq!(
            ran.report.notice.as_ref().map(|notice| notice.severity),
            Some(Severity::Error)
        );
        assert_eq!(
            text_of(&ran.report),
            format!(
                "Could not {verb} \u{201c}Docs\u{201d} \u{2014} cannot place a folder inside itself"
            )
        );
        assert!(ran.report.undo.is_none());
        let inside = scratch.0.join("Docs/inner");
        assert_eq!(
            fs::read_dir(&docs)?.count(),
            1,
            "nothing was created in Docs"
        );
        assert_eq!(fs::read_to_string(inside.join("a.txt"))?, "a");
    }
    // Into one of its own subfolders too.
    let inner = docs.join("inner");
    let ran = paste(vec![docs.clone()], &inner, ClipMode::Copy, Vec::new())?;
    assert_eq!(
        ran.report.notice.as_ref().map(|notice| notice.severity),
        Some(Severity::Error)
    );
    assert_eq!(fs::read_dir(&inner)?.count(), 1);
    Ok(())
}

#[test]
fn symlinks_are_copied_as_links() -> TestResult {
    let scratch = Scratch::new("links")?;
    let target = scratch.file("src/real.txt", "real")?;
    let link = scratch.0.join("src/link");
    std::os::unix::fs::symlink(&target, &link)?;
    let dest = scratch.dir("dest")?;
    paste(vec![link], &dest, ClipMode::Copy, Vec::new())?;
    let copied = fs::symlink_metadata(dest.join("link"))?;
    assert!(copied.file_type().is_symlink());
    assert_eq!(fs::read_link(dest.join("link"))?, target);
    Ok(())
}

// --- undo, on disk -------------------------------------------------------------------------------------

fn undo_of(ran: &Ran) -> Result<UndoAction, Box<dyn Error>> {
    ran.report
        .undo
        .clone()
        .ok_or_else(|| "the paste recorded no undo".into())
}

fn run_undo(action: &UndoAction, scratch: &Scratch) -> Report {
    run_job(
        Job::Undo(action.clone()),
        &scratch.ctx(),
        &Sink::new(Unit::Items),
    )
}

#[test]
fn undoing_a_copy_moves_the_copies_to_the_trash_and_keeps_the_sources() -> TestResult {
    let scratch = Scratch::new("undo-copy")?;
    let sources = vec![
        scratch.file("src/a.txt", "A")?,
        scratch.file("src/b.txt", "B")?,
    ];
    let dest = scratch.dir("dest")?;
    let ran = paste(sources.clone(), &dest, ClipMode::Copy, Vec::new())?;

    let report = run_undo(&undo_of(&ran)?, &scratch);
    assert_eq!(text_of(&report), "Moved 2 copies to Trash");
    assert!(!dest.join("a.txt").exists());
    assert!(!dest.join("b.txt").exists());
    for source in &sources {
        assert!(source.exists());
    }
    let trashed = fs::read_dir(scratch.0.join("trash/files"))?.count();
    assert_eq!(trashed, 2, "to the Trash, never deleted");
    Ok(())
}

#[test]
fn undoing_a_copy_refuses_a_copy_that_was_swapped_for_another_file() -> TestResult {
    let scratch = Scratch::new("undo-copy-swap")?;
    let source = scratch.file("src/a.txt", "A")?;
    let dest = scratch.dir("dest")?;
    let ran = paste(vec![source], &dest, ClipMode::Copy, Vec::new())?;
    // Someone replaces the copy with a different file of the same name.
    fs::remove_file(dest.join("a.txt"))?;
    fs::write(dest.join("a.txt"), "someone else's")?;

    let report = run_undo(&undo_of(&ran)?, &scratch);
    assert_eq!(
        report.notice.as_ref().map(|notice| notice.severity),
        Some(Severity::Error)
    );
    assert!(
        text_of(&report).starts_with("Can't undo: "),
        "{}",
        text_of(&report)
    );
    assert!(text_of(&report).contains("changed since it was copied"));
    assert_eq!(fs::read_to_string(dest.join("a.txt"))?, "someone else's");
    Ok(())
}

#[test]
fn undoing_a_move_puts_every_item_back_where_it_was() -> TestResult {
    let scratch = Scratch::new("undo-move")?;
    let a = scratch.file("src/a.txt", "A")?;
    let dir_file = scratch.file("src/dir/x.txt", "X")?;
    let dir = scratch.0.join("src/dir");
    let dest = scratch.dir("dest")?;
    let inode = fs::metadata(&a)?.ino();
    let ran = paste(
        vec![a.clone(), dir.clone()],
        &dest,
        ClipMode::Cut,
        Vec::new(),
    )?;
    assert!(!a.exists());

    let report = run_undo(&undo_of(&ran)?, &scratch);
    assert_eq!(text_of(&report), "Moved 2 items back");
    assert_eq!(fs::read_to_string(&a)?, "A");
    assert_eq!(fs::metadata(&a)?.ino(), inode);
    assert_eq!(fs::read_to_string(&dir_file)?, "X");
    assert!(!dest.join("a.txt").exists());
    assert!(!dest.join("dir").exists());
    assert_eq!(report.select, [a, dir]);
    assert!(report.undo.is_none());
    Ok(())
}

#[test]
fn undoing_a_move_never_replaces_what_took_the_original_place() -> TestResult {
    let scratch = Scratch::new("undo-move-taken")?;
    let a = scratch.file("src/a.txt", "A")?;
    let b = scratch.file("src/b.txt", "B")?;
    let dest = scratch.dir("dest")?;
    let ran = paste(vec![a.clone(), b.clone()], &dest, ClipMode::Cut, Vec::new())?;
    // A new file now sits where a.txt was.
    fs::write(&a, "newcomer")?;

    let report = run_undo(&undo_of(&ran)?, &scratch);
    assert_eq!(fs::read_to_string(&a)?, "newcomer", "never replaced");
    assert_eq!(
        fs::read_to_string(dest.join("a.txt"))?,
        "A",
        "left where it is"
    );
    assert_eq!(fs::read_to_string(&b)?, "B", "the other one still returns");
    assert_eq!(
        text_of(&report),
        "Moved \u{201c}b.txt\u{201d} back; 1 skipped"
    );
    assert_eq!(
        report.details,
        ["\u{201c}a.txt\u{201d} already exists where it came from"]
    );
    // What could not return stays undoable, so it can be retried.
    assert!(matches!(&report.undo, Some(UndoAction::Move { items }) if items.len() == 1));
    Ok(())
}

#[test]
fn undoing_a_move_refuses_an_item_that_is_no_longer_the_one_moved() -> TestResult {
    let scratch = Scratch::new("undo-move-swap")?;
    let a = scratch.file("src/a.txt", "A")?;
    let dest = scratch.dir("dest")?;
    let ran = paste(vec![a.clone()], &dest, ClipMode::Cut, Vec::new())?;
    fs::remove_file(dest.join("a.txt"))?;
    fs::write(dest.join("a.txt"), "someone else's")?;

    let report = run_undo(&undo_of(&ran)?, &scratch);
    assert!(
        text_of(&report).starts_with("Can't undo: "),
        "{}",
        text_of(&report)
    );
    assert!(!a.exists());
    assert_eq!(fs::read_to_string(dest.join("a.txt"))?, "someone else's");
    assert!(
        report.undo.is_none(),
        "an item that changed is gone for good"
    );
    Ok(())
}

#[test]
fn undoing_a_move_whose_origin_folder_vanished_says_so() -> TestResult {
    let scratch = Scratch::new("undo-move-nofolder")?;
    let a = scratch.file("src/a.txt", "A")?;
    let dest = scratch.dir("dest")?;
    let ran = paste(vec![a], &dest, ClipMode::Cut, Vec::new())?;
    fs::remove_dir(scratch.0.join("src"))?;

    let report = run_undo(&undo_of(&ran)?, &scratch);
    assert!(
        text_of(&report).starts_with("Can't undo: "),
        "{}",
        text_of(&report)
    );
    assert_eq!(fs::read_to_string(dest.join("a.txt"))?, "A");
    Ok(())
}
