//! The GTK bookmarks file (`$XDG_CONFIG_HOME/gtk-3.0/bookmarks`) as the
//! favorites store: parsing, labels, percent-encoding, the lines Fileflux
//! does not understand, and the atomic, order-preserving writes. Every test
//! works in a fresh directory under the system temp directory and never
//! touches the user's real bookmarks.

#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use fileflux::core::bookmarks::{
    Bookmark, add, add_observed, bookmarks_path, file_uri, merge, parse, read, remove, rename,
    reorder,
};

/// A scratch directory removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fileflux-bookmarks-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(&dir).expect("scratch directory is created");
        Self(dir)
    }

    fn file(&self) -> PathBuf {
        self.0.join("gtk-3.0/bookmarks")
    }

    fn write(&self, text: &[u8]) -> PathBuf {
        let file = self.file();
        std::fs::create_dir_all(file.parent().expect("has a parent")).expect("dir creates");
        std::fs::write(&file, text).expect("fixture writes");
        file
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.0));
    }
}

fn favorites(bytes: &[u8]) -> Vec<(PathBuf, Option<String>)> {
    parse(bytes)
        .favorites()
        .into_iter()
        .map(|Bookmark { path, label }| (path, label))
        .collect()
}

fn bytes_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

#[test]
fn the_file_follows_xdg_config_home_and_falls_back_to_home() {
    assert_eq!(
        bookmarks_path(Some("/h".as_ref()), Some("/cfg".as_ref())),
        Some(PathBuf::from("/cfg/gtk-3.0/bookmarks"))
    );
    assert_eq!(
        bookmarks_path(Some("/h".as_ref()), None),
        Some(PathBuf::from("/h/.config/gtk-3.0/bookmarks"))
    );
    assert_eq!(
        bookmarks_path(Some("/h".as_ref()), Some("relative".as_ref())),
        Some(PathBuf::from("/h/.config/gtk-3.0/bookmarks")),
        "a relative XDG variable is ignored"
    );
    assert_eq!(bookmarks_path(None, None), None);
    assert_eq!(bookmarks_path(Some("".as_ref()), None), None);
}

#[test]
fn local_entries_are_favorites_with_their_label_when_present() {
    let text = b"file:///home/u/Projects\nfile:///srv/data Shared data\nfile:///x   \n";
    assert_eq!(
        favorites(text),
        [
            (PathBuf::from("/home/u/Projects"), None),
            (PathBuf::from("/srv/data"), Some("Shared data".to_owned())),
            (PathBuf::from("/x"), None),
        ]
    );
}

#[test]
fn percent_escapes_decode_to_spaces_unicode_and_raw_non_utf8_bytes() {
    let text = b"file:///home/u/My%20Docs\n\
                 file:///home/u/caf%C3%A9%20%E2%82%AC Caf\xc3\xa9\n\
                 file:///data/caf%E9%FF\n\
                 file:///100%25/a%23b\n\
                 file:///broken%zz%4\n";
    assert_eq!(
        favorites(text),
        [
            (PathBuf::from("/home/u/My Docs"), None),
            (
                PathBuf::from("/home/u/caf\u{e9} \u{20ac}"),
                Some("Caf\u{e9}".to_owned())
            ),
            (bytes_path(b"/data/caf\xe9\xff"), None),
            (PathBuf::from("/100%/a#b"), None),
            (PathBuf::from("/broken%zz%4"), None),
        ]
    );
}

#[test]
fn remote_comment_blank_and_relative_lines_are_not_favorites() {
    let text = b"# a comment\n\
                 \n\
                 sftp://host/home/u Server\n\
                 smb://nas/share\n\
                 file://otherhost/srv\n\
                 file://relative\n\
                 file:///kept\n\
                 garbage\n";
    assert_eq!(favorites(text), [(PathBuf::from("/kept"), None)]);
}

#[test]
fn localhost_is_a_local_authority() {
    assert_eq!(
        favorites(b"file://localhost/srv/a\n"),
        [(PathBuf::from("/srv/a"), None)]
    );
}

#[test]
fn crlf_and_a_missing_final_newline_parse_the_same() {
    assert_eq!(
        favorites(b"file:///a Label\r\nfile:///b\r\nfile:///c"),
        [
            (PathBuf::from("/a"), Some("Label".to_owned())),
            (PathBuf::from("/b"), None),
            (PathBuf::from("/c"), None),
        ]
    );
}

#[test]
fn a_missing_file_has_no_favorites_and_an_unreadable_one_is_an_error() {
    let scratch = Scratch::new("missing");
    assert_eq!(read(&scratch.file()).expect("missing is empty"), []);
    // A directory where the file should be cannot be read as a file.
    std::fs::create_dir_all(scratch.file()).expect("dir creates");
    assert!(read(&scratch.file()).is_err());
}

#[test]
fn file_uris_escape_everything_but_unreserved_characters_and_slashes() {
    assert_eq!(
        file_uri(Path::new("/home/u/Projects")),
        "file:///home/u/Projects"
    );
    assert_eq!(
        file_uri(Path::new("/a b/ü#%")),
        "file:///a%20b/%C3%BC%23%25"
    );
    assert_eq!(file_uri(&bytes_path(b"/d/caf\xe9")), "file:///d/caf%E9");
    for path in [
        PathBuf::from("/a b/ü#%?&=+"),
        bytes_path(b"/d/caf\xe9\xff/x y"),
        PathBuf::from("/plain"),
    ] {
        let text = format!("{}\n", file_uri(&path));
        assert_eq!(favorites(text.as_bytes()), [(path, None)]);
    }
}

#[test]
fn adding_appends_one_line_and_keeps_everything_else_byte_for_byte() {
    let scratch = Scratch::new("add");
    let original: &[u8] =
        b"# mine\nfile:///home/u/Docs Documents\nsftp://host/srv Server\n\nsmb://nas/x\n";
    let file = scratch.write(original);

    assert!(add(&file, Path::new("/home/u/New Folder")).expect("adds"));

    let mut expected = original.to_vec();
    expected.extend_from_slice(b"file:///home/u/New%20Folder\n");
    assert_eq!(std::fs::read(&file).expect("reads"), expected);
}

#[test]
fn adding_a_present_folder_changes_nothing_even_with_another_spelling() {
    let scratch = Scratch::new("add-twice");
    let original = b"file:///home/u/Docs Documents\n";
    let file = scratch.write(original);
    assert!(!add(&file, Path::new("/home/u/Docs")).expect("no-op"));
    assert!(!add(&file, Path::new("/home/u/Docs/")).expect("no-op"));
    assert_eq!(std::fs::read(&file).expect("reads"), original);
}

#[test]
fn adding_to_a_missing_file_creates_it_and_its_directory() {
    let scratch = Scratch::new("add-missing");
    let file = scratch.file();
    assert!(add(&file, Path::new("/srv/a")).expect("adds"));
    assert_eq!(std::fs::read(&file).expect("reads"), b"file:///srv/a\n");
    let leftovers: Vec<_> = std::fs::read_dir(file.parent().expect("parent"))
        .expect("reads")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(
        leftovers,
        [OsString::from("bookmarks")],
        "no temporary file"
    );
}

#[test]
fn adding_keeps_crlf_endings_and_terminates_an_unterminated_last_line() {
    let scratch = Scratch::new("add-crlf");
    let file = scratch.write(b"file:///a\r\nfile:///b");
    add(&file, Path::new("/c")).expect("adds");
    assert_eq!(
        std::fs::read(&file).expect("reads"),
        b"file:///a\r\nfile:///b\r\nfile:///c\r\n"
    );

    let scratch = Scratch::new("add-unterminated");
    let file = scratch.write(b"file:///a\nfile:///b");
    add(&file, Path::new("/c")).expect("adds");
    assert_eq!(
        std::fs::read(&file).expect("reads"),
        b"file:///a\nfile:///b\nfile:///c\n"
    );
}

#[test]
fn removing_drops_only_the_matching_lines_and_keeps_order_labels_and_unknown_lines() {
    let scratch = Scratch::new("remove");
    let file = scratch.write(
        b"# top\nfile:///a Alpha\nsftp://h/x Remote\nfile:///b%20c\nfile:///b%20c Again\nfile:///d\n",
    );

    assert!(remove(&file, Path::new("/b c")).expect("removes"));

    assert_eq!(
        std::fs::read(&file).expect("reads"),
        b"# top\nfile:///a Alpha\nsftp://h/x Remote\nfile:///d\n"
    );
    assert!(!remove(&file, Path::new("/never-there")).expect("no-op"));
    assert!(!remove(&scratch.0.join("absent/bookmarks"), Path::new("/a")).expect("no-op"));
    assert!(
        !scratch.0.join("absent").exists(),
        "removing creates nothing"
    );
}

#[test]
fn removing_matches_a_non_utf8_path_by_its_bytes() {
    let scratch = Scratch::new("remove-bytes");
    let file = scratch.write(b"file:///d/caf%E9\nfile:///d/caf%C3%A9\n");
    assert!(remove(&file, &bytes_path(b"/d/caf\xe9")).expect("removes"));
    assert_eq!(
        std::fs::read(&file).expect("reads"),
        b"file:///d/caf%C3%A9\n"
    );
}

#[test]
fn merging_appends_the_missing_folders_in_order_and_reports_how_many() {
    let scratch = Scratch::new("merge");
    let file = scratch.write(b"file:///b Bee\nsftp://h/x\n");
    let added = merge(
        &file,
        &[
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/c d"),
            PathBuf::from("/a"),
        ],
    )
    .expect("merges");
    assert_eq!(added, 2);
    assert_eq!(
        std::fs::read(&file).expect("reads"),
        b"file:///b Bee\nsftp://h/x\nfile:///a\nfile:///c%20d\n"
    );
    assert_eq!(merge(&file, &[PathBuf::from("/a")]).expect("merges"), 0);
}

#[test]
fn a_write_keeps_the_file_mode() {
    let scratch = Scratch::new("mode");
    let file = scratch.write(b"file:///a\n");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).expect("chmod");
    add(&file, Path::new("/b")).expect("adds");
    let mode = std::fs::metadata(&file).expect("stat").permissions().mode() & 0o7777;
    assert_eq!(mode, 0o640);

    let scratch = Scratch::new("mode-new");
    let file = scratch.file();
    add(&file, Path::new("/b")).expect("adds");
    let mode = std::fs::metadata(&file).expect("stat").permissions().mode() & 0o7777;
    assert_eq!(mode, 0o644, "a new file is world-readable like GTK's");
}

#[test]
fn a_symlinked_file_is_written_through_and_stays_a_symlink() {
    let scratch = Scratch::new("symlink");
    let target = scratch.0.join("dotfiles/bookmarks");
    std::fs::create_dir_all(target.parent().expect("parent")).expect("dir creates");
    std::fs::write(&target, "file:///a\n").expect("fixture writes");
    let link = scratch.file();
    std::fs::create_dir_all(link.parent().expect("parent")).expect("dir creates");
    std::os::unix::fs::symlink(&target, &link).expect("symlink creates");

    add(&link, Path::new("/b")).expect("adds");

    assert!(
        std::fs::symlink_metadata(&link)
            .expect("stat")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read(&target).expect("reads"),
        b"file:///a\nfile:///b\n"
    );
}

#[test]
fn concurrent_adds_never_lose_a_line() {
    let scratch = Scratch::new("concurrent");
    let file = scratch.write(b"");
    std::thread::scope(|scope| {
        for worker in 0..4 {
            let file = &file;
            scope.spawn(move || {
                for index in 0..10 {
                    add(file, Path::new(&format!("/w{worker}/{index}"))).expect("adds");
                }
            });
        }
    });
    assert_eq!(read(&file).expect("reads").len(), 40);
}

#[test]
fn an_external_write_before_the_rename_is_merged_not_overwritten() {
    let scratch = Scratch::new("lost-update");
    let file = scratch.write(b"file:///a\n");
    let mut external = Some(());
    let hook_file = file.clone();
    let added = add_observed(&file, Path::new("/mine"), &mut || {
        if external.take().is_some() {
            std::fs::write(&hook_file, b"file:///a\nsftp://h/x Theirs\n").expect("other writes");
        }
    })
    .expect("adds");
    assert!(added);
    assert_eq!(
        std::fs::read(&file).expect("reads"),
        b"file:///a\nsftp://h/x Theirs\nfile:///mine\n"
    );
    let leftovers = std::fs::read_dir(file.parent().expect("parent"))
        .expect("reads")
        .count();
    assert_eq!(
        leftovers, 1,
        "the abandoned attempt leaves no temporary file"
    );
}

#[test]
fn an_external_write_that_already_added_the_folder_makes_the_add_a_no_op() {
    let scratch = Scratch::new("lost-update-same");
    let file = scratch.write(b"file:///a\n");
    let hook_file = file.clone();
    let mut once = true;
    let added = add_observed(&file, Path::new("/mine"), &mut || {
        if std::mem::take(&mut once) {
            std::fs::write(&hook_file, b"file:///a\nfile:///mine Theirs\n").expect("other writes");
        }
    })
    .expect("adds");
    assert!(!added);
    assert_eq!(
        std::fs::read(&file).expect("reads"),
        b"file:///a\nfile:///mine Theirs\n"
    );
}

#[test]
fn a_file_that_never_stops_changing_is_an_error_and_keeps_the_other_programs_content() {
    let scratch = Scratch::new("lost-update-storm");
    let file = scratch.write(b"");
    let hook_file = file.clone();
    let mut round = 0_u32;
    let result = add_observed(&file, Path::new("/mine"), &mut || {
        round += 1;
        std::fs::write(&hook_file, format!("file:///other{round}\n")).expect("other writes");
    });
    assert!(result.is_err());
    assert!(
        std::fs::read_to_string(&file)
            .expect("reads")
            .starts_with("file:///other")
    );
    let leftovers = std::fs::read_dir(file.parent().expect("parent"))
        .expect("reads")
        .count();
    assert_eq!(leftovers, 1, "no temporary file");
}

#[test]
fn relative_folders_are_rejected_by_add_and_merge_and_nothing_is_written() {
    let scratch = Scratch::new("relative");
    let original = b"file:///a\n";
    let file = scratch.write(original);
    let error = add(&file, Path::new("relative/dir")).expect_err("rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("relative/dir"), "{error}");
    let error = merge(&file, &[PathBuf::from("/ok"), PathBuf::from("rel")]).expect_err("rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(std::fs::read(&file).expect("reads"), original);

    let missing = scratch.0.join("absent/bookmarks");
    assert!(add(&missing, Path::new("")).is_err());
    assert!(!scratch.0.join("absent").exists(), "nothing is created");

    // An absolute folder whose parent is missing is a real change: only the
    // relative one must leave the missing directory alone.
    let nested = scratch.0.join("deeper/absent/bookmarks");
    assert!(add(&nested, Path::new("relative/dir")).is_err());
    assert!(merge(&nested, &[PathBuf::from("/ok"), PathBuf::from("rel")]).is_err());
    assert!(!scratch.0.join("deeper").exists(), "no parent is created");
    assert!(
        add(&nested, Path::new("/abs/dir")).is_ok(),
        "control: creates"
    );
    assert!(nested.exists());
}

// --- rename and reorder ---------------------------------------------------------

fn read_text(file: &Path) -> String {
    String::from_utf8(std::fs::read(file).expect("file reads")).expect("file is UTF-8")
}

#[test]
fn rename_sets_the_label_and_touches_no_other_line() {
    let scratch = Scratch::new("rename-set");
    let text = "# mine\nfile:///a\r\nsftp://h/p Server\nfile:///b Old\nfile:///c\n";
    let file = scratch.write(text.as_bytes());
    assert!(rename(&file, Path::new("/b"), "New name").expect("renames"));
    assert_eq!(
        read_text(&file),
        "# mine\nfile:///a\r\nsftp://h/p Server\nfile:///b New name\nfile:///c\n"
    );
    assert!(rename(&file, Path::new("/a"), "  Home  ").expect("renames"));
    assert_eq!(
        read_text(&file),
        "# mine\nfile:///a Home\r\nsftp://h/p Server\nfile:///b New name\nfile:///c\n",
        "the label is trimmed and the line keeps its own ending"
    );
}

#[test]
fn an_empty_label_restores_the_folder_name() {
    let scratch = Scratch::new("rename-clear");
    let file = scratch.write(b"file:///a Alpha\nfile:///b Beta");
    assert!(rename(&file, Path::new("/b"), "").expect("clears"));
    assert_eq!(read_text(&file), "file:///a Alpha\nfile:///b");
    assert_eq!(
        read(&file).expect("reads")[1],
        Bookmark {
            path: PathBuf::from("/b"),
            label: None
        }
    );
}

#[test]
fn rename_keeps_the_uri_as_written_and_the_missing_final_newline() {
    let scratch = Scratch::new("rename-uri");
    let file = scratch.write(b"file:///My%20Docs\nfile://localhost/x");
    assert!(rename(&file, Path::new("/My Docs"), "Docs").expect("renames"));
    assert!(rename(&file, Path::new("/x"), "X").expect("renames"));
    assert_eq!(
        read_text(&file),
        "file:///My%20Docs Docs\nfile://localhost/x X"
    );
}

#[test]
fn rename_that_changes_nothing_or_finds_nothing_leaves_the_file_alone() {
    let scratch = Scratch::new("rename-noop");
    let file = scratch.write(b"file:///a Alpha\nfile:///b\n");
    let before = std::fs::metadata(&file)
        .expect("metadata")
        .modified()
        .expect("mtime");
    assert!(!rename(&file, Path::new("/a"), "Alpha").expect("same label"));
    assert!(!rename(&file, Path::new("/b"), "").expect("already no label"));
    assert!(!rename(&file, Path::new("/missing"), "X").expect("unknown folder"));
    assert_eq!(read_text(&file), "file:///a Alpha\nfile:///b\n");
    assert_eq!(
        std::fs::metadata(&file)
            .expect("metadata")
            .modified()
            .expect("mtime"),
        before,
        "nothing was written"
    );
    let missing = scratch.0.join("none/bookmarks");
    assert!(!rename(&missing, Path::new("/a"), "X").expect("no file"));
    assert!(!missing.exists(), "nothing is created");
}

#[test]
fn rename_refuses_a_label_that_would_break_the_line_and_changes_nothing() {
    let scratch = Scratch::new("rename-bad");
    let file = scratch.write(b"file:///a\n");
    for label in ["two\nlines", "cr\rhere", "nul\0"] {
        let error = rename(&file, Path::new("/a"), label).expect_err("refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{label:?}");
    }
    assert_eq!(read_text(&file), "file:///a\n");
}

#[test]
fn rename_changes_only_the_first_line_of_a_repeated_folder() {
    let scratch = Scratch::new("rename-dup");
    let file = scratch.write(b"file:///a\nfile:///b\nfile:///a Second\n");
    assert!(rename(&file, Path::new("/a"), "First").expect("renames"));
    assert_eq!(
        read_text(&file),
        "file:///a First\nfile:///b\nfile:///a Second\n"
    );
}

const FIVE: &[u8] = b"file:///a\n# note\nfile:///b B\nsftp://h/p Srv\nfile:///c\n";

#[test]
fn reorder_moves_a_favorite_before_another_and_keeps_every_other_line_in_place() {
    let scratch = Scratch::new("reorder-before");
    let file = scratch.write(FIVE);
    assert!(reorder(&file, Path::new("/c"), Some(Path::new("/b"))).expect("moves"));
    assert_eq!(
        read_text(&file),
        "file:///a\n# note\nfile:///c\nfile:///b B\nsftp://h/p Srv\n"
    );
    assert!(reorder(&file, Path::new("/a"), Some(Path::new("/b"))).expect("moves"));
    assert_eq!(
        read_text(&file),
        "# note\nfile:///c\nfile:///a\nfile:///b B\nsftp://h/p Srv\n"
    );
}

#[test]
fn reorder_to_the_end_goes_after_the_last_favorite() {
    let scratch = Scratch::new("reorder-end");
    let file = scratch.write(FIVE);
    assert!(reorder(&file, Path::new("/a"), None).expect("moves"));
    assert_eq!(
        read_text(&file),
        "# note\nfile:///b B\nsftp://h/p Srv\nfile:///c\nfile:///a\n"
    );
}

#[test]
fn reorder_keeps_the_label_and_terminates_a_line_that_stops_being_last() {
    let scratch = Scratch::new("reorder-eol");
    let file = scratch.write(b"file:///a\r\nfile:///b Bee");
    assert!(reorder(&file, Path::new("/b"), Some(Path::new("/a"))).expect("moves"));
    assert_eq!(read_text(&file), "file:///b Bee\r\nfile:///a\r\n");
    assert!(reorder(&file, Path::new("/b"), None).expect("moves"));
    assert_eq!(read_text(&file), "file:///a\r\nfile:///b Bee\r\n");
}

#[test]
fn reorder_that_changes_nothing_leaves_the_file_alone() {
    let scratch = Scratch::new("reorder-noop");
    let file = scratch.write(FIVE);
    let before = std::fs::metadata(&file)
        .expect("metadata")
        .modified()
        .expect("mtime");
    assert!(!reorder(&file, Path::new("/a"), Some(Path::new("/a"))).expect("onto itself"));
    assert!(!reorder(&file, Path::new("/a"), Some(Path::new("/b"))).expect("already before"));
    assert!(!reorder(&file, Path::new("/c"), None).expect("already last"));
    assert!(!reorder(&file, Path::new("/zzz"), None).expect("unknown folder"));
    assert!(!reorder(&file, Path::new("/a"), Some(Path::new("/zzz"))).expect("unknown anchor"));
    assert_eq!(read_text(&file).as_bytes(), FIVE);
    assert_eq!(
        std::fs::metadata(&file)
            .expect("metadata")
            .modified()
            .expect("mtime"),
        before,
        "nothing was written"
    );
}
