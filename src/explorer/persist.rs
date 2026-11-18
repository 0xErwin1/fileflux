//! Keeping the explorer between runs: what the settings and session files
//! hold, how the live explorer maps to them, and when they are written.
//!
//! The files themselves (format, tolerant loading, atomic saving) live in
//! [`fileflux::core::settings`]. This module is the GPUI-free glue:
//!
//! - [`Startup::load`] reads both files before the window opens;
//! - the explorer takes a snapshot of its persisted state on every pump tick
//!   and hands it to [`Persistence::observe`], which waits for the state to
//!   stop changing for [`DEBOUNCE`] and then returns a [`SaveJob`] to run off
//!   the UI thread. Watching the snapshot instead of hooking every change
//!   means a new way to change a preference is saved without extra wiring;
//! - [`Persistence::flush`] saves what is pending when the window closes.
//!
//! What counts as "the default": the **active tab's** view, sort and filters
//! are the preferences saved as the default for new tabs and the next launch,
//! because a new tab already starts from the tab it was opened from.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use fileflux::core::Location;
use fileflux::core::kinds::FileCategory;
use fileflux::core::settings::{
    self, ArrangementPrefs, CategoryPref, DatePref, Preferences, Restored, Session, Settings,
    SizePref, SortDirPref, SortKeyPref, ViewMode,
};

use super::arrange::{Arrangement, DateRange, Filters, SizeRange, SortDir, SortKey, SortSpec};
use super::state::View;

/// How long the persisted state must stay unchanged before it is written, so
/// a drag or a burst of clicks is one write.
pub(crate) const DEBOUNCE: Duration = Duration::from_millis(500);

// --- mapping to the stored types ------------------------------------------------

pub(crate) fn view_from_pref(view: ViewMode) -> View {
    match view {
        ViewMode::Grid => View::Grid,
        ViewMode::List => View::List,
    }
}

pub(crate) fn view_to_pref(view: View) -> ViewMode {
    match view {
        View::Grid => ViewMode::Grid,
        View::List => ViewMode::List,
    }
}

fn category_from_pref(category: CategoryPref) -> FileCategory {
    match category {
        CategoryPref::Folders => FileCategory::Folders,
        CategoryPref::Images => FileCategory::Images,
        CategoryPref::Videos => FileCategory::Videos,
        CategoryPref::Audio => FileCategory::Audio,
        CategoryPref::Documents => FileCategory::Documents,
        CategoryPref::Archives => FileCategory::Archives,
        CategoryPref::Code => FileCategory::Code,
        CategoryPref::Other => FileCategory::Other,
    }
}

fn category_to_pref(category: FileCategory) -> CategoryPref {
    match category {
        FileCategory::Folders => CategoryPref::Folders,
        FileCategory::Images => CategoryPref::Images,
        FileCategory::Videos => CategoryPref::Videos,
        FileCategory::Audio => CategoryPref::Audio,
        FileCategory::Documents => CategoryPref::Documents,
        FileCategory::Archives => CategoryPref::Archives,
        FileCategory::Code => CategoryPref::Code,
        FileCategory::Other => CategoryPref::Other,
    }
}

pub(crate) fn arrangement_from_prefs(prefs: &ArrangementPrefs) -> Arrangement {
    Arrangement {
        sort: SortSpec {
            key: match prefs.sort_key {
                SortKeyPref::Name => SortKey::Name,
                SortKeyPref::Size => SortKey::Size,
                SortKeyPref::Modified => SortKey::Modified,
                SortKeyPref::Type => SortKey::Type,
            },
            dir: match prefs.sort_dir {
                SortDirPref::Ascending => SortDir::Ascending,
                SortDirPref::Descending => SortDir::Descending,
            },
            folders_first: prefs.folders_first,
            natural: prefs.natural,
        },
        filters: Filters {
            kinds: prefs
                .kinds
                .iter()
                .copied()
                .map(category_from_pref)
                .collect(),
            hide_hidden: prefs.hide_hidden,
            modified: prefs.modified.map(|range| match range {
                DatePref::Today => DateRange::Today,
                DatePref::Last7Days => DateRange::Last7Days,
                DatePref::Last30Days => DateRange::Last30Days,
                DatePref::ThisYear => DateRange::ThisYear,
                DatePref::Older => DateRange::Older,
            }),
            size: prefs.size.map(|range| match range {
                SizePref::Under1Mb => SizeRange::Under1Mb,
                SizePref::From1To100Mb => SizeRange::From1To100Mb,
                SizePref::From100MbTo1Gb => SizeRange::From100MbTo1Gb,
                SizePref::Over1Gb => SizeRange::Over1Gb,
            }),
        },
    }
}

pub(crate) fn arrangement_to_prefs(arrangement: &Arrangement) -> ArrangementPrefs {
    ArrangementPrefs {
        sort_key: match arrangement.sort.key {
            SortKey::Name => SortKeyPref::Name,
            SortKey::Size => SortKeyPref::Size,
            SortKey::Modified => SortKeyPref::Modified,
            SortKey::Type => SortKeyPref::Type,
        },
        sort_dir: match arrangement.sort.dir {
            SortDir::Ascending => SortDirPref::Ascending,
            SortDir::Descending => SortDirPref::Descending,
        },
        folders_first: arrangement.sort.folders_first,
        natural: arrangement.sort.natural,
        hide_hidden: arrangement.filters.hide_hidden,
        kinds: arrangement
            .filters
            .kinds
            .iter()
            .copied()
            .map(category_to_pref)
            .collect(),
        modified: arrangement.filters.modified.map(|range| match range {
            DateRange::Today => DatePref::Today,
            DateRange::Last7Days => DatePref::Last7Days,
            DateRange::Last30Days => DatePref::Last30Days,
            DateRange::ThisYear => DatePref::ThisYear,
            DateRange::Older => DatePref::Older,
        }),
        size: arrangement.filters.size.map(|range| match range {
            SizeRange::Under1Mb => SizePref::Under1Mb,
            SizeRange::From1To100Mb => SizePref::From1To100Mb,
            SizeRange::From100MbTo1Gb => SizePref::From100MbTo1Gb,
            SizeRange::Over1Gb => SizePref::Over1Gb,
        }),
    }
}

/// A session tab for `location`; the Trash is stored as such.
pub(crate) fn session_tab(
    location: &Location,
    view: View,
    arrangement: &Arrangement,
) -> settings::SessionTab {
    let view = view_to_pref(view);
    let arrangement = arrangement_to_prefs(arrangement);
    match location {
        Location::Directory(path) => {
            settings::SessionTab::directory(path.clone(), view, arrangement)
        }
        Location::Trash => settings::SessionTab::trash(view, arrangement),
    }
}

// --- saving ---------------------------------------------------------------------

/// What one save writes. A file that must not be written (see
/// [`Persistence`]) is absent.
#[derive(Debug)]
pub(crate) struct SaveJob {
    settings: Option<(PathBuf, Settings)>,
    session: Option<(PathBuf, Session)>,
}

impl SaveJob {
    /// Writes the files; returns one message per failure. Blocking: run it
    /// off the UI thread, except on close.
    pub(crate) fn run(self) -> Vec<String> {
        let mut errors = Vec::new();
        if let Some((path, settings)) = &self.settings
            && let Err(error) = settings::save_settings(path, settings)
        {
            errors.push(format!("{}: {error}", path.display()));
        }
        if let Some((path, session)) = &self.session
            && let Err(error) = settings::save_session(path, session)
        {
            errors.push(format!("{}: {error}", path.display()));
        }
        errors
    }
}

/// When and whether the settings and session files are written.
#[derive(Debug)]
pub(crate) struct Persistence {
    /// `None`: never written (no usable directory, or written by a newer
    /// Fileflux).
    settings_path: Option<PathBuf>,
    /// `None`: never written — as above, or this launch has a command-line
    /// path and leaves the saved session alone.
    session_path: Option<PathBuf>,
    /// The content last written or queued, what a change is measured
    /// against.
    saved_settings: Settings,
    saved_session: Session,
    /// The last snapshot that differed from the saved one, and when it was
    /// first seen: the debounce window.
    pending: Option<(Settings, Session, Instant)>,
    /// A background save is running.
    in_flight: bool,
    /// A failed save was already reported; later failures stay in the log.
    failure_reported: bool,
}

impl Persistence {
    /// Nothing is ever written; for tests and a missing home.
    #[cfg(test)]
    fn disabled() -> Self {
        Self::new(None, None, Settings::default(), Session::default())
    }

    fn new(
        settings_path: Option<PathBuf>,
        session_path: Option<PathBuf>,
        saved_settings: Settings,
        saved_session: Session,
    ) -> Self {
        Self {
            settings_path,
            session_path,
            saved_settings,
            saved_session,
            pending: None,
            in_flight: false,
            failure_reported: false,
        }
    }

    fn dirty(&self, settings: &Settings, session: &Session) -> bool {
        (self.settings_path.is_some() && *settings != self.saved_settings)
            || (self.session_path.is_some() && *session != self.saved_session)
    }

    fn take_job(&mut self, settings: Settings, session: Session) -> SaveJob {
        let job = SaveJob {
            settings: self
                .settings_path
                .clone()
                .filter(|_| settings != self.saved_settings)
                .map(|path| (path, settings.clone())),
            session: self
                .session_path
                .clone()
                .filter(|_| session != self.saved_session)
                .map(|path| (path, session.clone())),
        };
        // Measured against what was queued: a failed save is retried on the
        // next change, not in a loop.
        if job.settings.is_some() {
            self.saved_settings = settings;
        }
        if job.session.is_some() {
            self.saved_session = session;
        }
        job
    }

    /// Looks at the current persisted state at `now`. Returns the save to run
    /// once the state differs from what is saved and has not changed for
    /// [`DEBOUNCE`]; the caller must report its end with [`Self::finished`].
    pub(crate) fn observe(
        &mut self,
        settings: Settings,
        session: Session,
        now: Instant,
    ) -> Option<SaveJob> {
        if !self.dirty(&settings, &session) {
            self.pending = None;
            return None;
        }
        let since = match &self.pending {
            Some((seen_settings, seen_session, since))
                if *seen_settings == settings && *seen_session == session =>
            {
                *since
            }
            _ => {
                self.pending = Some((settings, session, now));
                return None;
            }
        };
        if self.in_flight || now.duration_since(since) < DEBOUNCE {
            return None;
        }
        self.pending = None;
        self.in_flight = true;
        Some(self.take_job(settings, session))
    }

    /// The save for the window closing: whatever differs from the saved
    /// files, without waiting. Run it on the spot — the process ends next.
    pub(crate) fn flush(&mut self, settings: Settings, session: Session) -> Option<SaveJob> {
        self.pending = None;
        self.dirty(&settings, &session)
            .then(|| self.take_job(settings, session))
    }

    /// A background save ended with these errors. Returns the status notice
    /// for the **first** failure only; the explorer never blocks on a save.
    pub(crate) fn finished(&mut self, errors: &[String]) -> Option<String> {
        self.in_flight = false;
        let first = errors.first()?;
        log::warn!("settings were not saved: {}", errors.join("; "));
        if std::mem::replace(&mut self.failure_reported, true) {
            return None;
        }
        Some(format!("Could not save settings ({first})"))
    }
}

// --- loading --------------------------------------------------------------------

/// Everything read before the window opens.
#[derive(Debug)]
pub(crate) struct Startup {
    /// The preferences, clamped to sane values.
    pub(crate) preferences: Preferences,
    pub(crate) favorites: Vec<PathBuf>,
    /// The tabs to reopen; `None` opens the command-line path (or home).
    pub(crate) restored: Option<Restored>,
    pub(crate) persistence: Persistence,
    /// Load problems for the status bar, in the order they happened.
    pub(crate) notices: Vec<String>,
}

impl Startup {
    /// Reads the files the environment points at. `start` is the
    /// command-line folder, which wins over the saved session.
    pub(crate) fn load(start: Option<&Path>) -> Self {
        let home = std::env::var_os("HOME").filter(|value| !value.is_empty());
        let settings_path = settings::settings_path(
            home.as_deref(),
            std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        );
        let session_path = settings::session_path(
            home.as_deref(),
            std::env::var_os("XDG_STATE_HOME").as_deref(),
        );
        Self::load_from(
            settings_path,
            session_path,
            start,
            home.as_deref().map(Path::new),
            SystemTime::now(),
            Path::is_dir,
        )
    }

    /// [`Self::load`] against explicit files, clock and folder check.
    pub(crate) fn load_from(
        settings_path: Option<PathBuf>,
        session_path: Option<PathBuf>,
        start: Option<&Path>,
        home: Option<&Path>,
        now: SystemTime,
        is_dir: impl Fn(&Path) -> bool,
    ) -> Self {
        let mut notices = Vec::new();

        let (settings, settings_writable) = match &settings_path {
            Some(path) => {
                let loaded = settings::load_settings(path, now);
                notices.extend(loaded.notice);
                (loaded.value, loaded.writable)
            }
            None => (Settings::default(), false),
        };

        // A command-line path means "just this folder": the session is
        // neither read nor restored nor overwritten.
        let uses_session = settings::launch_uses_session(start);
        let (session, session_writable) = match &session_path {
            Some(path) if uses_session => {
                let loaded = settings::load_session(path, now);
                notices.extend(loaded.notice);
                (loaded.value, loaded.writable)
            }
            _ => (Session::default(), false),
        };

        let restored = (uses_session && !session.tabs.is_empty())
            .then(|| settings::restore_session(&session, home, is_dir));
        if let Some(restored) = &restored {
            let moved = restored
                .tabs
                .iter()
                .filter(|tab| tab.fell_back_from.is_some())
                .count();
            if moved > 0 {
                notices.push(format!(
                    "{moved} restored tab{} pointed at a folder that no longer exists; \
                     opened the nearest existing parent instead",
                    if moved == 1 { "" } else { "s" }
                ));
            }
        }

        Self {
            preferences: settings.preferences.clone().sanitized(),
            favorites: settings.favorites.clone(),
            restored,
            persistence: Persistence::new(
                settings_path.filter(|_| settings_writable),
                session_path.filter(|_| session_writable && uses_session),
                settings,
                session,
            ),
            notices,
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use fileflux::core::settings::SessionTab;

    #[test]
    fn every_arrangement_survives_the_stored_form() {
        let mut arrangement = Arrangement::default();
        arrangement.sort.key = SortKey::Type;
        arrangement.sort.dir = SortDir::Descending;
        arrangement.sort.folders_first = false;
        arrangement.sort.natural = false;
        arrangement.filters.hide_hidden = true;
        arrangement.filters.kinds = FileCategory::ALL.into_iter().collect();
        for range in DateRange::ALL {
            arrangement.filters.modified = Some(range);
            for size in SizeRange::ALL {
                arrangement.filters.size = Some(size);
                assert_eq!(
                    arrangement_from_prefs(&arrangement_to_prefs(&arrangement)),
                    arrangement
                );
            }
        }
        for key in SortKey::ALL {
            arrangement.sort.key = key;
            assert_eq!(
                arrangement_from_prefs(&arrangement_to_prefs(&arrangement)),
                arrangement
            );
        }
        assert_eq!(
            arrangement_from_prefs(&ArrangementPrefs::default()),
            Arrangement::default(),
            "the stored default is the explorer's default"
        );
        assert_eq!(
            arrangement_to_prefs(&Arrangement::default()),
            ArrangementPrefs::default()
        );
    }

    #[test]
    fn views_and_the_trash_survive_the_stored_form() {
        for view in [View::Grid, View::List] {
            assert_eq!(view_from_pref(view_to_pref(view)), view);
        }
        let tab = session_tab(&Location::Trash, View::List, &Arrangement::default());
        assert!(tab.trash && tab.path.is_none());
        let tab = session_tab(
            &Location::Directory(PathBuf::from("/a")),
            View::Grid,
            &Arrangement::default(),
        );
        assert_eq!(tab.path.as_deref(), Some(Path::new("/a")));
    }

    fn settings_with(favorite: &str) -> Settings {
        Settings {
            favorites: vec![PathBuf::from(favorite)],
            ..Settings::default()
        }
    }

    fn persistence(dir: &Path) -> Persistence {
        Persistence::new(
            Some(dir.join("settings.toml")),
            Some(dir.join("session.toml")),
            Settings::default(),
            Session::default(),
        )
    }

    #[test]
    fn an_unchanged_state_is_never_written() {
        let mut persistence = persistence(Path::new("/unused"));
        let start = Instant::now();
        for step in 0..10 {
            let now = start + DEBOUNCE * step;
            assert!(
                persistence
                    .observe(Settings::default(), Session::default(), now)
                    .is_none()
            );
        }
    }

    #[test]
    fn a_change_is_written_once_it_stops_changing_for_the_debounce() {
        let mut persistence = persistence(Path::new("/unused"));
        let start = Instant::now();
        let observe = |persistence: &mut Persistence, favorite: &str, at: Duration| {
            persistence.observe(settings_with(favorite), Session::default(), start + at)
        };
        assert!(observe(&mut persistence, "/a", Duration::ZERO).is_none());
        // Still changing: the window restarts.
        assert!(observe(&mut persistence, "/b", Duration::from_millis(400)).is_none());
        assert!(observe(&mut persistence, "/b", Duration::from_millis(800)).is_none());
        let job = observe(&mut persistence, "/b", Duration::from_millis(900))
            .expect("saved 500 ms after the last change");
        assert_eq!(
            job.settings
                .as_ref()
                .map(|(_, settings)| settings.favorites.clone()),
            Some(vec![PathBuf::from("/b")])
        );
        assert!(
            job.session.is_none(),
            "an unchanged session is not rewritten"
        );
        // While the save runs and after it, the same state is not saved again.
        assert!(observe(&mut persistence, "/b", Duration::from_millis(2_000)).is_none());
        assert_eq!(persistence.finished(&[]), None);
        assert!(observe(&mut persistence, "/b", Duration::from_millis(3_000)).is_none());
    }

    #[test]
    fn files_that_must_not_be_written_are_left_out_of_the_job() {
        let mut persistence = Persistence::new(
            Some(PathBuf::from("/unused/settings.toml")),
            None,
            Settings::default(),
            Session::default(),
        );
        let session = Session {
            active: 1,
            tabs: vec![SessionTab::default(), SessionTab::default()],
            ..Session::default()
        };
        let start = Instant::now();
        assert!(
            persistence
                .observe(Settings::default(), session.clone(), start)
                .is_none()
        );
        assert!(
            persistence
                .observe(Settings::default(), session.clone(), start + DEBOUNCE)
                .is_none(),
            "a session change alone writes nothing without a session path"
        );
        assert!(persistence.flush(Settings::default(), session).is_none());
        assert!(
            Persistence::disabled()
                .flush(settings_with("/a"), Session::default())
                .is_none()
        );
    }

    #[test]
    fn close_flushes_pending_changes_without_waiting() {
        let mut persistence = persistence(Path::new("/unused"));
        let job = persistence
            .flush(settings_with("/a"), Session::default())
            .expect("a pending change is flushed");
        assert!(job.settings.is_some());
        assert!(
            persistence
                .flush(settings_with("/a"), Session::default())
                .is_none(),
            "nothing left to flush"
        );
    }

    #[test]
    fn only_the_first_failed_save_is_reported() {
        let mut persistence = persistence(Path::new("/unused"));
        let notice = persistence.finished(&["/x/settings.toml: denied".to_owned()]);
        assert_eq!(
            notice.as_deref(),
            Some("Could not save settings (/x/settings.toml: denied)")
        );
        assert_eq!(
            persistence.finished(&["/x/settings.toml: denied".to_owned()]),
            None
        );
        assert_eq!(persistence.finished(&[]), None);
    }

    /// A scratch directory under the system temp directory.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "fileflux-persist-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            drop(std::fs::remove_dir_all(&dir));
            std::fs::create_dir_all(&dir).expect("scratch is created");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.0));
        }
    }

    fn saved_session(dir: &Path) -> PathBuf {
        let path = dir.join("state/session.toml");
        let session = Session {
            active: 1,
            tabs: vec![
                SessionTab::directory(
                    PathBuf::from("/fx/a"),
                    ViewMode::List,
                    ArrangementPrefs::default(),
                ),
                SessionTab::directory(
                    PathBuf::from("/fx/gone"),
                    ViewMode::Grid,
                    ArrangementPrefs::default(),
                ),
            ],
            ..Session::default()
        };
        settings::save_session(&path, &session).expect("session saves");
        path
    }

    fn load(dir: &Path, start: Option<&Path>) -> Startup {
        Startup::load_from(
            Some(dir.join("config/settings.toml")),
            Some(dir.join("state/session.toml")),
            start,
            Some(Path::new("/home/u")),
            SystemTime::UNIX_EPOCH,
            |path| path == Path::new("/") || path == Path::new("/fx/a"),
        )
    }

    #[test]
    fn startup_restores_the_session_when_no_path_was_given() {
        let scratch = Scratch::new("restore");
        saved_session(&scratch.0);
        let startup = load(&scratch.0, None);
        let restored = startup.restored.expect("the session is restored");
        assert_eq!(restored.tabs.len(), 2);
        assert_eq!(restored.active, 1);
        assert_eq!(
            restored.tabs[1].location,
            Location::Directory(PathBuf::from("/"))
        );
        assert_eq!(startup.notices.len(), 1, "{:?}", startup.notices);
        assert!(startup.notices[0].contains("no longer exists"));
        assert!(startup.persistence.session_path.is_some());
    }

    #[test]
    fn a_command_line_path_ignores_and_never_overwrites_the_session() {
        let scratch = Scratch::new("cli");
        let session = saved_session(&scratch.0);
        let before = std::fs::read(&session).expect("session reads");
        let mut startup = load(&scratch.0, Some(Path::new("/fx/a")));
        assert!(startup.restored.is_none());
        assert!(startup.notices.is_empty());
        assert!(startup.persistence.session_path.is_none());

        // Even a changed session and a flush leave the file alone.
        let changed = Session {
            active: 0,
            tabs: vec![SessionTab::default()],
            ..Session::default()
        };
        assert!(
            startup
                .persistence
                .flush(Settings::default(), changed)
                .is_none()
        );
        assert_eq!(std::fs::read(&session).expect("session reads"), before);
    }

    #[test]
    fn a_session_from_a_newer_version_is_never_written_over() {
        let scratch = Scratch::new("newer-session");
        let path = scratch.0.join("state/session.toml");
        std::fs::create_dir_all(path.parent().expect("has a parent")).expect("dir is created");
        std::fs::write(&path, "version = 99\n").expect("fixture writes");
        let mut startup = load(&scratch.0, None);
        assert!(startup.restored.is_none());
        assert_eq!(startup.notices.len(), 1);
        assert!(startup.notices[0].contains("newer"));
        assert!(startup.persistence.session_path.is_none());
        let session = Session {
            tabs: vec![SessionTab::default()],
            ..Session::default()
        };
        assert!(
            startup
                .persistence
                .flush(Settings::default(), session)
                .is_none()
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("file reads"),
            "version = 99\n"
        );
    }

    #[test]
    fn startup_clamps_hand_edited_preferences() {
        let scratch = Scratch::new("clamp");
        let path = scratch.0.join("config/settings.toml");
        std::fs::create_dir_all(path.parent().expect("has a parent")).expect("dir is created");
        std::fs::write(
            &path,
            "[preferences]\nsidebar_width = 1\nwindow_width = 1\n",
        )
        .expect("fixture writes");
        let startup = load(&scratch.0, None);
        assert_eq!(startup.preferences.sidebar_width, 200);
        assert_eq!(startup.preferences.window_width, 480);
        assert!(startup.notices.is_empty());
    }
}
