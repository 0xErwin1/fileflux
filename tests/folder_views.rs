//! The per-folder view memory: the bounded LRU list, the file round trip and
//! version, a corrupt file, and which presentation a location takes. Every
//! test works in a fresh directory under the system temp directory and never
//! touches the user's real state directory.

#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use fileflux::core::Location;
use fileflux::core::folder_views::{
    FOLDERS_VERSION, FolderView, FolderViews, MAX_FOLDERS, folders_path, load_folders,
    presentation_for, save_folders_ordered,
};
use fileflux::core::settings::{ArrangementPrefs, SortKeyPref, ViewMode};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fileflux-folders-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(&dir).expect("scratch directory is created");
        Self(dir)
    }

    fn file(&self) -> PathBuf {
        self.0.join("fileflux").join("folders.toml")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.0));
    }
}

fn list() -> FolderView {
    FolderView {
        view: ViewMode::List,
        arrangement: ArrangementPrefs {
            sort_key: SortKeyPref::Size,
            hide_hidden: false,
            ..ArrangementPrefs::default()
        },
    }
}

fn compact() -> FolderView {
    FolderView {
        view: ViewMode::Compact,
        ..FolderView::default()
    }
}

fn folder(index: usize) -> PathBuf {
    PathBuf::from(format!("/f/{index}"))
}

#[test]
fn recording_remembers_and_a_second_record_replaces_without_duplicating() {
    let mut views = FolderViews::default();
    assert!(views.is_empty());
    assert_eq!(views.get(Path::new("/a")), None);
    views.record(Path::new("/a"), &list());
    assert_eq!(views.get(Path::new("/a")), Some(list()));
    views.record(Path::new("/a"), &compact());
    assert_eq!(views.get(Path::new("/a")), Some(compact()));
    assert_eq!(views.len(), 1);
    views.record(Path::new("relative"), &list());
    assert_eq!(views.len(), 1, "a relative path names no folder");
}

#[test]
fn the_least_recently_used_folder_is_forgotten_at_the_cap() {
    let mut views = FolderViews::default();
    for index in 0..MAX_FOLDERS {
        views.record(&folder(index), &list());
    }
    assert_eq!(views.len(), MAX_FOLDERS);
    // Touching the oldest makes the next oldest the one to go.
    views.record(&folder(0), &compact());
    views.record(&folder(MAX_FOLDERS), &list());
    assert_eq!(views.len(), MAX_FOLDERS);
    assert_eq!(views.get(&folder(1)), None, "the least recent went");
    assert_eq!(views.get(&folder(0)), Some(compact()), "the touched stayed");
    assert_eq!(views.get(&folder(MAX_FOLDERS)), Some(list()));
    assert_eq!(views.get(&folder(2)), Some(list()));
}

#[test]
fn getting_does_not_count_as_a_use() {
    let mut views = FolderViews::default();
    for index in 0..MAX_FOLDERS {
        views.record(&folder(index), &list());
    }
    assert!(views.get(&folder(0)).is_some());
    views.record(&folder(MAX_FOLDERS), &list());
    assert_eq!(views.get(&folder(0)), None);
}

#[test]
fn forgetting_drops_one_folder_and_says_whether_it_was_there() {
    let mut views = FolderViews::default();
    views.record(Path::new("/a"), &list());
    views.record(Path::new("/b"), &list());
    assert!(views.forget(Path::new("/a")));
    assert!(!views.forget(Path::new("/a")));
    assert_eq!(views.get(Path::new("/a")), None);
    assert_eq!(views.get(Path::new("/b")), Some(list()));
}

#[test]
fn the_file_round_trips_in_order_with_its_version() {
    let scratch = Scratch::new("round-trip");
    let mut views = FolderViews::default();
    views.record(Path::new("/a"), &list());
    views.record(Path::new("/b"), &compact());
    views.record(Path::new("/a"), &compact());
    save_folders_ordered(&scratch.file(), &views, 1).expect("saved");
    let text = std::fs::read_to_string(scratch.file()).expect("file exists");
    assert!(
        text.contains(&format!("version = {FOLDERS_VERSION}")),
        "{text}"
    );
    let loaded = load_folders(&scratch.file(), SystemTime::now());
    assert_eq!(loaded.notice, None);
    assert!(loaded.writable);
    assert_eq!(loaded.value, views);
    let paths: Vec<&Path> = loaded
        .value
        .folders
        .iter()
        .map(|record| record.path.as_path())
        .collect();
    assert_eq!(
        paths,
        [Path::new("/b"), Path::new("/a")],
        "least recent first"
    );
}

#[test]
fn a_non_utf8_path_round_trips() {
    use std::os::unix::ffi::OsStringExt as _;
    let scratch = Scratch::new("bytes");
    let odd = PathBuf::from(std::ffi::OsString::from_vec(b"/a/\xff\xfe".to_vec()));
    let mut views = FolderViews::default();
    views.record(&odd, &list());
    save_folders_ordered(&scratch.file(), &views, 1).expect("saved");
    let loaded = load_folders(&scratch.file(), SystemTime::now());
    assert_eq!(loaded.value.get(&odd), Some(list()));
}

#[test]
fn a_missing_file_loads_empty_without_a_notice() {
    let scratch = Scratch::new("missing");
    let loaded = load_folders(&scratch.file(), SystemTime::now());
    assert!(loaded.value.is_empty());
    assert_eq!(
        (loaded.notice, loaded.writable, loaded.heal),
        (None, true, false)
    );
}

#[test]
fn an_oversized_or_repeating_file_is_bounded_on_load() {
    let scratch = Scratch::new("bounded");
    let mut text = format!("version = {FOLDERS_VERSION}\n");
    for index in 0..MAX_FOLDERS + 5 {
        text.push_str(&format!(
            "[[folders]]\npath = \"/f/{index}\"\nview = \"list\"\n"
        ));
    }
    text.push_str("[[folders]]\npath = \"/f/7\"\nview = \"compact\"\n");
    text.push_str("[[folders]]\npath = \"relative\"\nview = \"list\"\n");
    std::fs::create_dir_all(scratch.file().parent().expect("parent")).expect("dir");
    std::fs::write(scratch.file(), text).expect("written");
    let loaded = load_folders(&scratch.file(), SystemTime::now());
    assert_eq!(loaded.value.len(), MAX_FOLDERS);
    assert_eq!(loaded.value.get(Path::new("relative")), None);
    assert_eq!(
        loaded.value.get(Path::new("/f/7")).map(|view| view.view),
        Some(ViewMode::Compact),
        "the later duplicate wins"
    );
    assert_eq!(loaded.value.get(&folder(0)), None, "the oldest went");
}

#[test]
fn a_corrupt_file_is_kept_aside_once_and_healed_by_the_next_save() {
    let scratch = Scratch::new("corrupt");
    let file = scratch.file();
    std::fs::create_dir_all(file.parent().expect("parent")).expect("dir");
    std::fs::write(&file, "this is [not toml").expect("written");
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let loaded = load_folders(&file, now);
    assert!(loaded.value.is_empty());
    assert!(loaded.heal && loaded.writable);
    assert_eq!(
        loaded.notice.as_deref(),
        Some(
            "Folder views file was invalid; using defaults \
             (kept as folders.toml.bad-20231114T221320Z)"
        )
    );
    let aside = file.with_file_name("folders.toml.bad-20231114T221320Z");
    assert_eq!(
        std::fs::read_to_string(aside).expect("the copy exists"),
        "this is [not toml"
    );
    save_folders_ordered(&file, &loaded.value, 1).expect("healed");
    assert_eq!(load_folders(&file, now).notice, None);
}

#[test]
fn a_newer_file_is_not_touched_and_not_writable() {
    let scratch = Scratch::new("newer");
    let file = scratch.file();
    std::fs::create_dir_all(file.parent().expect("parent")).expect("dir");
    let text = format!("version = {}\n", FOLDERS_VERSION + 1);
    std::fs::write(&file, &text).expect("written");
    let loaded = load_folders(&file, SystemTime::now());
    assert!(loaded.value.is_empty() && !loaded.writable);
    assert!(loaded.notice.is_some());
    assert_eq!(std::fs::read_to_string(&file).expect("kept"), text);
}

#[test]
fn an_older_save_never_overwrites_a_newer_one() {
    let scratch = Scratch::new("order");
    let mut views = FolderViews::default();
    views.record(Path::new("/new"), &list());
    save_folders_ordered(&scratch.file(), &views, 5).expect("saved");
    let older = FolderViews::default();
    save_folders_ordered(&scratch.file(), &older, 4).expect("skipped");
    let loaded = load_folders(&scratch.file(), SystemTime::now());
    assert_eq!(loaded.value, views);
}

#[test]
fn the_file_sits_in_the_state_directory() {
    use std::ffi::OsStr;
    assert_eq!(
        folders_path(Some(OsStr::new("/home/u")), Some(OsStr::new("/state"))),
        Some(PathBuf::from("/state/fileflux/folders.toml"))
    );
    assert_eq!(
        folders_path(Some(OsStr::new("/home/u")), None),
        Some(PathBuf::from("/home/u/.local/state/fileflux/folders.toml"))
    );
    assert_eq!(folders_path(None, None), None);
}

#[test]
fn a_folder_takes_its_memory_and_every_other_location_keeps_the_tab() {
    let mut memory = FolderViews::default();
    memory.record(Path::new("/known"), &list());
    let current = FolderView {
        view: ViewMode::Grid,
        arrangement: ArrangementPrefs {
            sort_key: SortKeyPref::Type,
            ..ArrangementPrefs::default()
        },
    };
    let pick = |location: &Location| presentation_for(location, &memory, &current);
    assert_eq!(pick(&Location::Directory("/known".into())), list());
    assert_eq!(
        pick(&Location::Directory("/other".into())),
        current,
        "no memory: today's behaviour, the last choice carries over"
    );
    assert_eq!(pick(&Location::Trash), current);
    assert_eq!(pick(&Location::Recent), current);
    assert_eq!(
        pick(&Location::Search {
            root: "/known".into(),
            query: "x".into()
        }),
        current,
        "a search of a remembered folder is not that folder"
    );
}
