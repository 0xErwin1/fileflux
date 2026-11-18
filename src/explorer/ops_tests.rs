use std::fs;
use std::io;
use std::os::unix::fs as unix_fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use fileflux::core::trash::TrashKind;

use super::super::progress::Unit;
use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A unique scratch directory under the system temp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> io::Result<Scratch> {
        let tick = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
            + u128::from(SEQ.fetch_add(1, Ordering::Relaxed));
        let dir = std::env::temp_dir().join(format!(
            "fileflux-explorer-ops-{tag}-{}-{tick}",
            std::process::id()
        ));
        fs::create_dir_all(&dir)?;
        Ok(Scratch(dir))
    }

    fn dir(&self, name: &str) -> io::Result<PathBuf> {
        let path = self.0.join(name);
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

    fn trash(&self) -> PathBuf {
        self.0.join("trash")
    }

    fn ctx(&self) -> JobCtx {
        JobCtx::from_env(self.trash(), Vec::new())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn sink() -> Sink {
    Sink::new(Unit::Items)
}

fn text(report: &Report) -> &str {
    report
        .notice
        .as_ref()
        .map_or("", |notice| notice.text.as_str())
}

fn severity(report: &Report) -> Option<Severity> {
    report.notice.as_ref().map(|notice| notice.severity)
}

fn no_trash_ctx(root: PathBuf) -> JobCtx {
    JobCtx {
        trash_root: root,
        target_for: Box::new(|_| Err(TrashError::NoTrash("no trash here".to_owned()))),
    }
}

fn os(name: &str) -> OsString {
    OsString::from(name)
}

// --- undo stack --------------------------------------------------------------

fn rename_action(old: &str, new: &str) -> UndoAction {
    UndoAction::Rename {
        old: PathBuf::from(old),
        new: PathBuf::from(new),
    }
}

#[test]
fn the_undo_stack_pops_the_newest_and_take_removes_only_that_entry() {
    let mut stack = UndoStack::default();
    let a = stack.push(rename_action("/x/a", "/x/b"));
    let b = stack.push(UndoAction::Create {
        path: PathBuf::from("/x/new"),
        kind: NewKind::Folder,
    });
    let c = stack.push(rename_action("/x/c", "/x/d"));
    assert_eq!(stack.len(), 3);
    assert!(a < b && b < c, "ids increase so a toast targets one entry");

    assert_eq!(stack.take(b).map(|entry| entry.id), Some(b));
    assert_eq!(stack.take(b), None, "a taken entry is gone");
    assert_eq!(stack.len(), 2);

    assert_eq!(stack.pop().map(|entry| entry.id), Some(c));
    assert_eq!(stack.pop().map(|entry| entry.id), Some(a));
    assert_eq!(stack.pop(), None);
    assert_eq!(stack.len(), 0);
}

#[test]
fn the_undo_stack_is_bounded_and_forgets_the_oldest() {
    let mut stack = UndoStack::default();
    for n in 0..UNDO_LIMIT + 5 {
        stack.push(rename_action("/x/a", &format!("/x/{n}")));
    }
    assert_eq!(stack.len(), UNDO_LIMIT);
    let mut oldest = None;
    while let Some(entry) = stack.pop() {
        oldest = Some(entry.id);
    }
    assert_eq!(oldest, Some(6), "the first five entries were dropped");
}

// --- rename helpers ------------------------------------------------------------

#[test]
fn the_rename_selection_covers_the_stem_only_for_files() {
    assert_eq!(stem_range("report.pdf", false), 0..6);
    assert_eq!(stem_range("archive.tar.gz", false), 0..11);
    assert_eq!(
        stem_range(".bashrc", false),
        0..7,
        "a dotfile has no extension"
    );
    assert_eq!(stem_range("Makefile", false), 0..8);
    assert_eq!(stem_range("name.", false), 0..4);
    assert_eq!(stem_range("Photos.2024", true), 0..11, "folders select all");
    assert_eq!(stem_range("ñandú.txt", false), 0..7, "byte offsets");
}

#[test]
fn a_typed_name_is_checked_before_anything_is_renamed() {
    let siblings = [os("a.txt"), os("b.txt")];
    let check = |typed: &str| {
        check_rename(
            OsStr::new("a.txt"),
            typed,
            siblings.iter().map(|n| n.as_os_str()),
        )
    };

    assert_eq!(check("a.txt"), RenameCheck::Unchanged);
    assert_eq!(check("  a.txt "), RenameCheck::Unchanged, "trimmed");
    let empty = RenameCheck::Invalid("The name can't be empty.".to_owned());
    assert_eq!(check(""), empty);
    assert_eq!(check("   "), empty);
    assert_eq!(
        check("x/y"),
        RenameCheck::Invalid("A name cannot contain \"/\"".to_owned())
    );
    assert!(matches!(check(".."), RenameCheck::Invalid(_)));
    assert_eq!(
        check("b.txt"),
        RenameCheck::Invalid("\u{201c}b.txt\u{201d} already exists here".to_owned())
    );
    assert_eq!(check("c.txt"), RenameCheck::Rename(os("c.txt")));
    assert_eq!(check(" c.txt "), RenameCheck::Rename(os("c.txt")));
}

// --- context menu model ---------------------------------------------------------

fn flat(rows: &[MenuRow]) -> Vec<String> {
    rows.iter()
        .map(|row| match row {
            MenuRow::Separator => "-".to_owned(),
            MenuRow::Item(item) => item.label.to_owned(),
        })
        .collect()
}

fn item(rows: &[MenuRow], command: MenuCommand) -> Option<&MenuItemSpec> {
    rows.iter().find_map(|row| match row {
        MenuRow::Item(item) if item.command == command => Some(item),
        _ => None,
    })
}

fn target(in_trash: bool, selected: usize, folder_is_empty: bool, terminal: bool) -> MenuTarget {
    MenuTarget {
        in_trash,
        selected,
        folder_is_empty,
        terminal,
    }
}

#[test]
fn the_menu_of_one_selected_entry_matches_the_artboard() {
    let rows = menu_rows(&target(false, 1, false, true));
    assert_eq!(
        flat(&rows),
        [
            "Open",
            "-",
            "Rename",
            "Copy path",
            "-",
            "Properties",
            "Move to Trash",
            "Delete permanently…"
        ]
    );
    let shortcut = |command| item(&rows, command).and_then(|item| item.shortcut);
    assert_eq!(shortcut(MenuCommand::Open), Some("Enter"));
    assert_eq!(shortcut(MenuCommand::Rename), Some("F2"));
    assert_eq!(shortcut(MenuCommand::CopyPath), Some("Ctrl+Shift+C"));
    assert_eq!(shortcut(MenuCommand::MoveToTrash), Some("Del"));
    assert_eq!(shortcut(MenuCommand::DeletePermanently), Some("Shift+Del"));
    let danger = |command| item(&rows, command).is_some_and(|item| item.danger);
    assert!(danger(MenuCommand::MoveToTrash));
    assert!(danger(MenuCommand::DeletePermanently));
    assert!(!danger(MenuCommand::Open));
}

#[test]
fn rename_is_offered_for_a_single_selection_only() {
    let rows = menu_rows(&target(false, 3, false, true));
    assert!(item(&rows, MenuCommand::Rename).is_none());
    assert!(item(&rows, MenuCommand::MoveToTrash).is_some());
    assert!(item(&rows, MenuCommand::DeletePermanently).is_some());
}

#[test]
fn the_menu_of_empty_space_offers_create_and_terminal_only_when_it_works() {
    let rows = menu_rows(&target(false, 0, false, true));
    assert_eq!(
        flat(&rows),
        [
            "New folder",
            "New file",
            "-",
            "Open terminal here",
            "Copy folder path"
        ]
    );
    assert_eq!(
        item(&rows, MenuCommand::NewFolder).and_then(|item| item.shortcut),
        Some("Ctrl+Shift+N")
    );
    let without = menu_rows(&target(false, 0, false, false));
    assert_eq!(
        flat(&without),
        ["New folder", "New file", "-", "Copy folder path"],
        "no dead 'Open terminal here' item"
    );
}

#[test]
fn the_trash_menu_restores_and_deletes_and_never_creates() {
    let rows = menu_rows(&target(true, 2, false, true));
    assert_eq!(
        flat(&rows),
        ["Restore", "-", "Properties", "Delete permanently…"]
    );
    assert!(item(&rows, MenuCommand::MoveToTrash).is_none());
    assert!(item(&rows, MenuCommand::NewFolder).is_none());

    let empty_space = menu_rows(&target(true, 0, false, true));
    assert_eq!(flat(&empty_space), ["Empty Trash"]);
    assert!(item(&empty_space, MenuCommand::EmptyTrash).is_some_and(|item| item.danger));
    assert!(
        menu_rows(&target(true, 0, true, true)).is_empty(),
        "an empty Trash has nothing to offer"
    );
}

#[test]
fn the_arrow_keys_walk_the_items_skip_separators_and_wrap() {
    let rows = menu_rows(&target(false, 1, false, true));
    // Open, -, Rename, Copy path, -, Properties, Move to Trash, Delete…
    assert_eq!(next_menu_index(&rows, None, true), Some(0));
    assert_eq!(
        next_menu_index(&rows, Some(0), true),
        Some(2),
        "skips the separator"
    );
    assert_eq!(next_menu_index(&rows, Some(3), true), Some(5));
    assert_eq!(next_menu_index(&rows, Some(7), true), Some(0), "wraps down");
    assert_eq!(next_menu_index(&rows, None, false), Some(7));
    assert_eq!(next_menu_index(&rows, Some(2), false), Some(0));
    assert_eq!(next_menu_index(&rows, Some(0), false), Some(7), "wraps up");
    assert_eq!(next_menu_index(&[MenuRow::Separator], None, true), None);
    assert_eq!(next_menu_index(&[], None, false), None);
}

// --- confirmation text ---------------------------------------------------------

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn the_permanent_delete_dialog_names_the_items_and_the_count() {
    let one = dialog_text(&DialogKind::DeletePermanently, &names(&["report.pdf"]));
    assert_eq!(one.title, "Delete \u{201c}report.pdf\u{201d} permanently?");
    assert_eq!(
        one.body,
        "\u{201c}report.pdf\u{201d} will be deleted for good. This cannot be undone."
    );
    assert_eq!(one.confirm, "Delete permanently");
    assert_eq!(one.names, ["report.pdf"]);

    let three = dialog_text(&DialogKind::DeletePermanently, &names(&["a", "b", "c"]));
    assert_eq!(three.title, "Delete 3 items permanently?");
    assert_eq!(
        three.body,
        "These 3 items will be deleted for good. This cannot be undone."
    );
    assert_eq!(three.names, ["a", "b", "c"]);

    let seven = dialog_text(
        &DialogKind::DeletePermanently,
        &names(&["a", "b", "c", "d", "e", "f", "g"]),
    );
    assert_eq!(seven.names, ["a", "b", "c", "d", "e", "\u{2026}and 2 more"]);
}

#[test]
fn the_no_trash_dialog_asks_instead_of_deleting_silently() {
    let one = dialog_text(&DialogKind::NoTrash, &names(&["x.bin"]));
    assert_eq!(one.title, "No Trash available");
    assert_eq!(
        one.body,
        "\u{201c}x.bin\u{201d} can't be moved to the Trash. Delete it permanently instead? This cannot be undone."
    );
    assert_eq!(one.confirm, "Delete permanently");
    let two = dialog_text(&DialogKind::NoTrash, &names(&["a", "b"]));
    assert_eq!(
        two.body,
        "These 2 items can't be moved to the Trash. Delete them permanently instead? This cannot be undone."
    );
}

#[test]
fn the_trash_view_dialogs_say_the_items_leave_the_trash_for_good() {
    let one = dialog_text(&DialogKind::DeleteFromTrash, &names(&["x"]));
    assert_eq!(one.title, "Delete \u{201c}x\u{201d} permanently?");
    assert_eq!(
        one.body,
        "\u{201c}x\u{201d} will be deleted from the Trash for good. This cannot be undone."
    );
    let empty = dialog_text(&DialogKind::EmptyTrash { count: 3 }, &[]);
    assert_eq!(empty.title, "Empty the Trash?");
    assert_eq!(
        empty.body,
        "All 3 items in the Trash will be deleted for good. This cannot be undone."
    );
    assert_eq!(empty.confirm, "Empty Trash");
    let single = dialog_text(&DialogKind::EmptyTrash { count: 1 }, &[]);
    assert_eq!(
        single.body,
        "The only item in the Trash will be deleted for good. This cannot be undone."
    );
}

// --- trash target choice -------------------------------------------------------

#[test]
fn the_topdir_is_the_longest_mount_that_contains_the_path() {
    let mounts = [
        PathBuf::from("/"),
        PathBuf::from("/mnt"),
        PathBuf::from("/mnt/usb"),
    ];
    assert_eq!(
        topdir_of(&mounts, Path::new("/mnt/usb/a/b")),
        Some(PathBuf::from("/mnt/usb"))
    );
    assert_eq!(
        topdir_of(&mounts, Path::new("/mnt/usbx/a")),
        Some(PathBuf::from("/mnt")),
        "component-wise, not a string prefix"
    );
    assert_eq!(
        topdir_of(&mounts, Path::new("/home/me")),
        Some(PathBuf::from("/"))
    );
    assert_eq!(topdir_of(&[], Path::new("/home/me")), None);
}

#[test]
fn a_file_on_the_home_device_goes_to_the_home_trash() -> TestResult {
    let scratch = Scratch::new("target")?;
    let file = scratch.file("a.txt", "x")?;
    let ctx = scratch.ctx();
    let target = (ctx.target_for)(&file)?;
    assert_eq!(target.root, scratch.trash());
    assert_eq!(target.kind, TrashKind::Home);
    Ok(())
}

// --- terminal ------------------------------------------------------------------

fn executable(dir: &Path, name: &str, mode: u32) -> io::Result<PathBuf> {
    let path = dir.join(name);
    fs::write(&path, "#!/bin/sh\n")?;
    fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
    Ok(path)
}

#[test]
fn the_terminal_is_terminal_then_xdg_terminal_exec_and_must_exist() -> TestResult {
    let scratch = Scratch::new("terminal")?;
    let bin = scratch.dir("bin")?;
    let kitty = executable(&bin, "kitty", 0o755)?;
    let exec = executable(&bin, "xdg-terminal-exec", 0o755)?;
    executable(&bin, "plain", 0o644)?;
    let path_var = bin.as_os_str();

    assert_eq!(
        pick_terminal(Some(OsStr::new("kitty")), Some(path_var)),
        Some(kitty)
    );
    assert_eq!(
        pick_terminal(Some(OsStr::new("missing")), Some(path_var)),
        Some(exec.clone()),
        "an unusable $TERMINAL falls through"
    );
    assert_eq!(
        pick_terminal(Some(OsStr::new("plain")), Some(path_var)),
        Some(exec.clone()),
        "a non-executable file is not a terminal"
    );
    assert_eq!(
        pick_terminal(Some(OsStr::new("")), Some(path_var)),
        Some(exec)
    );
    assert_eq!(pick_terminal(None, Some(scratch.0.as_os_str())), None);
    assert_eq!(pick_terminal(None, None), None);
    Ok(())
}

// --- jobs: create / rename ------------------------------------------------------

#[test]
fn new_folder_and_file_get_unique_names_and_ask_for_a_rename() -> TestResult {
    let scratch = Scratch::new("create")?;
    let dir = scratch.dir("work")?;
    let ctx = scratch.ctx();

    let first = run_job(
        Job::Create {
            dir: dir.clone(),
            kind: NewKind::Folder,
        },
        &ctx,
        &sink(),
    );
    let made = dir.join("Untitled folder");
    assert!(made.is_dir());
    assert_eq!(first.select, std::slice::from_ref(&made));
    assert!(first.begin_rename);
    assert_eq!(
        first.undo,
        Some(UndoAction::Create {
            path: made,
            kind: NewKind::Folder
        })
    );

    let second = run_job(
        Job::Create {
            dir: dir.clone(),
            kind: NewKind::Folder,
        },
        &ctx,
        &sink(),
    );
    assert_eq!(second.select, [dir.join("Untitled folder (2)")]);

    let file = run_job(
        Job::Create {
            dir: dir.clone(),
            kind: NewKind::File,
        },
        &ctx,
        &sink(),
    );
    let made = dir.join("Untitled file");
    assert_eq!(fs::metadata(&made)?.len(), 0);
    assert!(made.is_file());
    assert_eq!(file.select, [made]);
    Ok(())
}

#[test]
fn a_rename_records_its_undo_and_never_replaces() -> TestResult {
    let scratch = Scratch::new("rename")?;
    let a = scratch.file("a.txt", "keep")?;
    let c = scratch.file("c.txt", "other")?;
    let ctx = scratch.ctx();

    let renamed = run_job(
        Job::Rename {
            path: a.clone(),
            new_name: os("b.txt"),
        },
        &ctx,
        &sink(),
    );
    let b = scratch.0.join("b.txt");
    assert_eq!(fs::read_to_string(&b)?, "keep");
    assert!(!a.exists());
    assert_eq!(renamed.select, std::slice::from_ref(&b));
    assert_eq!(
        renamed.undo,
        Some(UndoAction::Rename {
            old: a.clone(),
            new: b.clone()
        })
    );

    let clash = run_job(
        Job::Rename {
            path: b.clone(),
            new_name: os("c.txt"),
        },
        &ctx,
        &sink(),
    );
    assert_eq!(severity(&clash), Some(Severity::Error));
    assert_eq!(text(&clash), "\u{201c}c.txt\u{201d} already exists here");
    assert_eq!(fs::read_to_string(&c)?, "other", "nothing was overwritten");
    assert_eq!(fs::read_to_string(&b)?, "keep");
    assert_eq!(clash.undo, None);
    assert!(clash.select.is_empty());
    Ok(())
}

// --- jobs: trash and undo ----------------------------------------------------------

fn trash_paths(ctx: &JobCtx, paths: &[&PathBuf]) -> Report {
    run_job(
        Job::Trash(paths.iter().map(|path| (*path).clone()).collect()),
        ctx,
        &sink(),
    )
}

#[test]
fn deleting_moves_to_the_trash_with_info_and_offers_undo() -> TestResult {
    let scratch = Scratch::new("trash")?;
    let a = scratch.file("a.txt", "aaa")?;
    let b = scratch.file("b.txt", "bbb")?;
    let ctx = scratch.ctx();

    let one = trash_paths(&ctx, &[&a]);
    assert_eq!(severity(&one), Some(Severity::Success));
    assert_eq!(text(&one), "Moved \u{201c}a.txt\u{201d} to Trash");
    assert!(!a.exists());
    assert_eq!(
        fs::read_to_string(scratch.trash().join("files/a.txt"))?,
        "aaa"
    );
    let info = fs::read_to_string(scratch.trash().join("info/a.txt.trashinfo"))?;
    assert!(info.contains(&format!("Path={}", a.display())), "{info}");
    assert!(matches!(&one.undo, Some(UndoAction::Trash { items }) if items.len() == 1));

    let c = scratch.file("c.txt", "ccc")?;
    let two = trash_paths(&ctx, &[&b, &c]);
    assert_eq!(text(&two), "Moved 2 items to Trash");
    assert!(!b.exists() && !c.exists());
    Ok(())
}

#[test]
fn undoing_a_trash_restores_and_a_changed_world_is_reported_not_acted_on() -> TestResult {
    let scratch = Scratch::new("undo-trash")?;
    let a = scratch.file("a.txt", "original")?;
    let ctx = scratch.ctx();

    let trashed = trash_paths(&ctx, &[&a]);
    let action = trashed.undo.clone().ok_or("no undo recorded")?;
    let restored = run_job(Job::Undo(action.clone()), &ctx, &sink());
    assert_eq!(severity(&restored), Some(Severity::Success));
    assert_eq!(text(&restored), "Restored \u{201c}a.txt\u{201d}");
    assert_eq!(fs::read_to_string(&a)?, "original");
    assert_eq!(restored.select, std::slice::from_ref(&a));

    // Trash again, then put something else at the original path.
    let trashed = trash_paths(&ctx, &[&a]);
    let action = trashed.undo.ok_or("no undo recorded")?;
    scratch.file("a.txt", "newer")?;
    let stale = run_job(Job::Undo(action.clone()), &ctx, &sink());
    assert_eq!(severity(&stale), Some(Severity::Error));
    assert!(text(&stale).starts_with("Can't undo: "), "{}", text(&stale));
    assert!(text(&stale).contains("\u{201c}a.txt\u{201d} already exists"));
    assert_eq!(fs::read_to_string(&a)?, "newer", "never replaced");
    assert_eq!(
        fs::read_to_string(scratch.trash().join("files/a.txt"))?,
        "original",
        "the trashed copy stays in the trash"
    );

    // The trashed item was emptied by someone else.
    fs::remove_file(scratch.trash().join("files/a.txt"))?;
    let gone = run_job(Job::Undo(action), &ctx, &sink());
    assert_eq!(
        text(&gone),
        "Can't undo: \u{201c}a.txt\u{201d} is no longer in the Trash"
    );
    Ok(())
}

#[test]
fn without_a_trash_nothing_is_deleted_and_the_paths_are_handed_back() -> TestResult {
    let scratch = Scratch::new("no-trash")?;
    let a = scratch.file("a.txt", "keep me")?;
    let ctx = no_trash_ctx(scratch.trash());

    let report = trash_paths(&ctx, &[&a]);
    assert_eq!(report.needs_permanent, std::slice::from_ref(&a));
    assert_eq!(report.notice, None, "the confirmation dialog speaks next");
    assert_eq!(report.undo, None);
    assert_eq!(fs::read_to_string(&a)?, "keep me", "never a silent delete");
    Ok(())
}

#[test]
fn a_trash_failure_is_reported_with_details_and_changes_nothing_else() -> TestResult {
    let scratch = Scratch::new("trash-fail")?;
    let a = scratch.file("a.txt", "x")?;
    let gone = scratch.0.join("gone.txt");
    let report = trash_paths(&scratch.ctx(), &[&a, &gone]);
    assert_eq!(severity(&report), Some(Severity::Warning));
    assert!(text(&report).starts_with("Moved \u{201c}a.txt\u{201d} to Trash"));
    assert_eq!(report.details.len(), 1);
    assert!(
        report
            .details
            .first()
            .is_some_and(|line| line.contains("gone.txt")),
        "{:?}",
        report.details
    );
    assert!(matches!(&report.undo, Some(UndoAction::Trash { items }) if items.len() == 1));
    Ok(())
}

#[test]
fn cancelling_before_an_item_leaves_it_alone() -> TestResult {
    let scratch = Scratch::new("cancel")?;
    let a = scratch.file("a.txt", "x")?;
    let cancel = sink();
    cancel.request_cancel();
    let report = run_job(Job::Trash(vec![a.clone()]), &scratch.ctx(), &cancel);
    assert!(report.cancelled);
    assert!(a.exists(), "a cancelled operation touches nothing more");
    assert_eq!(severity(&report), Some(Severity::Info));
    assert_eq!(text(&report), "Cancelled: nothing was moved to the Trash");
    Ok(())
}

#[test]
fn undoing_a_creation_trashes_the_empty_item_and_refuses_changed_ones() -> TestResult {
    let scratch = Scratch::new("undo-create")?;
    let dir = scratch.dir("work")?;
    let ctx = scratch.ctx();
    let make = |kind| {
        run_job(
            Job::Create {
                dir: dir.clone(),
                kind,
            },
            &ctx,
            &sink(),
        )
    };

    let created = make(NewKind::Folder);
    let action = created.undo.ok_or("no undo recorded")?;
    let undone = run_job(Job::Undo(action), &ctx, &sink());
    assert_eq!(
        text(&undone),
        "Moved \u{201c}Untitled folder\u{201d} to Trash"
    );
    assert!(!dir.join("Untitled folder").exists());
    assert!(
        scratch.trash().join("files/Untitled folder").is_dir(),
        "moved to the Trash, not deleted for good"
    );

    let created = make(NewKind::Folder);
    let action = created.undo.ok_or("no undo recorded")?;
    scratch.file("work/Untitled folder/data.txt", "mine")?;
    let refused = run_job(Job::Undo(action), &ctx, &sink());
    assert_eq!(
        text(&refused),
        "Can't undo: \u{201c}Untitled folder\u{201d} is no longer empty"
    );
    assert_eq!(
        fs::read_to_string(dir.join("Untitled folder/data.txt"))?,
        "mine"
    );

    let created = make(NewKind::File);
    let action = created.undo.ok_or("no undo recorded")?;
    fs::remove_file(dir.join("Untitled file"))?;
    let missing = run_job(Job::Undo(action), &ctx, &sink());
    assert_eq!(
        text(&missing),
        "Can't undo: \u{201c}Untitled file\u{201d} no longer exists"
    );
    Ok(())
}

#[test]
fn undoing_a_rename_renames_back_unless_either_name_changed() -> TestResult {
    let scratch = Scratch::new("undo-rename")?;
    let a = scratch.file("a.txt", "A")?;
    let ctx = scratch.ctx();
    let b = scratch.0.join("b.txt");
    let action = UndoAction::Rename {
        old: a.clone(),
        new: b.clone(),
    };

    let missing = run_job(Job::Undo(action.clone()), &ctx, &sink());
    assert_eq!(
        text(&missing),
        "Can't undo: \u{201c}b.txt\u{201d} no longer exists"
    );

    fs::rename(&a, &b)?;
    scratch.file("a.txt", "someone else's")?;
    let taken = run_job(Job::Undo(action.clone()), &ctx, &sink());
    assert_eq!(
        text(&taken),
        "Can't undo: \u{201c}a.txt\u{201d} already exists"
    );
    assert_eq!(fs::read_to_string(&a)?, "someone else's");
    assert_eq!(fs::read_to_string(&b)?, "A");

    fs::remove_file(&a)?;
    let back = run_job(Job::Undo(action), &ctx, &sink());
    assert_eq!(severity(&back), Some(Severity::Success));
    assert_eq!(fs::read_to_string(&a)?, "A");
    assert!(!b.exists());
    assert_eq!(back.select, [a]);
    Ok(())
}

// --- jobs: permanent delete -------------------------------------------------------

#[test]
fn permanent_delete_removes_trees_and_never_follows_symlinks() -> TestResult {
    let scratch = Scratch::new("delete")?;
    let outside = scratch.file("outside/precious.txt", "precious")?;
    let tree = scratch.file("tree/sub/file.txt", "x")?;
    let tree_dir = scratch.0.join("tree");
    unix_fs::symlink(scratch.0.join("outside"), tree_dir.join("link-to-outside"))?;
    let lone = scratch.file("lone.txt", "y")?;
    let ctx = scratch.ctx();

    let one = run_job(
        Job::DeletePermanently(vec![tree_dir.clone()]),
        &ctx,
        &sink(),
    );
    assert_eq!(text(&one), "Deleted \u{201c}tree\u{201d}");
    assert!(!tree.exists() && !tree_dir.exists());
    assert_eq!(
        fs::read_to_string(&outside)?,
        "precious",
        "the link was not followed"
    );

    let extra = scratch.file("extra.txt", "z")?;
    let two = run_job(
        Job::DeletePermanently(vec![lone.clone(), extra.clone()]),
        &ctx,
        &sink(),
    );
    assert_eq!(text(&two), "Deleted 2 items");
    assert!(!lone.exists() && !extra.exists());
    assert_eq!(two.undo, None, "permanent deletion cannot be undone");

    let missing = run_job(Job::DeletePermanently(vec![lone]), &ctx, &sink());
    assert_eq!(severity(&missing), Some(Severity::Error));
    Ok(())
}

// --- jobs: Trash view ---------------------------------------------------------------

#[test]
fn restoring_keeps_both_on_conflict_and_names_the_new_name() -> TestResult {
    let scratch = Scratch::new("restore")?;
    let a = scratch.file("a.txt", "trashed")?;
    let ctx = scratch.ctx();
    trash_paths(&ctx, &[&a]);

    let plain = run_job(Job::Restore(vec![os("a.txt")]), &ctx, &sink());
    assert_eq!(text(&plain), "Restored \u{201c}a.txt\u{201d}");
    assert_eq!(fs::read_to_string(&a)?, "trashed");
    assert_eq!(plain.select, std::slice::from_ref(&a));

    trash_paths(&ctx, &[&a]);
    scratch.file("a.txt", "newer")?;
    let both = run_job(Job::Restore(vec![os("a.txt")]), &ctx, &sink());
    assert_eq!(
        text(&both),
        "Restored \u{201c}a.txt\u{201d} as \u{201c}a (2).txt\u{201d}"
    );
    assert_eq!(fs::read_to_string(&a)?, "newer", "never replaced");
    assert_eq!(fs::read_to_string(scratch.0.join("a (2).txt"))?, "trashed");
    Ok(())
}

#[test]
fn deleting_from_the_trash_and_emptying_remove_items_for_good() -> TestResult {
    let scratch = Scratch::new("empty")?;
    let a = scratch.file("a.txt", "a")?;
    let b = scratch.file("b.txt", "b")?;
    let c = scratch.file("c.txt", "c")?;
    let ctx = scratch.ctx();
    trash_paths(&ctx, &[&a, &b, &c]);

    let one = run_job(Job::DeleteFromTrash(vec![os("a.txt")]), &ctx, &sink());
    assert_eq!(text(&one), "Deleted \u{201c}a.txt\u{201d} from the Trash");
    assert!(!scratch.trash().join("files/a.txt").exists());
    assert!(!scratch.trash().join("info/a.txt.trashinfo").exists());
    assert!(scratch.trash().join("files/b.txt").exists());

    let cancel = sink();
    cancel.request_cancel();
    let stopped = run_job(
        Job::EmptyTrash(vec![os("b.txt"), os("c.txt")]),
        &ctx,
        &cancel,
    );
    assert!(stopped.cancelled);
    assert!(
        scratch.trash().join("files/b.txt").exists(),
        "cancel keeps the rest"
    );

    let emptied = run_job(
        Job::EmptyTrash(vec![os("b.txt"), os("c.txt")]),
        &ctx,
        &sink(),
    );
    assert_eq!(text(&emptied), "Emptied the Trash");
    assert!(!scratch.trash().join("files/b.txt").exists());
    assert!(!scratch.trash().join("files/c.txt").exists());
    assert!(!scratch.trash().join("info/c.txt.trashinfo").exists());

    let again = run_job(Job::EmptyTrash(Vec::new()), &ctx, &sink());
    assert_eq!(severity(&again), Some(Severity::Info));
    assert_eq!(text(&again), "The Trash is already empty");
    Ok(())
}

#[test]
fn empty_trash_touches_only_the_confirmed_items() -> TestResult {
    let scratch = Scratch::new("empty-confirmed")?;
    let a = scratch.file("a.txt", "a")?;
    let late = scratch.file("late.txt", "late")?;
    let ctx = scratch.ctx();
    trash_paths(&ctx, &[&a]);
    // Trashed after the confirmation counted the Trash.
    trash_paths(&ctx, &[&late]);
    // A concurrent put that has written its info but not moved its file yet.
    let pending_info = scratch.trash().join("info/pending.txt.trashinfo");
    std::fs::write(
        &pending_info,
        "[Trash Info]\nPath=/somewhere/pending.txt\nDeletionDate=2026-11-18T10:00:00\n",
    )?;

    let emptied = run_job(Job::EmptyTrash(vec![os("a.txt")]), &ctx, &sink());
    assert_eq!(text(&emptied), "Emptied the Trash");
    assert!(!scratch.trash().join("files/a.txt").exists());
    assert!(
        scratch.trash().join("files/late.txt").exists(),
        "unconfirmed item kept"
    );
    assert!(scratch.trash().join("info/late.txt.trashinfo").exists());
    assert!(pending_info.exists(), "in-flight put metadata kept");
    Ok(())
}
