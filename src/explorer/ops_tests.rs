use std::fs;
use std::io;
use std::os::unix::fs as unix_fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use fileflux::core::apps::{App, OpenWith};
use fileflux::core::places::startup_trash_root;
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
        identity: Identity { dev: 1, ino: 2 },
    }
}

#[test]
fn the_undo_stack_pops_the_newest_and_take_removes_only_that_entry() {
    let mut stack = UndoStack::default();
    let a = stack.push(rename_action("/x/a", "/x/b"));
    let b = stack.push(UndoAction::Create {
        path: PathBuf::from("/x/new"),
        kind: NewKind::Folder,
        identity: Identity { dev: 1, ino: 3 },
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
fn looking_at_an_entry_leaves_it_on_the_stack_so_a_refused_undo_loses_nothing() {
    let mut stack = UndoStack::default();
    let a = stack.push(rename_action("/x/a", "/x/b"));
    let b = stack.push(rename_action("/x/c", "/x/d"));
    assert_eq!(stack.newest().map(|entry| entry.id), Some(b));
    assert_eq!(stack.get(a).map(|entry| entry.id), Some(a));
    assert_eq!(stack.get(99), None);
    assert_eq!(stack.len(), 2, "nothing was removed");
    assert_eq!(stack.pop().map(|entry| entry.id), Some(b));
}

#[test]
fn an_entry_remembers_its_toast_so_ctrl_z_can_close_the_stale_undo_button() {
    let mut stack = UndoStack::default();
    let a = stack.push(rename_action("/x/a", "/x/b"));
    let b = stack.push(rename_action("/x/c", "/x/d"));
    stack.attach_toast(a, 41);
    stack.attach_toast(b, 42);
    stack.attach_toast(99, 7); // an unknown entry changes nothing

    // Ctrl+Z pops the newest entry and learns which toast offered its Undo.
    assert_eq!(stack.pop().and_then(|entry| entry.toast), Some(42));
    assert_eq!(stack.take(a).and_then(|entry| entry.toast), Some(41));

    let c = stack.push(rename_action("/x/e", "/x/f"));
    assert_eq!(
        stack.take(c).and_then(|entry| entry.toast),
        None,
        "no toast was shown (or it was throttled): nothing to close"
    );
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
fn names_the_single_line_editor_would_change_are_not_edited_inline() {
    for name in [
        "new\nline",
        "tab\there",
        "cr\rhere",
        "bell\u{7}",
        "nul\u{85}x",
    ] {
        assert_eq!(
            rename_seed(OsStr::new(name)),
            Err("This name has characters that can't be edited here"),
            "{name:?}"
        );
    }
    assert_eq!(
        rename_seed(OsStr::new("report v1.txt")),
        Ok("report v1.txt".to_owned())
    );
    assert_eq!(
        rename_seed(OsStr::new(" padded ")),
        Ok(" padded ".to_owned())
    );
    assert_eq!(
        rename_seed(OsStr::new("ñandú \u{1F600}")),
        Ok("ñandú \u{1F600}".to_owned())
    );
}

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
    assert_eq!(
        check("  a.txt "),
        RenameCheck::Rename(os("  a.txt ")),
        "the name is kept exactly as typed, never trimmed behind the user's back"
    );
    assert_eq!(
        check(""),
        RenameCheck::Invalid("The name can't be empty.".to_owned())
    );
    assert_eq!(
        check("   "),
        RenameCheck::Invalid("The name can't be blank.".to_owned())
    );
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
    assert_eq!(check(" c.txt "), RenameCheck::Rename(os(" c.txt ")));
    // A sibling that differs only by the padding is a different name.
    let padded = [os(" b.txt")];
    assert_eq!(
        check_rename(
            OsStr::new("a.txt"),
            "b.txt",
            padded.iter().map(|n| n.as_os_str())
        ),
        RenameCheck::Rename(os("b.txt"))
    );
}

#[test]
fn unchanged_is_judged_on_the_exact_name_even_with_padding() {
    let no_siblings = std::iter::empty::<&OsStr>();
    assert_eq!(
        check_rename(OsStr::new(" a.txt "), " a.txt ", no_siblings),
        RenameCheck::Unchanged
    );
    let no_siblings = std::iter::empty::<&OsStr>();
    assert_eq!(
        check_rename(OsStr::new(" a.txt "), "a.txt", no_siblings),
        RenameCheck::Rename(os("a.txt")),
        "removing the padding is a real rename"
    );
}

#[test]
fn a_padded_name_is_renamed_as_typed_and_the_user_is_told() -> TestResult {
    let scratch = Scratch::new("rename-padded")?;
    let a = scratch.file("a.txt", "A")?;
    let report = run_job(
        Job::Rename {
            path: a.clone(),
            new_name: os(" b.txt "),
        },
        &scratch.ctx(),
        &sink(),
    );
    let padded = scratch.0.join(" b.txt ");
    assert_eq!(fs::read_to_string(&padded)?, "A", "spaces are kept on disk");
    assert!(!a.exists());
    assert!(!scratch.0.join("b.txt").exists(), "nothing was trimmed");
    assert_eq!(severity(&report), Some(Severity::Info));
    assert_eq!(
        text(&report),
        "Renamed to \u{201c} b.txt \u{201d}; its leading or trailing spaces are kept"
    );
    assert_eq!(report.select, [padded]);
    Ok(())
}

// --- context menu model ---------------------------------------------------------

fn flat(rows: &[MenuRow]) -> Vec<String> {
    rows.iter()
        .map(|row| match row {
            MenuRow::Separator => "-".to_owned(),
            MenuRow::Header(label) => format!("# {label}"),
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
        can_paste: false,
        folder_selected: false,
        single_file: false,
        folder_favorite: false,
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
            "Cut",
            "Copy",
            "Paste",
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
            "Paste",
            "-",
            "Sort by",
            "Filter",
            "-",
            "Open in terminal",
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
        [
            "New folder",
            "New file",
            "-",
            "Paste",
            "-",
            "Sort by",
            "Filter",
            "-",
            "Copy folder path"
        ],
        "no dead 'Open in terminal' item"
    );
}

#[test]
fn the_menu_header_names_what_the_menu_acts_on() {
    let names = |list: &[&str]| {
        list.iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(menu_title(&names(&["report.pdf"]), "Home"), "report.pdf");
    assert_eq!(menu_title(&names(&["a", "b", "c"]), "Home"), "3 items");
    assert_eq!(menu_title(&names(&["a", "b"]), "Home"), "2 items");
    assert_eq!(menu_title(&[], "Home"), "Home (this folder)");
    assert_eq!(menu_title(&[], "Home Trash"), "Home Trash (this folder)");
}

#[test]
fn a_sidebar_row_menu_offers_open_tab_favorite_copy_and_properties() {
    let rows = sidebar_menu_rows(&SidebarTarget {
        trash: false,
        favorite: false,
        terminal: false,
    });
    assert_eq!(
        flat(&rows),
        [
            "Open",
            "Open in new tab",
            "-",
            "Add to favorites",
            "Copy path",
            "-",
            "Properties"
        ]
    );
    assert!(item(&rows, MenuCommand::AddFavorite).is_some());
    assert!(item(&rows, MenuCommand::RemoveFavorite).is_none());
    assert!(
        rows.iter()
            .all(|row| !matches!(row, MenuRow::Item(item) if !item.enabled || item.danger)),
        "every sidebar row works and none is destructive"
    );
}

#[test]
fn a_favorite_row_offers_remove_instead_of_add() {
    let rows = sidebar_menu_rows(&SidebarTarget {
        trash: false,
        favorite: true,
        terminal: false,
    });
    assert_eq!(
        flat(&rows),
        [
            "Open",
            "Open in new tab",
            "-",
            "Remove from favorites",
            "Copy path",
            "-",
            "Properties"
        ]
    );
    assert!(item(&rows, MenuCommand::RemoveFavorite).is_some());
    assert!(item(&rows, MenuCommand::AddFavorite).is_none());
}

#[test]
fn a_sidebar_row_offers_open_in_terminal_only_when_a_terminal_exists() {
    let rows = sidebar_menu_rows(&SidebarTarget {
        trash: false,
        favorite: false,
        terminal: true,
    });
    assert_eq!(
        flat(&rows),
        [
            "Open",
            "Open in new tab",
            "Open in terminal",
            "-",
            "Add to favorites",
            "Copy path",
            "-",
            "Properties"
        ]
    );
    assert!(item(&rows, MenuCommand::OpenTerminal).is_some());
}

#[test]
fn a_single_selected_folder_offers_favorites_and_a_terminal_in_its_menu() {
    let folder = MenuTarget {
        folder_selected: true,
        ..target(false, 1, false, true)
    };
    let rows = menu_rows(&folder);
    assert_eq!(
        flat(&rows),
        [
            "Open",
            "Open in new tab",
            "Add to favorites",
            "Open in terminal",
            "-",
            "Cut",
            "Copy",
            "Paste into folder",
            "-",
            "Rename",
            "Copy path",
            "-",
            "Properties",
            "Move to Trash",
            "Delete permanently…"
        ]
    );
    assert!(item(&rows, MenuCommand::AddFavorite).is_some_and(|row| row.enabled));
    let favorite = MenuTarget {
        folder_favorite: true,
        ..folder
    };
    let rows = menu_rows(&favorite);
    assert_eq!(
        flat(&rows).get(2).map(String::as_str),
        Some("Remove from favorites")
    );
    assert!(item(&rows, MenuCommand::AddFavorite).is_none());

    // No terminal on the machine: the row is left out, favorites stay.
    let without = menu_rows(&MenuTarget {
        terminal: false,
        ..folder
    });
    assert!(item(&without, MenuCommand::OpenTerminal).is_none());
    assert!(item(&without, MenuCommand::AddFavorite).is_some());
}

#[test]
fn favorites_and_terminal_never_show_for_files_selections_or_the_trash() {
    let offers = |target: MenuTarget| {
        let rows = menu_rows(&target);
        (
            item(&rows, MenuCommand::AddFavorite).is_some()
                || item(&rows, MenuCommand::RemoveFavorite).is_some(),
            item(&rows, MenuCommand::OpenTerminal).is_some(),
        )
    };
    let folder = MenuTarget {
        folder_selected: true,
        ..target(false, 1, false, true)
    };
    assert_eq!(offers(folder), (true, true));
    assert_eq!(
        offers(target(false, 1, false, true)),
        (false, false),
        "a file or link"
    );
    assert_eq!(
        offers(MenuTarget {
            single_file: true,
            ..target(false, 1, false, true)
        }),
        (false, false)
    );
    assert_eq!(
        offers(target(false, 3, false, true)),
        (false, false),
        "several"
    );
    assert_eq!(
        offers(MenuTarget {
            in_trash: true,
            ..folder
        }),
        (false, false),
        "the Trash view"
    );
    assert_eq!(
        offers(target(false, 0, false, true)),
        (false, true),
        "empty space"
    );
}

#[test]
fn the_sidebar_trash_row_offers_open_new_tab_and_empty_trash_only() {
    // A favorite flag is meaningless for the Trash: it is never favorited.
    for favorite in [false, true] {
        let rows = sidebar_menu_rows(&SidebarTarget {
            trash: true,
            favorite,
            terminal: true,
        });
        assert_eq!(
            flat(&rows),
            ["Open", "Open in new tab", "-", "Empty Trash\u{2026}"]
        );
        assert!(item(&rows, MenuCommand::EmptyTrash).is_some_and(|item| item.danger));
        assert!(item(&rows, MenuCommand::AddFavorite).is_none());
        assert!(item(&rows, MenuCommand::CopyPath).is_none());
    }
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
    // 0 Open, 1 -, 2 Cut, 3 Copy, 4 Paste (disabled: nothing to paste),
    // 5 -, 6 Rename, 7 Copy path, 8 -, 9 Properties, 10 Trash, 11 Delete…
    assert_eq!(next_menu_index(&rows, None, true), Some(0));
    assert_eq!(
        next_menu_index(&rows, Some(0), true),
        Some(2),
        "skips the separator"
    );
    assert_eq!(
        next_menu_index(&rows, Some(3), true),
        Some(6),
        "skips the disabled Paste and the separator"
    );
    assert_eq!(
        next_menu_index(&rows, Some(11), true),
        Some(0),
        "wraps down"
    );
    assert_eq!(next_menu_index(&rows, None, false), Some(11));
    assert_eq!(next_menu_index(&rows, Some(6), false), Some(3));
    assert_eq!(next_menu_index(&rows, Some(0), false), Some(11), "wraps up");
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
            path: made.clone(),
            kind: NewKind::Folder,
            identity: Identity::of(&made)?,
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
            new: b.clone(),
            identity: Identity::of(&b)?,
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
fn the_startup_trash_root_serves_a_delete_before_places_are_discovered() -> TestResult {
    let scratch = Scratch::new("startup-trash")?;
    let home = scratch.dir("home")?;
    let a = scratch.file("home/a.txt", "x")?;
    let root = startup_trash_root(Some(home.as_os_str()), None).ok_or("no startup trash root")?;
    assert_eq!(root, home.join(".local/share/Trash"));

    // What the explorer builds before `discover()` has answered.
    let ctx = JobCtx::from_env(root.clone(), Vec::new());
    let report = run_job(Job::Trash(vec![a.clone()]), &ctx, &sink());

    assert!(
        report.needs_permanent.is_empty(),
        "no permanent-delete offer"
    );
    assert!(!a.exists());
    assert_eq!(fs::read(root.join("files/a.txt"))?, b"x");
    Ok(())
}

fn app(id: &str, name: &str, exec: &str) -> App {
    App {
        id: id.to_owned(),
        name: name.to_owned(),
        exec: exec.to_owned(),
    }
}

#[test]
fn open_with_is_offered_for_one_selected_file_only() {
    let offers = |target: MenuTarget| {
        menu_rows(&target)
            .iter()
            .any(|row| matches!(row, MenuRow::Item(spec) if spec.command == MenuCommand::OpenWith))
    };
    let one_file = MenuTarget {
        single_file: true,
        ..target(false, 1, false, false)
    };
    assert!(offers(one_file));
    assert!(
        !offers(target(false, 1, false, false)),
        "a folder or link target"
    );
    assert!(!offers(target(false, 2, false, false)), "several entries");
    assert!(!offers(target(false, 0, false, false)), "empty space");
    assert!(
        !offers(MenuTarget {
            in_trash: true,
            ..one_file
        }),
        "the Trash view"
    );
    let label = menu_rows(&one_file).into_iter().find_map(|row| match row {
        MenuRow::Item(spec) if spec.command == MenuCommand::OpenWith => Some(spec.label),
        _ => None,
    });
    assert_eq!(label, Some("Open with\u{2026}"));
}

#[test]
fn open_in_new_tab_is_offered_for_one_selected_folder_only() {
    let offers = |target: MenuTarget| {
        menu_rows(&target).iter().any(
            |row| matches!(row, MenuRow::Item(spec) if spec.command == MenuCommand::OpenInNewTab),
        )
    };
    let one_folder = MenuTarget {
        folder_selected: true,
        ..target(false, 1, false, false)
    };
    assert!(offers(one_folder));
    let rows = menu_rows(&one_folder);
    let spec = item(&rows, MenuCommand::OpenInNewTab);
    assert_eq!(spec.map(|spec| spec.label), Some("Open in new tab"));
    assert_eq!(spec.and_then(|spec| spec.shortcut), None);
    assert_eq!(
        flat(&rows).first().map(String::as_str),
        Some("Open"),
        "Open stays first"
    );
    assert!(
        !offers(target(false, 1, false, false)),
        "a file or link target"
    );
    assert!(!offers(target(false, 0, false, false)), "empty space");
    assert!(
        !offers(MenuTarget {
            in_trash: true,
            ..one_folder
        }),
        "the Trash view"
    );
}

#[test]
fn the_open_with_dialog_lists_the_apps_and_defaults_to_opening() {
    let found = OpenWith {
        mime: "text/plain".to_owned(),
        apps: vec![
            app("a.desktop", "Alpha", "alpha %f"),
            app("b.desktop", "Beta", "beta %f"),
        ],
    };
    let dialog = DialogState::open_with(Path::new("/d/notes.txt"), &found);
    assert_eq!(
        dialog.text.title,
        "Open \u{201c}notes.txt\u{201d} with\u{2026}"
    );
    assert_eq!(dialog.text.names, ["Alpha", "Beta"]);
    assert_eq!(dialog.text.confirm, "Open");
    assert_eq!(
        dialog.initial_focus(),
        DialogChoice::Confirm,
        "Enter launches"
    );
    assert_eq!(
        dialog.action,
        DialogAction::OpenWith {
            path: PathBuf::from("/d/notes.txt"),
            apps: found.apps.clone(),
            selected: 0,
        }
    );
    // A delete confirmation still defaults to the safe button.
    let delete = DialogState::new(
        DialogKind::DeletePermanently,
        &["a".to_owned()],
        DialogAction::DeletePermanently(vec![PathBuf::from("/d/a")]),
    );
    assert_eq!(delete.initial_focus(), DialogChoice::Cancel);
}

#[test]
fn the_open_with_selection_moves_within_the_list() {
    assert_eq!(move_selection(0, 3, 1), 1);
    assert_eq!(move_selection(2, 3, 1), 2, "stops at the last");
    assert_eq!(move_selection(0, 3, -1), 0, "stops at the first");
    assert_eq!(move_selection(1, 3, -1), 0);
    assert_eq!(move_selection(0, 0, 1), 0, "an empty list stays put");
}

#[test]
fn launching_an_app_runs_its_exec_in_the_files_folder() {
    let chosen = app("v.desktop", "Viewer", "viewer --open %f");
    let command = open_with_command(Path::new("/d/My Docs/a b.txt"), &chosen);
    assert_eq!(
        command,
        Ok(OpenWithCommand {
            program: PathBuf::from("viewer"),
            args: vec![
                OsString::from("--open"),
                OsString::from("/d/My Docs/a b.txt")
            ],
            cwd: PathBuf::from("/d/My Docs"),
        })
    );
    let broken = app("x.desktop", "Broken", "\"never closed %f");
    assert_eq!(
        open_with_command(Path::new("/d/a.txt"), &broken),
        Err("Could not open with Broken: the Exec line has an unclosed quote".to_owned())
    );
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
        identity: Identity::of(&a)?,
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

// --- Trash view gaps ----------------------------------------------------------------

/// Puts a directory's permissions back when dropped, so the scratch
/// directory can be removed after a test made something undeletable.
struct Unlock(PathBuf);

impl Drop for Unlock {
    fn drop(&mut self) {
        fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700)).ok();
    }
}

#[test]
fn deleting_from_the_trash_works_without_readable_metadata() -> TestResult {
    let scratch = Scratch::new("delete-no-info")?;
    let a = scratch.file("a.txt", "a")?;
    let b = scratch.file("b.txt", "b")?;
    let c = scratch.file("c.txt", "c")?;
    let ctx = scratch.ctx();
    trash_paths(&ctx, &[&a, &b, &c]);
    let info = |name: &str| scratch.trash().join(format!("info/{name}.trashinfo"));
    fs::remove_file(info("a.txt"))?; // missing
    fs::write(info("b.txt"), "not a trashinfo at all")?; // malformed
    fs::set_permissions(info("c.txt"), fs::Permissions::from_mode(0o000))?; // unreadable

    let report = run_job(
        Job::DeleteFromTrash(vec![os("a.txt"), os("b.txt"), os("c.txt")]),
        &ctx,
        &sink(),
    );
    assert_eq!(severity(&report), Some(Severity::Success));
    assert_eq!(text(&report), "Deleted 3 items from the Trash");
    assert!(report.details.is_empty(), "{:?}", report.details);
    for name in ["a.txt", "b.txt", "c.txt"] {
        assert!(!scratch.trash().join("files").join(name).exists(), "{name}");
        assert!(!info(name).exists(), "{name} info");
    }
    Ok(())
}

#[test]
fn restoring_without_usable_metadata_fails_and_keeps_the_item() -> TestResult {
    let scratch = Scratch::new("restore-no-info")?;
    let a = scratch.file("a.txt", "precious")?;
    let b = scratch.file("b.txt", "also")?;
    let ctx = scratch.ctx();
    trash_paths(&ctx, &[&a, &b]);
    fs::remove_file(scratch.trash().join("info/a.txt.trashinfo"))?;
    fs::write(scratch.trash().join("info/b.txt.trashinfo"), "garbage")?;

    let report = run_job(Job::Restore(vec![os("a.txt"), os("b.txt")]), &ctx, &sink());
    assert_eq!(severity(&report), Some(Severity::Error));
    assert_eq!(text(&report), "Could not restore 2 items");
    assert_eq!(report.details.len(), 2);
    assert!(report.select.is_empty());
    assert_eq!(
        fs::read_to_string(scratch.trash().join("files/a.txt"))?,
        "precious"
    );
    assert_eq!(
        fs::read_to_string(scratch.trash().join("files/b.txt"))?,
        "also"
    );
    assert!(!a.exists() && !b.exists());
    Ok(())
}

#[test]
fn restoring_several_items_counts_the_keep_both_renames() -> TestResult {
    let scratch = Scratch::new("restore-many")?;
    let a = scratch.file("a.txt", "trashed a")?;
    let b = scratch.file("b.txt", "trashed b")?;
    let c = scratch.file("c.txt", "trashed c")?;
    let ctx = scratch.ctx();
    trash_paths(&ctx, &[&a, &b, &c]);
    scratch.file("a.txt", "new a")?;
    scratch.file("b.txt", "new b")?;

    let report = run_job(
        Job::Restore(vec![os("a.txt"), os("b.txt"), os("c.txt")]),
        &ctx,
        &sink(),
    );
    assert_eq!(severity(&report), Some(Severity::Success));
    assert_eq!(text(&report), "Restored 3 items; 2 renamed to keep both");
    assert_eq!(fs::read_to_string(&a)?, "new a", "never replaced");
    assert_eq!(fs::read_to_string(&b)?, "new b", "never replaced");
    assert_eq!(
        fs::read_to_string(scratch.0.join("a (2).txt"))?,
        "trashed a"
    );
    assert_eq!(
        fs::read_to_string(scratch.0.join("b (2).txt"))?,
        "trashed b"
    );
    assert_eq!(fs::read_to_string(&c)?, "trashed c");
    let mut selected = report.select.clone();
    selected.sort();
    assert_eq!(
        selected,
        [
            scratch.0.join("a (2).txt"),
            scratch.0.join("b (2).txt"),
            c.clone()
        ]
    );
    Ok(())
}

#[test]
fn emptying_the_trash_reports_the_item_that_could_not_be_removed() -> TestResult {
    let scratch = Scratch::new("empty-fail")?;
    let a = scratch.file("a.txt", "a")?;
    scratch.file("stuck/inner.txt", "inner")?;
    let stuck = scratch.0.join("stuck");
    let ctx = scratch.ctx();
    trash_paths(&ctx, &[&a, &stuck]);
    // A directory without write permission cannot lose its children.
    let locked = scratch.trash().join("files/stuck");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500))?;
    let _unlock = Unlock(locked.clone());

    let report = run_job(
        Job::EmptyTrash(vec![os("a.txt"), os("stuck")]),
        &ctx,
        &sink(),
    );
    assert_eq!(severity(&report), Some(Severity::Warning));
    assert_eq!(text(&report), "Emptied the Trash; 1 could not be removed");
    assert_eq!(report.details.len(), 1);
    assert!(
        report
            .details
            .first()
            .is_some_and(|line| line.contains("stuck")),
        "{:?}",
        report.details
    );
    assert!(!scratch.trash().join("files/a.txt").exists());
    assert!(!scratch.trash().join("info/a.txt.trashinfo").exists());
    assert!(locked.exists(), "the failed item stays");
    assert!(
        scratch.trash().join("info/stuck.trashinfo").exists(),
        "and stays listed with its info"
    );
    Ok(())
}

// --- undo identity (R1) -------------------------------------------------------------

/// Names in `dir`, sorted: the whole observable state of a scratch folder.
fn listing(dir: &Path) -> io::Result<Vec<String>> {
    let mut names = fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<io::Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

#[test]
fn an_identity_tells_a_replaced_file_from_the_same_one() -> TestResult {
    let scratch = Scratch::new("identity")?;
    let a = scratch.file("a.txt", "A")?;
    let before = Identity::of(&a)?;
    let moved = scratch.0.join("moved.txt");
    fs::rename(&a, &moved)?;
    assert_eq!(Identity::of(&moved)?, before, "a rename keeps the identity");
    // The original stays alive, so the new file cannot reuse its inode.
    fs::write(&a, "other")?;
    assert_ne!(Identity::of(&a)?, before);
    assert!(Identity::of(&scratch.0.join("missing")).is_err());
    Ok(())
}

#[test]
fn undoing_a_rename_refuses_an_item_that_was_replaced_since() -> TestResult {
    let scratch = Scratch::new("undo-rename-identity")?;
    let a = scratch.file("a.txt", "A")?;
    let ctx = scratch.ctx();
    let renamed = run_job(
        Job::Rename {
            path: a.clone(),
            new_name: os("b.txt"),
        },
        &ctx,
        &sink(),
    );
    let action = renamed.undo.ok_or("no undo recorded")?;
    let b = scratch.0.join("b.txt");
    // Someone moves our file away and puts another one under its name.
    let kept = scratch.0.join("kept.txt");
    fs::rename(&b, &kept)?;
    fs::write(&b, "someone else's")?;

    let refused = run_job(Job::Undo(action), &ctx, &sink());
    assert_eq!(severity(&refused), Some(Severity::Error));
    assert_eq!(
        text(&refused),
        "Can't undo: \u{201c}b.txt\u{201d} changed since it was renamed"
    );
    assert_eq!(refused.undo, None);
    assert!(refused.select.is_empty());
    assert_eq!(fs::read_to_string(&b)?, "someone else's");
    assert_eq!(fs::read_to_string(&kept)?, "A");
    assert!(!a.exists(), "nothing was renamed back");
    assert_eq!(listing(&scratch.0)?, ["b.txt", "kept.txt"]);
    Ok(())
}

#[test]
fn undoing_a_trash_refuses_a_swapped_item_and_changed_metadata() -> TestResult {
    let scratch = Scratch::new("undo-trash-identity")?;
    let a = scratch.file("a.txt", "original a")?;
    let b = scratch.file("b.txt", "original b")?;
    let ctx = scratch.ctx();
    let a_action = trash_paths(&ctx, &[&a]).undo.ok_or("no undo recorded")?;
    let b_action = trash_paths(&ctx, &[&b]).undo.ok_or("no undo recorded")?;

    // `a`: the trashed file was swapped for another one.
    let in_trash = scratch.trash().join("files/a.txt");
    let kept = scratch.0.join("kept.txt");
    fs::rename(&in_trash, &kept)?;
    fs::write(&in_trash, "impostor")?;
    let swapped = run_job(Job::Undo(a_action), &ctx, &sink());
    assert_eq!(
        text(&swapped),
        "Can't undo: \u{201c}a.txt\u{201d} changed since it was trashed"
    );
    assert_eq!(swapped.undo, None);
    assert_eq!(fs::read_to_string(&in_trash)?, "impostor", "left alone");
    assert!(!a.exists(), "nothing was restored");

    // `b`: its .trashinfo now says it came from somewhere else.
    let elsewhere = scratch.0.join("elsewhere");
    let info = scratch.trash().join("info/b.trashinfo");
    fs::write(
        scratch.trash().join("info/b.txt.trashinfo"),
        format!(
            "[Trash Info]\nPath={}/b.txt\nDeletionDate=2026-11-18T10:00:00\n",
            elsewhere.display()
        ),
    )?;
    assert!(!info.exists());
    let rewritten = run_job(Job::Undo(b_action), &ctx, &sink());
    assert_eq!(
        text(&rewritten),
        "Can't undo: \u{201c}b.txt\u{201d} changed since it was trashed"
    );
    assert_eq!(rewritten.undo, None);
    assert_eq!(
        fs::read_to_string(scratch.trash().join("files/b.txt"))?,
        "original b"
    );
    assert!(!b.exists() && !elsewhere.exists(), "nothing was restored");
    Ok(())
}

#[test]
fn undoing_a_creation_refuses_an_item_that_was_replaced_since() -> TestResult {
    let scratch = Scratch::new("undo-create-identity")?;
    let dir = scratch.dir("work")?;
    let ctx = scratch.ctx();
    let created = run_job(
        Job::Create {
            dir: dir.clone(),
            kind: NewKind::Folder,
        },
        &ctx,
        &sink(),
    );
    let action = created.undo.ok_or("no undo recorded")?;
    let made = dir.join("Untitled folder");
    // The folder we made was moved away; another empty one took its name.
    fs::rename(&made, scratch.0.join("kept"))?;
    fs::create_dir(&made)?;

    let refused = run_job(Job::Undo(action), &ctx, &sink());
    assert_eq!(
        text(&refused),
        "Can't undo: \u{201c}Untitled folder\u{201d} changed since it was created"
    );
    assert!(made.is_dir(), "the other folder is untouched");
    assert!(scratch.0.join("kept").is_dir());
    assert!(!scratch.trash().exists(), "nothing was trashed");
    Ok(())
}

// --- partial undo (R3/R4) -----------------------------------------------------------

/// Three trashed files and the undo entry of the one job that trashed them.
fn trashed_three(
    scratch: &Scratch,
) -> Result<(JobCtx, [PathBuf; 3], UndoAction), Box<dyn std::error::Error>> {
    let a = scratch.file("a.txt", "A")?;
    let b = scratch.file("b.txt", "B")?;
    let c = scratch.file("c.txt", "C")?;
    let ctx = scratch.ctx();
    let action = trash_paths(&ctx, &[&a, &b, &c])
        .undo
        .ok_or("no undo recorded")?;
    Ok((ctx, [a, b, c], action))
}

fn entries_of(action: &UndoAction) -> Result<&[TrashedEntry], &'static str> {
    match action {
        UndoAction::Trash { items } => Ok(items),
        _ => Err("not a trash undo"),
    }
}

#[test]
fn undoing_a_trash_restores_the_rest_when_one_item_left_the_trash() -> TestResult {
    let scratch = Scratch::new("undo-partial-missing")?;
    let (ctx, [a, b, c], action) = trashed_three(&scratch)?;
    // Emptied from the Trash by someone else in the meantime.
    fs::remove_file(scratch.trash().join("files/b.txt"))?;

    let report = run_job(Job::Undo(action), &ctx, &sink());
    assert_eq!(severity(&report), Some(Severity::Warning));
    assert_eq!(text(&report), "Restored 2 items; 1 skipped");
    assert_eq!(
        report.details,
        ["\u{201c}b.txt\u{201d} is no longer in the Trash"]
    );
    assert_eq!(report.select, [a.clone(), c.clone()]);
    assert_eq!(report.undo, None, "b can never come back: nothing remains");
    assert_eq!(fs::read_to_string(&a)?, "A");
    assert_eq!(fs::read_to_string(&c)?, "C");
    assert!(!b.exists());
    Ok(())
}

#[test]
fn a_cancelled_undo_keeps_what_was_not_restored_undoable() -> TestResult {
    let scratch = Scratch::new("undo-partial-cancel")?;
    let (ctx, [a, b, c], action) = trashed_three(&scratch)?;
    let entries = entries_of(&action)?;

    // Stop before the second item.
    let report = restore_entries(entries, &sink(), |index| index >= 1);
    assert!(report.cancelled);
    assert_eq!(severity(&report), Some(Severity::Info));
    assert_eq!(
        text(&report),
        "Cancelled: restored \u{201c}a.txt\u{201d}; 2 skipped"
    );
    assert_eq!(report.select, std::slice::from_ref(&a));
    assert_eq!(fs::read_to_string(&a)?, "A");
    assert!(!b.exists() && !c.exists());
    assert_eq!(
        report.undo,
        Some(UndoAction::Trash {
            items: entries.get(1..).unwrap_or_default().to_vec()
        }),
        "the items still in the Trash stay undoable"
    );

    // The remaining entry restores them.
    let rest = report.undo.ok_or("no remaining undo")?;
    let finished = run_job(Job::Undo(rest), &ctx, &sink());
    assert_eq!(severity(&finished), Some(Severity::Success));
    assert_eq!(text(&finished), "Restored 2 items");
    assert_eq!(finished.undo, None);
    assert_eq!(fs::read_to_string(&b)?, "B");
    assert_eq!(fs::read_to_string(&c)?, "C");
    Ok(())
}

#[test]
fn undoing_with_the_cancel_flag_already_set_restores_nothing_and_keeps_all() -> TestResult {
    let scratch = Scratch::new("undo-pre-cancelled")?;
    let (ctx, [a, b, c], action) = trashed_three(&scratch)?;
    let cancel = sink();
    cancel.request_cancel();
    let report = run_job(Job::Undo(action.clone()), &ctx, &cancel);
    assert!(report.cancelled);
    assert_eq!(text(&report), "Cancelled: nothing was restored");
    assert_eq!(report.undo, Some(action));
    assert!(!a.exists() && !b.exists() && !c.exists());
    assert!(report.select.is_empty());
    Ok(())
}

#[test]
fn a_failed_item_is_reported_and_stays_undoable_without_stopping_the_rest() -> TestResult {
    let scratch = Scratch::new("undo-partial-failed")?;
    let keep = scratch.file("ok/a.txt", "A")?;
    let nested = scratch.file("blocked/deep/b.txt", "B")?;
    let also = scratch.file("ok/c.txt", "C")?;
    let ctx = scratch.ctx();
    let action = trash_paths(&ctx, &[&keep, &nested, &also])
        .undo
        .ok_or("no undo recorded")?;
    // The folder `b.txt` came from is now a plain file: restoring cannot
    // recreate its parents (an I/O error, not a name clash).
    fs::remove_dir_all(scratch.0.join("blocked"))?;
    fs::write(scratch.0.join("blocked"), "in the way")?;

    let report = run_job(Job::Undo(action.clone()), &ctx, &sink());
    assert_eq!(severity(&report), Some(Severity::Warning));
    assert_eq!(text(&report), "Restored 2 items; 1 failed");
    assert_eq!(report.details.len(), 1);
    assert!(
        report
            .details
            .first()
            .is_some_and(|line| line.contains("b.txt")),
        "{:?}",
        report.details
    );
    assert_eq!(report.select, [keep.clone(), also.clone()]);
    let remaining = report.undo.ok_or("the failed item must stay undoable")?;
    let items = entries_of(&remaining)?;
    assert_eq!(
        items
            .iter()
            .map(|entry| entry.item.name.clone())
            .collect::<Vec<_>>(),
        [os("b.txt")]
    );
    assert_eq!(
        fs::read_to_string(scratch.trash().join("files/b.txt"))?,
        "B",
        "still in the Trash"
    );
    Ok(())
}

#[test]
fn an_undo_blocked_by_a_taken_path_stays_undoable() -> TestResult {
    let scratch = Scratch::new("undo-taken")?;
    let a = scratch.file("a.txt", "original")?;
    let ctx = scratch.ctx();
    let action = trash_paths(&ctx, &[&a]).undo.ok_or("no undo recorded")?;
    scratch.file("a.txt", "newer")?;

    let blocked = run_job(Job::Undo(action.clone()), &ctx, &sink());
    assert_eq!(
        text(&blocked),
        "Can't undo: \u{201c}a.txt\u{201d} already exists"
    );
    assert_eq!(blocked.undo, Some(action), "retry after freeing the name");

    fs::remove_file(&a)?;
    let retry = run_job(Job::Undo(blocked.undo.ok_or("kept")?), &ctx, &sink());
    assert_eq!(text(&retry), "Restored \u{201c}a.txt\u{201d}");
    assert_eq!(fs::read_to_string(&a)?, "original");
    Ok(())
}

// --- dialogs (R4) --------------------------------------------------------------------

fn delete_dialog(path: &str) -> DialogState {
    let path = PathBuf::from(path);
    let name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    DialogState::new(
        DialogKind::DeletePermanently,
        &[name],
        DialogAction::DeletePermanently(vec![path]),
    )
}

fn paths_of(dialog: &DialogState) -> Vec<PathBuf> {
    match &dialog.action {
        DialogAction::DeletePermanently(paths) => paths.clone(),
        _ => Vec::new(),
    }
}

#[test]
fn a_dialog_that_arrives_while_one_is_open_waits_and_never_retargets_it() -> TestResult {
    let mut queue = DialogQueue::default();
    assert!(!queue.is_open());
    queue.offer(delete_dialog("/x/a"));
    // A background job finishes: its follow-up must not replace the dialog.
    queue.offer(no_trash_dialog(vec![PathBuf::from("/x/b")]));
    queue.offer(no_trash_dialog(vec![PathBuf::from("/x/c")]));

    let shown = queue.current().ok_or("a dialog is open")?;
    assert_eq!(shown.text.title, "Delete \u{201c}a\u{201d} permanently?");
    assert_eq!(paths_of(shown), [PathBuf::from("/x/a")]);
    assert_eq!(queue.waiting(), 2);

    // Confirming acts on exactly what that dialog showed.
    let closed = queue.close().ok_or("closing the open dialog")?;
    assert_eq!(paths_of(&closed), [PathBuf::from("/x/a")]);
    let next = queue.current().ok_or("the queued dialog opens")?;
    assert_eq!(next.text.title, "No Trash available");
    assert_eq!(paths_of(next), [PathBuf::from("/x/b")]);

    let closed = queue.close().ok_or("closing the second")?;
    assert_eq!(paths_of(&closed), [PathBuf::from("/x/b")]);
    assert_eq!(
        queue.current().map(paths_of),
        Some(vec![PathBuf::from("/x/c")])
    );
    queue.close();
    assert!(!queue.is_open());
    assert!(queue.close().is_none());
    Ok(())
}

#[test]
fn the_no_trash_dialog_is_bound_to_the_paths_it_names() {
    let dialog = no_trash_dialog(vec![PathBuf::from("/x/a"), PathBuf::from("/y/b")]);
    assert_eq!(dialog.kind, DialogKind::NoTrash);
    assert_eq!(dialog.text.names, ["a", "b"]);
    assert_eq!(
        paths_of(&dialog),
        [PathBuf::from("/x/a"), PathBuf::from("/y/b")]
    );
}

#[test]
fn dismissing_the_no_trash_dialog_reports_the_items_it_dropped() {
    let one = dismissed_notice(&no_trash_dialog(vec![PathBuf::from("/x/a")]));
    assert_eq!(
        one,
        Some(Notice {
            severity: Severity::Info,
            text: "1 item not deleted (no Trash available)".to_owned()
        })
    );
    let two = dismissed_notice(&no_trash_dialog(vec![
        PathBuf::from("/x/a"),
        PathBuf::from("/x/b"),
    ]));
    assert_eq!(
        two.map(|notice| notice.text),
        Some("2 items not deleted (no Trash available)".to_owned())
    );
    // Declining a delete the user asked for says nothing more.
    assert_eq!(dismissed_notice(&delete_dialog("/x/a")), None);
    let empty = DialogState::new(
        DialogKind::EmptyTrash { count: 3 },
        &[],
        DialogAction::EmptyTrash(vec![os("a")]),
    );
    assert_eq!(dismissed_notice(&empty), None);
}

#[test]
fn a_phone_has_no_trash_so_the_delete_asks_for_permanent_deletion() -> TestResult {
    let scratch = Scratch::new("gvfs-no-trash")?;
    let gvfs = scratch.dir("run/gvfs")?;
    let dcim = scratch.dir("run/gvfs/mtp:host=Fake_Phone_123/Internal shared storage/DCIM")?;
    let photo = scratch.file(
        "run/gvfs/mtp:host=Fake_Phone_123/Internal shared storage/DCIM/IMG_1.jpg",
        "x",
    )?;
    let local = scratch.file("local.txt", "y")?;
    // Even listed as a device of its own (a mount point of the sidebar), and
    // on the very device the home Trash is on, a gvfs path is never trashed
    // and no `.Trash-<uid>` is created on the phone.
    let phone = gvfs.join("mtp:host=Fake_Phone_123");
    let ctx = JobCtx::from_env_with(
        scratch.trash(),
        vec![phone.clone()],
        FuseRoots::from_table(b"", Some(&gvfs)),
    );

    let report = run_job(
        Job::Trash(vec![photo.clone(), local.clone()]),
        &ctx,
        &sink(),
    );

    assert_eq!(report.needs_permanent, std::slice::from_ref(&photo));
    assert!(photo.exists(), "nothing was deleted");
    assert!(!local.exists(), "the local file went to the Trash");
    assert_eq!(fs::read_dir(&dcim)?.count(), 1);
    assert!(!phone.join(".Trash-1000").exists());
    let created: Vec<_> = fs::read_dir(&phone)?
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(created, [os("Internal shared storage")]);
    Ok(())
}

#[test]
fn a_cancelled_trash_job_reports_what_needed_permanent_deletion() -> TestResult {
    let scratch = Scratch::new("cancel-no-trash")?;
    let a = scratch.file("a.txt", "a")?;
    let b = scratch.file("b.txt", "b")?;
    let c = scratch.file("c.txt", "c")?;
    let cancel = sink();
    let trigger = cancel.clone();
    // `a` has no Trash, and the user cancels right then.
    let ctx = JobCtx {
        trash_root: scratch.trash(),
        target_for: Box::new(move |_| {
            trigger.request_cancel();
            Err(TrashError::NoTrash("no trash here".to_owned()))
        }),
    };
    let report = run_job(
        Job::Trash(vec![a.clone(), b.clone(), c.clone()]),
        &ctx,
        &cancel,
    );
    assert!(report.cancelled);
    assert_eq!(report.needs_permanent, std::slice::from_ref(&a));
    assert_eq!(severity(&report), Some(Severity::Warning));
    assert_eq!(
        text(&report),
        "Cancelled: nothing was moved to the Trash; 1 item not deleted (no Trash available)"
    );
    assert_eq!(
        report.details,
        ["\u{201c}a.txt\u{201d}: not deleted (no Trash available)"]
    );
    assert!(a.exists() && b.exists() && c.exists());
    Ok(())
}

#[test]
fn a_cancelled_trash_job_keeps_its_moved_items_and_its_undo() -> TestResult {
    let scratch = Scratch::new("cancel-mixed")?;
    let a = scratch.file("a.txt", "a")?;
    let b = scratch.file("b.txt", "b")?;
    let c = scratch.file("c.txt", "c")?;
    let cancel = sink();
    let trigger = cancel.clone();
    let real = scratch.ctx();
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let ctx = JobCtx {
        trash_root: scratch.trash(),
        target_for: Box::new(move |path| {
            if calls.fetch_add(1, Ordering::Relaxed) == 0 {
                (real.target_for)(path)
            } else {
                trigger.request_cancel();
                Err(TrashError::NoTrash("no trash here".to_owned()))
            }
        }),
    };
    let report = run_job(
        Job::Trash(vec![a.clone(), b.clone(), c.clone()]),
        &ctx,
        &cancel,
    );
    assert_eq!(
        text(&report),
        "Cancelled: moved \u{201c}a.txt\u{201d} to Trash; 1 item not deleted (no Trash available)"
    );
    assert_eq!(report.needs_permanent, std::slice::from_ref(&b));
    assert!(matches!(&report.undo, Some(UndoAction::Trash { items }) if items.len() == 1));
    assert!(!a.exists() && b.exists() && c.exists());
    Ok(())
}

#[test]
fn a_finished_trash_job_still_hands_the_dialog_its_paths() -> TestResult {
    let scratch = Scratch::new("no-trash-dialog")?;
    let a = scratch.file("a.txt", "a")?;
    let report = trash_paths(&no_trash_ctx(scratch.trash()), &[&a]);
    assert!(!report.cancelled);
    assert_eq!(report.notice, None, "the dialog speaks, not a toast");
    assert_eq!(report.needs_permanent, std::slice::from_ref(&a));
    Ok(())
}

// --- UI safety decisions (R3) --------------------------------------------------------

#[test]
fn enter_alone_never_confirms_a_dialog() {
    assert_eq!(DIALOG_DEFAULT, DialogChoice::Cancel);
    // Enter presses the focused button and no key binding confirms: the
    // only Enter binding the explorer has is the entries' "open", which a
    // dialog's own key context shadows.
    let bindings = include_str!("view.rs");
    for line in bindings
        .lines()
        .filter(|line| line.contains("KeyBinding::new(\"enter\""))
    {
        assert!(
            !line.contains("Modal"),
            "Enter must not be bound inside a dialog: {line}"
        );
    }
    assert!(
        !bindings.contains("ConfirmDialog"),
        "no action may confirm a dialog from the keyboard on its own"
    );
}

/// Every kind of dialog the explorer can open, with a conflict question.
fn every_dialog() -> Vec<DialogState> {
    let kinds = [
        DialogKind::DeletePermanently,
        DialogKind::NoTrash,
        DialogKind::DeleteFromTrash,
        DialogKind::EmptyTrash { count: 2 },
    ];
    let mut dialogs: Vec<DialogState> = kinds
        .into_iter()
        .map(|kind| DialogState::new(kind, &names(&["a"]), DialogAction::EmptyTrash(Vec::new())))
        .collect();
    dialogs.push(DialogState::conflict(
        1,
        paste::ConflictRequest {
            kind: DialogKind::Conflict {
                name: "a".to_owned(),
                folder: "b".to_owned(),
                existing: "1 B".to_owned(),
                incoming: "2 B".to_owned(),
            },
        },
    ));
    dialogs
}

#[test]
fn every_dialog_opens_on_a_safe_button_whether_it_opened_at_once_or_was_promoted() {
    let t0 = Instant::now();
    for dialog in every_dialog() {
        // Opened at once, in an empty queue.
        let mut queue = DialogQueue::default();
        queue.offer_at(dialog.clone(), t0);
        let at_once = queue.default_focus();
        assert!(
            at_once.is_some_and(|choice| !choice.is_danger()),
            "{dialog:?}"
        );

        // Promoted from the line behind another dialog.
        let mut queue = DialogQueue::default();
        queue.offer_at(delete_dialog("/x/first"), t0);
        queue.offer_at(dialog.clone(), t0);
        assert_eq!(queue.default_focus(), Some(DialogChoice::Cancel));
        queue.close_at(t0);
        assert_eq!(queue.default_focus(), at_once, "promotion decides the same");
    }
}

#[test]
fn a_conflict_question_opens_on_skip_and_a_confirmation_on_cancel() {
    let t0 = Instant::now();
    let mut queue = DialogQueue::default();
    queue.offer_at(delete_dialog("/x/a"), t0);
    assert_eq!(queue.default_focus(), Some(DialogChoice::Cancel));
    for dialog in every_dialog() {
        let expected = match dialog.kind {
            DialogKind::Conflict { .. } => DialogChoice::Skip,
            _ => DialogChoice::Cancel,
        };
        assert_eq!(dialog.initial_focus(), expected);
    }
    assert!(
        !CONFLICT_DEFAULT.is_danger(),
        "Replace is never the default"
    );
    assert!(!DIALOG_DEFAULT.is_danger());
    assert!(DialogChoice::Confirm.is_danger());
}

#[test]
fn enter_on_the_focused_button_never_reaches_a_danger_button() {
    // Enter presses the button that has the focus; the queue decides that
    // focus, and a press of it is accepted at once without being a danger
    // press, even before the dialog is armed.
    let t0 = Instant::now();
    for dialog in every_dialog() {
        let mut queue = DialogQueue::default();
        queue.offer_at(delete_dialog("/x/first"), t0);
        queue.offer_at(dialog, t0);
        queue.close_at(t0);
        let focused = queue.default_focus();
        assert!(focused.is_some_and(|choice| !choice.is_danger()));
        let pressed = focused.unwrap_or(DialogChoice::Confirm);
        assert!(queue.accepts(pressed, t0));
        assert_ne!(pressed, DialogChoice::Confirm);
    }
}

#[test]
fn the_danger_button_arms_after_the_delay_and_not_before() {
    let t0 = Instant::now();
    assert!(!is_armed(t0, t0));
    assert!(!is_armed(t0, t0 + ARM_DELAY - Duration::from_millis(1)));
    assert!(is_armed(t0, t0 + ARM_DELAY));
    assert!(is_armed(t0, t0 + Duration::from_secs(60)));
    // A clock that went backwards never arms early.
    assert!(!is_armed(t0 + Duration::from_secs(5), t0));
    assert_eq!(ARM_DELAY, Duration::from_millis(400));
}

#[test]
fn a_new_dialog_ignores_its_danger_button_for_the_arming_delay() {
    let t0 = Instant::now();
    let mut queue = DialogQueue::default();
    assert!(!queue.armed_at(t0), "nothing open, nothing to confirm");
    assert!(!queue.accepts(DialogChoice::Cancel, t0));

    queue.offer_at(delete_dialog("/x/a"), t0);
    let early = t0 + Duration::from_millis(399);
    assert!(!queue.accepts(DialogChoice::Confirm, early));
    assert!(
        queue.accepts(DialogChoice::Cancel, early),
        "cancel is always live"
    );
    assert!(queue.accepts(DialogChoice::Confirm, t0 + ARM_DELAY));
}

#[test]
fn a_promoted_dialog_starts_its_own_clock_when_it_appears() {
    let t0 = Instant::now();
    let mut queue = DialogQueue::default();
    queue.offer_at(delete_dialog("/x/a"), t0);
    queue.offer_at(no_trash_dialog(vec![PathBuf::from("/x/b")]), t0);

    // The user double-clicks "Delete permanently" long after the second
    // dialog was queued: the first click closes the first dialog, the
    // second click lands on the promoted one.
    let click = t0 + Duration::from_secs(30);
    assert!(queue.accepts(DialogChoice::Confirm, click));
    queue.close_at(click);
    let trailing = click + Duration::from_millis(80);
    assert!(
        !queue.accepts(DialogChoice::Confirm, trailing),
        "the trailing click must not confirm the dialog that just appeared"
    );
    assert!(queue.is_open(), "and it leaves the dialog open");
    assert!(queue.accepts(DialogChoice::Confirm, click + ARM_DELAY));
}

#[test]
fn a_conflict_question_of_a_finished_job_is_withdrawn_from_the_line() {
    let t0 = Instant::now();
    let question = |job| {
        DialogState::conflict(
            job,
            paste::ConflictRequest {
                kind: DialogKind::Conflict {
                    name: "a".to_owned(),
                    folder: "b".to_owned(),
                    existing: String::new(),
                    incoming: String::new(),
                },
            },
        )
    };
    let mut queue = DialogQueue::default();
    queue.offer_at(question(1), t0);
    queue.offer_at(delete_dialog("/x/a"), t0);
    queue.offer_at(question(2), t0);

    assert!(
        !queue.discard_conflicts_of(2, t0),
        "a waiting question going away leaves the open dialog alone"
    );
    assert_eq!(queue.waiting(), 1, "only its own question is dropped");
    assert!(
        queue
            .current()
            .is_some_and(|dialog| dialog.conflict_job() == Some(1))
    );

    // The open one: dropped, and the next in line appears (and arms later).
    assert!(queue.discard_conflicts_of(1, t0));
    let now_open = queue.current();
    assert!(now_open.is_some_and(|dialog| dialog.kind == DialogKind::DeletePermanently));
    assert!(!queue.armed_at(t0));
    assert!(
        !queue.discard_conflicts_of(9, t0),
        "an unknown job changes nothing"
    );
}

#[test]
fn a_rename_that_is_refused_on_blur_cancels_and_changes_nothing() -> TestResult {
    let scratch = Scratch::new("rename-blur")?;
    let a = scratch.file("a.txt", "A")?;
    let b = scratch.file("b.txt", "B")?;
    let siblings = [os("a.txt"), os("b.txt")];
    let check = |typed: &str| {
        check_rename(
            OsStr::new("a.txt"),
            typed,
            siblings.iter().map(|n| n.as_os_str()),
        )
    };

    // Typing a name that exists, then clicking elsewhere.
    let blurred = rename_step(check("b.txt"), true);
    assert_eq!(
        blurred,
        RenameStep::Cancel("Rename canceled: \u{201c}b.txt\u{201d} already exists here".to_owned())
    );
    assert!(matches!(
        rename_step(check("x/y"), true),
        RenameStep::Cancel(_)
    ));
    assert!(matches!(
        rename_step(check(""), true),
        RenameStep::Cancel(_)
    ));
    assert_eq!(fs::read_to_string(&a)?, "A", "no job was started");
    assert_eq!(fs::read_to_string(&b)?, "B");
    assert_eq!(listing(&scratch.0)?, ["a.txt", "b.txt"]);

    // The same name with Enter keeps the editor open to be fixed.
    assert_eq!(
        rename_step(check("b.txt"), false),
        RenameStep::KeepOpen("\u{201c}b.txt\u{201d} already exists here".to_owned())
    );
    // Valid names and untouched ones.
    assert_eq!(
        rename_step(check("c.txt"), true),
        RenameStep::Run(os("c.txt"))
    );
    assert_eq!(rename_step(check("a.txt"), true), RenameStep::Close);
    assert_eq!(rename_step(check("a.txt"), false), RenameStep::Close);
    Ok(())
}

#[test]
fn rename_seed_refuses_every_control_character() {
    for name in [
        "a\nb", "c\td", "e\u{1}f", "g\u{7f}h", "i\u{1b}j", "k\rl", "m\u{85}n",
    ] {
        assert!(rename_seed(OsStr::new(name)).is_err(), "{name:?}");
    }
    assert!(rename_seed(OsStr::new("zz\u{2028}line")).is_ok());
}

// --- activating an entry ---------------------------------------------------------

#[test]
fn enter_navigates_into_a_real_folder_and_a_symlink_to_one() -> TestResult {
    let scratch = Scratch::new("activate")?;
    let target = scratch.dir("target")?;
    scratch.file("target/inside.txt", "x")?;
    scratch.file("plain.txt", "x")?;
    unix_fs::symlink(&target, scratch.0.join("to-dir"))?;
    unix_fs::symlink(scratch.0.join("plain.txt"), scratch.0.join("to-file"))?;
    unix_fs::symlink(scratch.0.join("nothing-here"), scratch.0.join("broken"))?;

    let listing = fileflux::core::fs::read_directory(&scratch.0)?;
    let action = |name: &str| {
        listing
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .map(open_action)
    };
    assert_eq!(action("target"), Some(OpenAction::Navigate));
    assert_eq!(
        action("to-dir"),
        Some(OpenAction::Navigate),
        "a link to a folder goes into it, not to xdg-open"
    );
    assert_eq!(action("plain.txt"), Some(OpenAction::External));
    assert_eq!(action("to-file"), Some(OpenAction::External));
    assert_eq!(
        action("broken"),
        Some(OpenAction::External),
        "nothing to enter"
    );
    Ok(())
}

// --- opening images in the viewer --------------------------------------------------

fn listed(name: &str, kind: fileflux::core::EntryKind) -> Entry {
    Entry {
        name: name.into(),
        path: PathBuf::from("/pics").join(name),
        kind,
        is_symlink: false,
        size: None,
        modified: None,
        origin: None,
    }
}

fn paths(names: &[&str]) -> Vec<PathBuf> {
    names
        .iter()
        .map(|name| PathBuf::from("/pics").join(name))
        .collect()
}

#[test]
fn an_image_opens_the_viewer_on_the_displayed_images_in_displayed_order() {
    use fileflux::core::EntryKind::{Directory, File};
    // The displayed order is not the name order (a size or date sort).
    let entries = [
        listed("sub", Directory),
        listed("c.png", File),
        listed("notes.txt", File),
        listed("A.JPG", File),
        listed("b.gif", File),
        listed("fake.png", Directory),
    ];
    let displayed: Vec<&Entry> = entries.iter().collect();
    assert_eq!(
        open_target(Path::new("/pics/A.JPG"), &displayed),
        OpenTarget::Viewer {
            list: paths(&["c.png", "A.JPG", "b.gif"]),
            index: 1,
        }
    );
    assert_eq!(
        open_target(Path::new("/pics/c.png"), &displayed),
        OpenTarget::Viewer {
            list: paths(&["c.png", "A.JPG", "b.gif"]),
            index: 0,
        }
    );
}

#[test]
fn an_image_outside_the_displayed_entries_is_viewed_alone() {
    use fileflux::core::EntryKind::File;
    let entries = [listed("a.png", File)];
    let displayed: Vec<&Entry> = entries.iter().collect();
    assert_eq!(
        open_target(Path::new("/pics/hidden.png"), &displayed),
        OpenTarget::Viewer {
            list: paths(&["hidden.png"]),
            index: 0,
        }
    );
}

#[test]
fn other_files_keep_the_default_application() {
    use fileflux::core::EntryKind::File;
    let entries = [listed("a.png", File), listed("notes.txt", File)];
    let displayed: Vec<&Entry> = entries.iter().collect();
    for name in ["notes.txt", "photo.avif", "png", "Makefile"] {
        assert_eq!(
            open_target(&PathBuf::from("/pics").join(name), &displayed),
            OpenTarget::External,
            "{name}"
        );
    }
}

#[test]
fn open_in_viewer_follows_open_and_leaves_open_with_in_place() {
    let one_file = MenuTarget {
        single_file: true,
        ..target(false, 1, false, false)
    };
    let mut rows = menu_rows(&one_file);
    let before = flat(&rows);
    add_viewer_row(&mut rows);
    let mut expected = before.clone();
    expected.insert(1, "Open in viewer".to_owned());
    assert_eq!(flat(&rows), expected);
    assert!(item(&rows, MenuCommand::OpenWith).is_some());
    assert_eq!(
        item(&rows, MenuCommand::OpenInViewer).map(|spec| (spec.enabled, spec.danger)),
        Some((true, false))
    );

    // Without an "Open" row (the empty space, the Trash) nothing is added.
    for target in [target(false, 0, false, true), target(true, 1, false, false)] {
        let mut rows = menu_rows(&target);
        let before = flat(&rows);
        add_viewer_row(&mut rows);
        assert_eq!(flat(&rows), before);
    }
}
