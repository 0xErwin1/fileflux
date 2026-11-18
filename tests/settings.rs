//! The settings and session files: the TOML round trip, tolerant loading,
//! atomic saving, session restore and the command-line decision. Every test
//! works in a fresh directory under the system temp directory and never
//! touches the user's real config or state directories.

#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use fileflux::core::Location;
use fileflux::core::settings::{
    ArrangementPrefs, CategoryPref, DatePref, SESSION_VERSION, SETTINGS_VERSION, Session,
    SessionTab, Settings, SizePref, SortDirPref, SortKeyPref, ViewMode, launch_uses_session,
    load_session, load_settings, restore_session, save_session, save_settings, session_path,
    settings_path,
};

/// A scratch directory removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fileflux-settings-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(&dir).expect("scratch directory is created");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.0));
    }
}

fn now() -> SystemTime {
    // 2026-11-18T12:00:00Z
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_794_657_600)
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("directory reads")
        .map(|entry| {
            entry
                .expect("entry reads")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

fn busy_arrangement() -> ArrangementPrefs {
    ArrangementPrefs {
        sort_key: SortKeyPref::Modified,
        sort_dir: SortDirPref::Descending,
        folders_first: false,
        natural: false,
        hide_hidden: true,
        kinds: vec![CategoryPref::Images, CategoryPref::Code],
        modified: Some(DatePref::Last7Days),
        size: Some(SizePref::From1To100Mb),
    }
}

#[test]
fn settings_round_trip_keeps_every_preference_and_a_non_utf8_favorite() {
    let scratch = Scratch::new("roundtrip");
    let path = scratch.path().join("fileflux/settings.toml");
    let non_utf8 = PathBuf::from(OsString::from_vec(b"/data/caf\xe9 \xff".to_vec()));

    let mut settings = Settings::default();
    settings.preferences.view = ViewMode::List;
    settings.preferences.arrangement = busy_arrangement();
    settings.preferences.sidebar_width = 333;
    settings.preferences.sidebar_collapsed = true;
    settings.preferences.details_open = false;
    settings.preferences.window_width = 1111;
    settings.preferences.window_height = 777;
    settings.favorites = vec![
        PathBuf::from("/home/u/Projects"),
        non_utf8.clone(),
        PathBuf::from("/srv/with \"quotes\" and \\ backslash"),
    ];

    save_settings(&path, &settings).expect("settings save");
    let loaded = load_settings(&path, now());

    assert_eq!(loaded.notice, None);
    assert!(loaded.writable);
    assert_eq!(loaded.value, settings);
    assert_eq!(loaded.value.favorites[1], non_utf8);
}

#[test]
fn session_round_trip_keeps_tabs_views_arrangements_and_the_active_index() {
    let scratch = Scratch::new("session-roundtrip");
    let path = scratch.path().join("session.toml");
    let non_utf8 = PathBuf::from(OsString::from_vec(b"/tmp/\xfe\xfe".to_vec()));
    let session = Session {
        version: SESSION_VERSION,
        active: 2,
        tabs: vec![
            SessionTab::directory(
                PathBuf::from("/home/u"),
                ViewMode::Grid,
                ArrangementPrefs::default(),
            ),
            SessionTab::directory(non_utf8, ViewMode::List, busy_arrangement()),
            SessionTab::trash(ViewMode::List, ArrangementPrefs::default()),
        ],
    };

    save_session(&path, &session).expect("session saves");
    let loaded = load_session(&path, now());

    assert_eq!(loaded.notice, None);
    assert_eq!(loaded.value, session);
}

#[test]
fn a_missing_file_loads_defaults_quietly_and_stays_writable() {
    let scratch = Scratch::new("missing");
    let loaded = load_settings(&scratch.path().join("settings.toml"), now());
    assert_eq!(loaded.value, Settings::default());
    assert_eq!(loaded.notice, None);
    assert!(loaded.writable);
    let loaded = load_session(&scratch.path().join("session.toml"), now());
    assert_eq!(loaded.value, Session::default());
    assert_eq!(loaded.notice, None);
}

#[test]
fn missing_keys_take_their_defaults_and_unknown_keys_are_ignored() {
    let scratch = Scratch::new("tolerant");
    let path = scratch.path().join("settings.toml");
    std::fs::write(
        &path,
        "version = 1\n\
         future_top_level = \"ignored\"\n\
         [preferences]\n\
         default_view = \"list\"\n\
         surprise = 7\n\
         [preferences.arrangement]\n\
         hide_hidden = true\n\
         [elsewhere]\n\
         x = 1\n",
    )
    .expect("fixture writes");

    let loaded = load_settings(&path, now());

    assert_eq!(loaded.notice, None);
    assert!(loaded.writable);
    let defaults = Settings::default();
    assert_eq!(loaded.value.preferences.view, ViewMode::List);
    assert!(loaded.value.preferences.arrangement.hide_hidden);
    assert_eq!(
        loaded.value.preferences.arrangement.sort_key,
        defaults.preferences.arrangement.sort_key
    );
    assert!(loaded.value.preferences.arrangement.folders_first);
    assert_eq!(
        loaded.value.preferences.sidebar_width,
        defaults.preferences.sidebar_width
    );
    assert_eq!(loaded.value.favorites, defaults.favorites);
    assert_eq!(loaded.value.version, SETTINGS_VERSION);
}

#[test]
fn an_empty_file_is_all_defaults() {
    let scratch = Scratch::new("empty");
    let path = scratch.path().join("settings.toml");
    std::fs::write(&path, "").expect("fixture writes");
    let loaded = load_settings(&path, now());
    assert_eq!(loaded.value, Settings::default());
    assert_eq!(loaded.notice, None);
}

#[test]
fn a_newer_version_loads_defaults_with_a_notice_and_is_never_overwritten() {
    let scratch = Scratch::new("newer");
    let path = scratch.path().join("settings.toml");
    let original = format!(
        "version = {}\nfavorites = [\"/kept\"]\n[preferences]\nnew_shape = [1, 2]\n",
        SETTINGS_VERSION + 1
    );
    std::fs::write(&path, &original).expect("fixture writes");

    let loaded = load_settings(&path, now());

    assert_eq!(loaded.value, Settings::default());
    assert!(!loaded.writable, "a newer file must not be overwritten");
    let notice = loaded.notice.expect("a notice is produced");
    assert!(notice.contains("newer"), "{notice}");
    assert!(notice.contains("settings.toml"), "{notice}");
    assert_eq!(
        std::fs::read_to_string(&path).expect("file reads"),
        original
    );
    assert_eq!(entries(scratch.path()), ["settings.toml"], "no aside copy");
}

#[test]
fn a_corrupt_file_loads_defaults_and_is_kept_aside_before_any_write() {
    let scratch = Scratch::new("corrupt");
    let path = scratch.path().join("settings.toml");
    std::fs::write(&path, "version = [[[ this is not toml").expect("fixture writes");

    let loaded = load_settings(&path, now());

    assert_eq!(loaded.value, Settings::default());
    assert!(loaded.writable);
    let notice = loaded.notice.expect("a notice is produced");
    assert!(
        notice.starts_with("Settings file was invalid; using defaults (kept as settings.toml.bad-"),
        "{notice}"
    );
    let aside: Vec<String> = entries(scratch.path())
        .into_iter()
        .filter(|name| name.starts_with("settings.toml.bad-"))
        .collect();
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert!(notice.contains(&aside[0]), "{notice} names {aside:?}");
    assert_eq!(
        std::fs::read_to_string(scratch.path().join(&aside[0])).expect("aside reads"),
        "version = [[[ this is not toml"
    );

    // A later save replaces the original; the aside copy survives.
    save_settings(&path, &Settings::default()).expect("settings save");
    assert_eq!(
        std::fs::read_to_string(scratch.path().join(&aside[0])).expect("aside reads"),
        "version = [[[ this is not toml"
    );
    assert_eq!(load_settings(&path, now()).notice, None);
}

#[test]
fn wrongly_typed_values_count_as_corrupt_and_non_utf8_bytes_too() {
    let scratch = Scratch::new("types");
    let path = scratch.path().join("settings.toml");
    std::fs::write(
        &path,
        "version = 1\n[preferences]\nsidebar_width = \"wide\"\n",
    )
    .expect("fixture writes");
    let loaded = load_settings(&path, now());
    assert_eq!(loaded.value, Settings::default());
    assert!(loaded.notice.is_some());

    let session = scratch.path().join("session.toml");
    std::fs::write(&session, [0xff, 0xfe, 0x00]).expect("fixture writes");
    let loaded = load_session(&session, now());
    assert_eq!(loaded.value, Session::default());
    let notice = loaded.notice.expect("a notice is produced");
    assert!(notice.starts_with("Session file was invalid"), "{notice}");
    assert!(
        entries(scratch.path())
            .iter()
            .any(|name| name.starts_with("session.toml.bad-"))
    );
}

#[test]
fn saving_creates_directories_leaves_no_temporary_file_and_replaces_the_old_one() {
    let scratch = Scratch::new("atomic");
    let dir = scratch.path().join("a/b/fileflux");
    let path = dir.join("settings.toml");

    save_settings(&path, &Settings::default()).expect("first save");
    assert_eq!(entries(&dir), ["settings.toml"]);

    let mut changed = Settings::default();
    changed.favorites.push(PathBuf::from("/x"));
    save_settings(&path, &changed).expect("second save");
    assert_eq!(entries(&dir), ["settings.toml"]);
    assert_eq!(load_settings(&path, now()).value, changed);
}

#[test]
fn a_failed_save_reports_the_error_and_keeps_the_old_file() {
    let scratch = Scratch::new("failed-save");
    let path = scratch.path().join("settings.toml");
    save_settings(&path, &Settings::default()).expect("first save");
    let before = std::fs::read(&path).expect("file reads");

    // A directory where the file's parent should be makes the write fail.
    let blocked = path.join("nested/settings.toml");
    assert!(save_settings(&blocked, &Settings::default()).is_err());
    assert_eq!(std::fs::read(&path).expect("file reads"), before);
    assert_eq!(entries(scratch.path()), ["settings.toml"]);
}

#[test]
fn saved_files_are_private() {
    use std::os::unix::fs::PermissionsExt as _;
    let scratch = Scratch::new("mode");
    let path = scratch.path().join("settings.toml");
    save_settings(&path, &Settings::default()).expect("settings save");
    let mode = std::fs::metadata(&path)
        .expect("metadata reads")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn paths_follow_the_xdg_variables_and_fall_back_to_home() {
    let home = OsString::from("/home/u");
    let xdg = OsString::from("/xdg/config");
    assert_eq!(
        settings_path(Some(&home), Some(&xdg)),
        Some(PathBuf::from("/xdg/config/fileflux/settings.toml"))
    );
    assert_eq!(
        settings_path(Some(&home), None),
        Some(PathBuf::from("/home/u/.config/fileflux/settings.toml"))
    );
    assert_eq!(
        settings_path(Some(&home), Some(&OsString::from("relative"))),
        Some(PathBuf::from("/home/u/.config/fileflux/settings.toml"))
    );
    assert_eq!(
        settings_path(Some(&home), Some(&OsString::new())),
        Some(PathBuf::from("/home/u/.config/fileflux/settings.toml"))
    );
    assert_eq!(settings_path(None, None), None);
    assert_eq!(
        settings_path(None, Some(&xdg)),
        Some(PathBuf::from("/xdg/config/fileflux/settings.toml"))
    );
    let state = OsString::from("/xdg/state");
    assert_eq!(
        session_path(Some(&home), Some(&state)),
        Some(PathBuf::from("/xdg/state/fileflux/session.toml"))
    );
    assert_eq!(
        session_path(Some(&home), None),
        Some(PathBuf::from("/home/u/.local/state/fileflux/session.toml"))
    );
}

fn tab(path: &Path) -> SessionTab {
    SessionTab::directory(
        path.to_path_buf(),
        ViewMode::List,
        ArrangementPrefs::default(),
    )
}

#[test]
fn restore_keeps_existing_tabs_and_falls_back_to_the_nearest_existing_ancestor() {
    let session = Session {
        version: SESSION_VERSION,
        active: 1,
        tabs: vec![
            tab(Path::new("/fx/a")),
            tab(Path::new("/fx/gone/deeper/still")),
            SessionTab::trash(ViewMode::Grid, ArrangementPrefs::default()),
        ],
    };
    let existing = [Path::new("/"), Path::new("/fx"), Path::new("/fx/a")];
    let restored = restore_session(&session, Some(Path::new("/home/u")), |path| {
        existing.contains(&path)
    });

    assert_eq!(restored.active, 1);
    let locations: Vec<Location> = restored
        .tabs
        .iter()
        .map(|tab| tab.location.clone())
        .collect();
    assert_eq!(
        locations,
        [
            Location::Directory(PathBuf::from("/fx/a")),
            Location::Directory(PathBuf::from("/fx")),
            Location::Trash,
        ]
    );
    assert_eq!(restored.tabs[0].fell_back_from, None);
    assert_eq!(
        restored.tabs[1].fell_back_from,
        Some(PathBuf::from("/fx/gone/deeper/still"))
    );
    assert_eq!(restored.tabs[1].view, ViewMode::List);
}

#[test]
fn restore_uses_home_when_no_ancestor_exists_and_clamps_the_active_index() {
    let session = Session {
        version: SESSION_VERSION,
        active: 9,
        tabs: vec![
            tab(Path::new("/nowhere/at/all")),
            tab(Path::new("relative")),
        ],
    };
    let restored = restore_session(&session, Some(Path::new("/home/u")), |path| {
        path == Path::new("/home/u")
    });
    assert_eq!(restored.active, 1, "clamped to the last tab");
    assert!(restored.tabs.iter().all(|tab| {
        tab.location == Location::Directory(PathBuf::from("/home/u"))
            && tab.fell_back_from.is_some()
    }));

    let restored = restore_session(&session, None, |_| false);
    assert!(
        restored
            .tabs
            .iter()
            .all(|tab| { tab.location == Location::Directory(PathBuf::from("/")) })
    );
}

#[test]
fn an_empty_session_restores_nothing() {
    let restored = restore_session(&Session::default(), None, |_| true);
    assert!(restored.tabs.is_empty());
    assert_eq!(restored.active, 0);
}

#[test]
fn a_command_line_path_wins_over_the_session() {
    assert!(launch_uses_session(None));
    assert!(!launch_uses_session(Some(Path::new("/some/dir"))));
}

#[test]
fn preferences_are_clamped_to_sane_values() {
    let mut settings = Settings::default();
    settings.preferences.sidebar_width = 5;
    settings.preferences.window_width = 0;
    settings.preferences.window_height = u32::MAX;
    let prefs = settings.preferences.sanitized();
    assert!(prefs.sidebar_width >= 200);
    assert!(prefs.window_width >= 480);
    assert!(prefs.window_height <= 16_384);
}
