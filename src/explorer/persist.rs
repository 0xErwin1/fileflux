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
use fileflux::core::bookmarks::{self, Bookmark};
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

/// How long a failed save waits before the background retries it, so a
/// read-only directory is not hit twice a second. The close flush never waits.
pub(crate) const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(30);

/// What one save writes. A file that must not be written (see
/// [`Persistence`]) is absent. Every job carries a generation: a job older
/// than the newest one written is skipped under the write lock, so a slow
/// background save never overwrites what the close flush wrote after it.
#[derive(Debug)]
pub(crate) struct SaveJob {
    generation: u64,
    settings: Option<SettingsWrite>,
    session: Option<(PathBuf, Session)>,
}

#[derive(Debug)]
struct SettingsWrite {
    path: PathBuf,
    value: Settings,
    /// What the file held when this snapshot was taken, for merging what
    /// another instance changed meanwhile; `None` overwrites (healing).
    base: Option<Settings>,
}

/// How one file of a job ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FileResult {
    /// The job did not include the file.
    Skipped,
    /// Written, or skipped because a newer generation already was.
    Done,
    Failed(String),
}

/// What a [`SaveJob`] did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SaveReport {
    pub(crate) generation: u64,
    pub(crate) settings: FileResult,
    pub(crate) session: FileResult,
}

impl SaveReport {
    /// The failure messages, one per failed file.
    pub(crate) fn errors(&self) -> Vec<&str> {
        [&self.settings, &self.session]
            .into_iter()
            .filter_map(|result| match result {
                FileResult::Failed(message) => Some(message.as_str()),
                FileResult::Skipped | FileResult::Done => None,
            })
            .collect()
    }
}

fn result_of(path: &Path, saved: std::io::Result<settings::SaveOutcome>) -> FileResult {
    match saved {
        Ok(_) => FileResult::Done,
        Err(error) => FileResult::Failed(format!("{}: {error}", path.display())),
    }
}

impl SaveJob {
    /// Writes the files. Blocking: run it off the UI thread, except on close.
    pub(crate) fn run(self) -> SaveReport {
        let settings = self.settings.as_ref().map_or(FileResult::Skipped, |write| {
            result_of(
                &write.path,
                settings::save_settings_ordered(
                    &write.path,
                    &write.value,
                    write.base.as_ref(),
                    self.generation,
                ),
            )
        });
        let session = self
            .session
            .as_ref()
            .map_or(FileResult::Skipped, |(path, session)| {
                result_of(
                    path,
                    settings::save_session_ordered(path, session, self.generation),
                )
            });
        SaveReport {
            generation: self.generation,
            settings,
            session,
        }
    }
}

/// The newest job queued for a file and the baseline it replaced, to roll
/// back to when that job fails.
#[derive(Debug)]
struct Queued<T> {
    generation: u64,
    previous: Option<T>,
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
    /// against. `None` never matches: the file is written at the first
    /// opportunity (an invalid file that was kept aside heals this way).
    saved_settings: Option<Settings>,
    saved_session: Option<Session>,
    queued_settings: Option<Queued<Settings>>,
    queued_session: Option<Queued<Session>>,
    next_generation: u64,
    /// The last snapshot that differed from the saved one, and when it was
    /// first seen: the debounce window.
    pending: Option<(Settings, Session, Instant)>,
    /// A background save is running.
    in_flight: bool,
    /// The background does not save again before this (after a failure).
    retry_not_before: Option<Instant>,
    /// A failed save was already reported; later failures stay in the log.
    failure_reported: bool,
}

impl Persistence {
    /// Nothing is ever written; for tests and a missing home.
    #[cfg(test)]
    fn disabled() -> Self {
        Self::new(
            None,
            None,
            Some(Settings::default()),
            Some(Session::default()),
        )
    }

    /// `saved_*` is what the file held at startup; `None` marks a file that
    /// must be rewritten at the first opportunity.
    fn new(
        settings_path: Option<PathBuf>,
        session_path: Option<PathBuf>,
        saved_settings: Option<Settings>,
        saved_session: Option<Session>,
    ) -> Self {
        Self {
            settings_path,
            session_path,
            saved_settings,
            saved_session,
            queued_settings: None,
            queued_session: None,
            next_generation: 1,
            pending: None,
            in_flight: false,
            retry_not_before: None,
            failure_reported: false,
        }
    }

    fn settings_dirty(&self, settings: &Settings) -> bool {
        self.settings_path.is_some() && self.saved_settings.as_ref() != Some(settings)
    }

    fn session_dirty(&self, session: &Session) -> bool {
        self.session_path.is_some() && self.saved_session.as_ref() != Some(session)
    }

    fn dirty(&self, settings: &Settings, session: &Session) -> bool {
        self.settings_dirty(settings) || self.session_dirty(session)
    }

    fn take_job(&mut self, settings: Settings, session: Session) -> SaveJob {
        let generation = self.next_generation;
        self.next_generation += 1;
        // Measured against what was queued; a failed save rolls the baseline
        // back in `finished`, so the change is still pending.
        let settings_write = match &self.settings_path {
            Some(path) if self.settings_dirty(&settings) => {
                let previous = self.saved_settings.replace(settings.clone());
                self.queued_settings = Some(Queued {
                    generation,
                    previous: previous.clone(),
                });
                Some(SettingsWrite {
                    path: path.clone(),
                    value: settings,
                    base: previous,
                })
            }
            _ => None,
        };
        let session_write = match &self.session_path {
            Some(path) if self.session_dirty(&session) => {
                let previous = self.saved_session.replace(session.clone());
                self.queued_session = Some(Queued {
                    generation,
                    previous,
                });
                Some((path.clone(), session))
            }
            _ => None,
        };
        SaveJob {
            generation,
            settings: settings_write,
            session: session_write,
        }
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
        if self.in_flight
            || now.duration_since(since) < DEBOUNCE
            || self.retry_not_before.is_some_and(|until| now < until)
        {
            return None;
        }
        self.pending = None;
        self.in_flight = true;
        Some(self.take_job(settings, session))
    }

    /// The save for the window closing: whatever differs from the saved
    /// files, without waiting. Run it on the spot — the process ends next.
    /// It carries the newest generation, so a background save still in flight
    /// either already wrote older state (this overwrites it) or becomes a
    /// no-op when it reaches the write lock.
    pub(crate) fn flush(&mut self, settings: Settings, session: Session) -> Option<SaveJob> {
        self.pending = None;
        self.dirty(&settings, &session)
            .then(|| self.take_job(settings, session))
    }

    /// A background save ended. A failed file's baseline is rolled back to
    /// what it was before that save, so the state is still dirty and the
    /// close flush (and a later retry) writes it. Returns the status notice
    /// for the **first** failure only; the explorer never blocks on a save.
    pub(crate) fn finished(&mut self, report: &SaveReport, now: Instant) -> Option<String> {
        self.in_flight = false;
        // Only the newest job queued for a file may roll it back: an older
        // one failing says nothing about what a newer one already did.
        if matches!(report.settings, FileResult::Failed(_))
            && let Some(queued) = self
                .queued_settings
                .take_if(|queued| queued.generation == report.generation)
        {
            self.saved_settings = queued.previous;
        }
        if matches!(report.session, FileResult::Failed(_))
            && let Some(queued) = self
                .queued_session
                .take_if(|queued| queued.generation == report.generation)
        {
            self.saved_session = queued.previous;
        }
        let errors = report.errors();
        let Some(first) = errors.first() else {
            self.retry_not_before = None;
            return None;
        };
        self.retry_not_before = Some(now + RETRY_AFTER_FAILURE);
        log::warn!("settings were not saved: {}", errors.join("; "));
        if std::mem::replace(&mut self.failure_reported, true) {
            return None;
        }
        Some(format!("Could not save settings ({first})"))
    }
}

/// The status notice for `count` restored tabs whose folder no longer exists.
pub(crate) fn restore_fallback_notice(count: usize) -> String {
    format!(
        "{count} restored tab{} pointed at a folder that no longer exists; \
         opened the nearest existing parent instead",
        if count == 1 { "" } else { "s" }
    )
}

// --- loading --------------------------------------------------------------------

/// Everything read before the window opens.
#[derive(Debug)]
pub(crate) struct Startup {
    /// The preferences, clamped to sane values.
    pub(crate) preferences: Preferences,
    /// The favorites: the local entries of the GTK bookmarks file.
    pub(crate) favorites: Vec<Bookmark>,
    /// Favorites that could not move to the bookmarks file yet; they stay in
    /// `settings.toml` and show with the others.
    pub(crate) legacy_favorites: Vec<PathBuf>,
    /// The bookmarks file, when `$HOME` or `$XDG_CONFIG_HOME` gives one.
    pub(crate) bookmarks_path: Option<PathBuf>,
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
        let bookmarks_path = bookmarks::bookmarks_path(
            home.as_deref(),
            std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        );
        Self::load_from(
            settings_path,
            session_path,
            bookmarks_path,
            start,
            home.as_deref().map(Path::new),
            SystemTime::now(),
        )
    }

    /// [`Self::load`] against explicit files and clock. Nothing here probes
    /// the folders of the restored tabs: they open as saved and the explorer
    /// falls back when a scan finds one gone, off the UI thread.
    pub(crate) fn load_from(
        settings_path: Option<PathBuf>,
        session_path: Option<PathBuf>,
        bookmarks_path: Option<PathBuf>,
        start: Option<&Path>,
        home: Option<&Path>,
        now: SystemTime,
    ) -> Self {
        let mut notices = Vec::new();

        let (mut settings, settings_writable, settings_rewrite) = match &settings_path {
            Some(path) => {
                let loaded = settings::load_settings(path, now);
                notices.extend(loaded.notice);
                (
                    loaded.value,
                    loaded.writable,
                    loaded.heal || loaded.migrated,
                )
            }
            None => (Settings::default(), false, false),
        };

        // Version 1 kept the favorites in the settings: they move to the
        // bookmarks once, and the settings are rewritten without them. When
        // the move fails they stay where they are and are retried next launch.
        let mut legacy_favorites = std::mem::take(&mut settings.favorites);
        let mut settings_rewrite = settings_rewrite;
        if let (Some(path), true, false) = (
            &bookmarks_path,
            settings_writable,
            legacy_favorites.is_empty(),
        ) {
            // A relative entry names no folder a bookmarks file could hold:
            // it is dropped, with one notice, rather than failing the batch.
            let (absolute, relative): (Vec<PathBuf>, Vec<PathBuf>) = legacy_favorites
                .iter()
                .cloned()
                .partition(|folder| folder.is_absolute());
            match bookmarks::merge(path, &absolute) {
                Ok(_) => {
                    legacy_favorites.clear();
                    if !relative.is_empty() {
                        let listed = relative
                            .iter()
                            .map(|folder| folder.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        notices.push(format!(
                            "Left out of the GTK bookmarks, because they are not absolute paths: {listed}"
                        ));
                    }
                    // Only now, with the bookmarks written, do the favorites
                    // leave `settings.toml`, in this same step: were they left
                    // for the first tick, a launch that dies before it would
                    // merge them again and bring back one removed meanwhile.
                    // A failed write leaves the file as it was and the first
                    // tick retries it.
                    settings_rewrite = match settings_path
                        .as_deref()
                        .map(|settings_path| settings::save_settings(settings_path, &settings))
                    {
                        Some(Ok(())) => false,
                        Some(Err(error)) => {
                            log::warn!(
                                "cannot drop the moved favorites from the settings: {error}"
                            );
                            true
                        }
                        None => true,
                    };
                }
                Err(error) => notices.push(format!(
                    "Could not move the favorites to the GTK bookmarks ({error}); \
                     keeping them in the settings"
                )),
            }
        }
        // Not writable settings (newer or unreadable) keep their favorites
        // to themselves: nothing is shown from them and nothing is touched.
        if !settings_writable {
            legacy_favorites.clear();
        }
        let mut favorites = match &bookmarks_path {
            Some(path) => bookmarks::read(path).unwrap_or_else(|error| {
                notices.push(format!("Could not read the GTK bookmarks ({error})"));
                Vec::new()
            }),
            None => Vec::new(),
        };
        for path in &legacy_favorites {
            if !favorites.iter().any(|known| known.path == *path) {
                favorites.push(Bookmark {
                    path: path.clone(),
                    label: None,
                });
            }
        }

        // A command-line path means "just this folder": the session is
        // neither read nor restored nor overwritten.
        let uses_session = settings::launch_uses_session(start);
        let (session, session_writable, session_heal) = match &session_path {
            Some(path) if uses_session => {
                let loaded = settings::load_session(path, now);
                notices.extend(loaded.notice);
                (
                    loaded.value,
                    loaded.writable,
                    loaded.heal || loaded.migrated,
                )
            }
            _ => (Session::default(), false, false),
        };

        let restored = (uses_session && !session.tabs.is_empty())
            .then(|| settings::restore_session(&session, home));
        if let Some(restored) = &restored {
            let moved = restored
                .tabs
                .iter()
                .filter(|tab| tab.fell_back_from.is_some())
                .count();
            if moved > 0 {
                notices.push(restore_fallback_notice(moved));
            }
        }

        Self {
            preferences: settings.preferences.clone().sanitized(),
            favorites,
            legacy_favorites: legacy_favorites.clone(),
            bookmarks_path,
            restored,
            persistence: Persistence::new(
                settings_path.filter(|_| settings_writable),
                session_path.filter(|_| session_writable && uses_session),
                // An invalid file kept aside is rewritten at the first tick:
                // `None` never matches the live state.
                (!settings_rewrite).then_some(Settings {
                    favorites: legacy_favorites,
                    ..settings
                }),
                (!session_heal).then_some(session),
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
            Some(Settings::default()),
            Some(Session::default()),
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
                .map(|write| write.value.favorites.clone()),
            Some(vec![PathBuf::from("/b")])
        );
        assert!(
            job.session.is_none(),
            "an unchanged session is not rewritten"
        );
        // While the save runs and after it, the same state is not saved again.
        assert!(observe(&mut persistence, "/b", Duration::from_millis(2_000)).is_none());
        assert_eq!(persistence.finished(&done(&job), start), None);
        assert!(observe(&mut persistence, "/b", Duration::from_millis(3_000)).is_none());
    }

    #[test]
    fn files_that_must_not_be_written_are_left_out_of_the_job() {
        let mut persistence = Persistence::new(
            Some(PathBuf::from("/unused/settings.toml")),
            None,
            Some(Settings::default()),
            Some(Session::default()),
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

    /// A report of a job whose every file was written.
    fn done(job: &SaveJob) -> SaveReport {
        SaveReport {
            generation: job.generation,
            settings: if job.settings.is_some() {
                FileResult::Done
            } else {
                FileResult::Skipped
            },
            session: if job.session.is_some() {
                FileResult::Done
            } else {
                FileResult::Skipped
            },
        }
    }

    /// A report of a job whose settings write failed.
    fn settings_failed(job: &SaveJob) -> SaveReport {
        SaveReport {
            settings: FileResult::Failed("/x/settings.toml: denied".to_owned()),
            ..done(job)
        }
    }

    #[test]
    fn only_the_first_failed_save_is_reported() {
        let mut persistence = persistence(Path::new("/unused"));
        let now = Instant::now();
        let first = persistence
            .flush(settings_with("/a"), Session::default())
            .expect("a change is queued");
        let notice = persistence.finished(&settings_failed(&first), now);
        assert_eq!(
            notice.as_deref(),
            Some("Could not save settings (/x/settings.toml: denied)")
        );
        let second = persistence
            .flush(settings_with("/a"), Session::default())
            .expect("the failed change is still pending");
        assert_eq!(persistence.finished(&settings_failed(&second), now), None);
        let third = persistence
            .flush(settings_with("/a"), Session::default())
            .expect("still pending");
        assert_eq!(persistence.finished(&done(&third), now), None);
    }

    #[test]
    fn a_failed_save_keeps_the_state_dirty_so_the_close_flush_retries() {
        let mut persistence = persistence(Path::new("/unused"));
        let start = Instant::now();
        assert!(
            persistence
                .observe(settings_with("/a"), Session::default(), start)
                .is_none()
        );
        let job = persistence
            .observe(settings_with("/a"), Session::default(), start + DEBOUNCE)
            .expect("saved after the debounce");
        let notice = persistence.finished(&settings_failed(&job), start + DEBOUNCE);
        assert!(notice.is_some());

        let retry = persistence
            .flush(settings_with("/a"), Session::default())
            .expect("the close flush retries what failed");
        assert!(retry.settings.is_some());
        assert!(
            retry.session.is_none(),
            "the unchanged session is not rewritten"
        );
        // A write that succeeds settles it.
        assert_eq!(persistence.finished(&done(&retry), start), None);
        assert!(
            persistence
                .flush(settings_with("/a"), Session::default())
                .is_none()
        );
    }

    #[test]
    fn the_background_waits_before_retrying_a_failed_save() {
        let mut persistence = persistence(Path::new("/unused"));
        let start = Instant::now();
        let observe = |persistence: &mut Persistence, at: Duration| {
            persistence.observe(settings_with("/a"), Session::default(), start + at)
        };
        assert!(observe(&mut persistence, Duration::ZERO).is_none());
        let job = observe(&mut persistence, DEBOUNCE).expect("first attempt");
        persistence.finished(&settings_failed(&job), start + DEBOUNCE);
        assert!(observe(&mut persistence, DEBOUNCE * 2).is_none());
        assert!(
            observe(
                &mut persistence,
                DEBOUNCE + RETRY_AFTER_FAILURE - Duration::from_secs(1)
            )
            .is_none(),
            "no retry loop"
        );
        assert!(
            observe(&mut persistence, DEBOUNCE + RETRY_AFTER_FAILURE).is_some(),
            "retried once the wait is over"
        );
    }

    #[test]
    fn an_older_job_failing_does_not_roll_back_a_newer_one() {
        let mut persistence = persistence(Path::new("/unused"));
        let start = Instant::now();
        persistence.observe(settings_with("/a"), Session::default(), start);
        let older = persistence
            .observe(settings_with("/a"), Session::default(), start + DEBOUNCE)
            .expect("background job");
        let newer = persistence
            .flush(settings_with("/b"), Session::default())
            .expect("close flush");
        assert!(newer.generation > older.generation);
        persistence.finished(&settings_failed(&older), start + DEBOUNCE);
        assert_eq!(persistence.finished(&done(&newer), start), None);
        assert!(
            persistence
                .flush(settings_with("/b"), Session::default())
                .is_none(),
            "the newer write settled the file"
        );
    }

    #[test]
    fn every_snapshot_gets_a_newer_generation() {
        let mut persistence = persistence(Path::new("/unused"));
        let first = persistence
            .flush(settings_with("/a"), Session::default())
            .expect("job");
        let second = persistence
            .flush(settings_with("/b"), Session::default())
            .expect("job");
        assert!(second.generation > first.generation);
    }

    #[test]
    fn a_slow_background_job_cannot_overwrite_the_close_flush() {
        let scratch = Scratch::new("ordering");
        let mut persistence = persistence(&scratch.0);
        let start = Instant::now();
        persistence.observe(settings_with("/old"), Session::default(), start);
        let background = persistence
            .observe(settings_with("/old"), Session::default(), start + DEBOUNCE)
            .expect("background job");
        // The window closes while the background save is still queued.
        let close = persistence
            .flush(settings_with("/new"), Session::default())
            .expect("close flush");

        let report = close.run();
        assert_eq!(report.settings, FileResult::Done);
        let late = background.run();
        assert_eq!(
            late.settings,
            FileResult::Done,
            "a skipped save is not a failure"
        );

        let on_disk =
            settings::load_settings(&scratch.0.join("settings.toml"), SystemTime::UNIX_EPOCH);
        assert_eq!(on_disk.value.favorites, vec![PathBuf::from("/new")]);
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
            Some(dir.join("config/gtk-3.0/bookmarks")),
            start,
            Some(Path::new("/home/u")),
            SystemTime::UNIX_EPOCH,
        )
    }

    #[test]
    fn startup_restores_the_session_as_saved_without_probing_the_folders() {
        let scratch = Scratch::new("restore");
        saved_session(&scratch.0);
        // `/fx/a` and `/fx/gone` do not exist on this machine: startup opens
        // both as saved, in order, and has nothing to say yet. The explorer
        // falls back off the UI thread when a scan finds a folder gone.
        let startup = load(&scratch.0, None);
        let restored = startup.restored.expect("the session is restored");
        assert_eq!(restored.tabs.len(), 2);
        assert_eq!(restored.active, 1);
        assert_eq!(
            restored.tabs[0].location,
            Location::Directory(PathBuf::from("/fx/a"))
        );
        assert_eq!(
            restored.tabs[1].location,
            Location::Directory(PathBuf::from("/fx/gone"))
        );
        assert!(startup.notices.is_empty(), "{:?}", startup.notices);
        assert!(startup.persistence.session_path.is_some());
    }

    #[test]
    fn the_restore_notice_keeps_its_text() {
        assert_eq!(
            restore_fallback_notice(1),
            "1 restored tab pointed at a folder that no longer exists; \
             opened the nearest existing parent instead"
        );
        assert_eq!(
            restore_fallback_notice(3),
            "3 restored tabs pointed at a folder that no longer exists; \
             opened the nearest existing parent instead"
        );
    }

    /// Runs the first background save the way the explorer does.
    fn first_tick(startup: &mut Startup) {
        let start = Instant::now();
        let settings = Settings::default();
        let session = Session {
            tabs: vec![SessionTab::default()],
            ..Session::default()
        };
        assert!(
            startup
                .persistence
                .observe(settings.clone(), session.clone(), start)
                .is_none()
        );
        let job = startup
            .persistence
            .observe(settings, session, start + DEBOUNCE)
            .expect("the first tick rewrites an invalid file");
        let report = job.run();
        assert_eq!(startup.persistence.finished(&report, start), None);
    }

    fn bad_copies(dir: &Path) -> usize {
        ["config", "state"]
            .iter()
            .flat_map(|sub| std::fs::read_dir(dir.join(sub)).expect("dir reads"))
            .filter(|entry| {
                entry
                    .as_ref()
                    .expect("entry reads")
                    .file_name()
                    .to_string_lossy()
                    .contains(".bad-")
            })
            .count()
    }

    #[test]
    fn a_corrupt_file_heals_on_the_first_tick_and_the_next_launch_is_quiet() {
        let scratch = Scratch::new("heal");
        for (sub, file, text) in [
            ("config", "settings.toml", "version = [[[ nope"),
            ("state", "session.toml", "version = 1\nactive = \"first\"\n"),
        ] {
            let path = scratch.0.join(sub).join(file);
            std::fs::create_dir_all(path.parent().expect("has a parent")).expect("dir creates");
            std::fs::write(&path, text).expect("fixture writes");
        }

        let mut first = load(&scratch.0, None);
        assert_eq!(first.notices.len(), 2, "{:?}", first.notices);
        first_tick(&mut first);
        assert_eq!(bad_copies(&scratch.0), 2);

        // Second launch: valid files, no notice, no new aside copy, and
        // nothing is rewritten because nothing differs.
        let mut second = Startup::load_from(
            Some(scratch.0.join("config/settings.toml")),
            Some(scratch.0.join("state/session.toml")),
            Some(scratch.0.join("config/gtk-3.0/bookmarks")),
            None,
            Some(Path::new("/home/u")),
            SystemTime::UNIX_EPOCH + Duration::from_secs(3_600),
        );
        assert!(second.notices.is_empty(), "{:?}", second.notices);
        assert_eq!(bad_copies(&scratch.0), 2);
        assert!(
            second
                .persistence
                .flush(
                    Settings::default(),
                    Session {
                        tabs: vec![SessionTab::default()],
                        ..Session::default()
                    }
                )
                .is_none()
        );
    }

    // --- favorites come from the GTK bookmarks ------------------------------------

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("has a parent")).expect("dir creates");
        std::fs::write(path, text).expect("fixture writes");
    }

    fn favorite_paths(startup: &Startup) -> Vec<PathBuf> {
        startup
            .favorites
            .iter()
            .map(|bookmark| bookmark.path.clone())
            .collect()
    }

    #[test]
    fn the_favorites_are_the_local_bookmarks_with_their_labels() {
        let scratch = Scratch::new("bookmarks-read");
        write(
            &scratch.0.join("config/gtk-3.0/bookmarks"),
            "file:///srv/a Alpha\nsftp://h/x Remote\nfile:///srv/b\n",
        );
        write(&scratch.0.join("config/settings.toml"), "version = 2\n");
        let startup = load(&scratch.0, None);
        assert_eq!(
            favorite_paths(&startup),
            [PathBuf::from("/srv/a"), PathBuf::from("/srv/b")]
        );
        assert_eq!(startup.favorites[0].label.as_deref(), Some("Alpha"));
        assert!(startup.legacy_favorites.is_empty());
        assert!(startup.notices.is_empty(), "{:?}", startup.notices);
    }

    #[test]
    fn a_missing_bookmarks_file_means_no_favorites_and_no_notice() {
        let scratch = Scratch::new("bookmarks-missing");
        let startup = load(&scratch.0, None);
        assert!(startup.favorites.is_empty());
        assert!(startup.notices.is_empty(), "{:?}", startup.notices);
        assert!(
            !scratch.0.join("config/gtk-3.0").exists(),
            "reading creates nothing"
        );
    }

    #[test]
    fn version_1_favorites_move_to_the_bookmarks_once_and_leave_the_settings() {
        let scratch = Scratch::new("bookmarks-migrate");
        let bookmarks = scratch.0.join("config/gtk-3.0/bookmarks");
        let original = "# mine\nfile:///srv/b Bee\nsftp://h/x Remote\n";
        write(&bookmarks, original);
        write(
            &scratch.0.join("config/settings.toml"),
            "version = 1\nfavorites = [\"/srv/a\", \"/srv/b\", \"/srv/c d\"]\n",
        );

        let mut first = load(&scratch.0, None);

        assert_eq!(
            std::fs::read_to_string(&bookmarks).expect("reads"),
            format!("{original}file:///srv/a\nfile:///srv/c%20d\n"),
            "missing ones appended in order, the rest untouched"
        );
        assert_eq!(
            favorite_paths(&first),
            [
                PathBuf::from("/srv/b"),
                PathBuf::from("/srv/a"),
                PathBuf::from("/srv/c d")
            ]
        );
        assert!(first.legacy_favorites.is_empty());
        assert!(first.notices.is_empty(), "{:?}", first.notices);

        // The settings file is rewritten at the first tick without them.
        first_tick(&mut first);
        let text = std::fs::read_to_string(scratch.0.join("config/settings.toml")).expect("reads");
        assert!(!text.contains("favorites"), "{text}");
        assert!(text.contains("version = 2"), "{text}");

        // The next launch changes nothing and finds the same favorites.
        let second = load(&scratch.0, None);
        assert_eq!(favorite_paths(&second), favorite_paths(&first));
        assert_eq!(
            std::fs::read_to_string(&bookmarks).expect("reads"),
            format!("{original}file:///srv/a\nfile:///srv/c%20d\n")
        );
    }

    #[test]
    fn relative_legacy_favorites_are_dropped_with_one_notice_and_the_rest_still_move() {
        let scratch = Scratch::new("bookmarks-relative");
        let bookmarks = scratch.0.join("config/gtk-3.0/bookmarks");
        write(
            &scratch.0.join("config/settings.toml"),
            "version = 1\nfavorites = [\"/srv/a\", \"rel/dir\", \"/srv/b\", \"other\"]\n",
        );

        let mut first = load(&scratch.0, None);

        assert_eq!(
            std::fs::read_to_string(&bookmarks).expect("reads"),
            "file:///srv/a\nfile:///srv/b\n",
            "the absolute ones moved, in order"
        );
        assert_eq!(
            favorite_paths(&first),
            [PathBuf::from("/srv/a"), PathBuf::from("/srv/b")]
        );
        assert!(first.legacy_favorites.is_empty());
        assert_eq!(first.notices.len(), 1, "{:?}", first.notices);
        assert!(
            first.notices[0].contains("rel/dir") && first.notices[0].contains("other"),
            "the notice lists what was dropped: {:?}",
            first.notices
        );

        first_tick(&mut first);
        let text = std::fs::read_to_string(scratch.0.join("config/settings.toml")).expect("reads");
        assert!(!text.contains("favorites"), "{text}");

        let second = load(&scratch.0, None);
        assert!(second.notices.is_empty(), "one time: {:?}", second.notices);
        assert_eq!(favorite_paths(&second), favorite_paths(&first));
    }

    #[test]
    fn an_interrupted_launch_after_the_move_never_brings_a_removed_favorite_back() {
        let scratch = Scratch::new("bookmarks-interrupted");
        let bookmarks = scratch.0.join("config/gtk-3.0/bookmarks");
        let settings = scratch.0.join("config/settings.toml");
        write(&bookmarks, "file:///srv/b\n");
        write(
            &settings,
            "version = 1\nfavorites = [\"/srv/a\", \"/srv/b\"]\n[preferences]\ndefault_view = \"list\"\n",
        );

        // The launch moves the favorites and is killed before the first tick.
        let first = load(&scratch.0, None);
        assert_eq!(
            favorite_paths(&first),
            [PathBuf::from("/srv/b"), PathBuf::from("/srv/a")]
        );
        let text = std::fs::read_to_string(&settings).expect("reads");
        assert!(
            !text.contains("favorites") && text.contains("version = 2"),
            "the legacy favorites left the settings in the same step: {text}"
        );
        assert!(text.contains("list"), "the other choices are kept: {text}");

        // Meanwhile the user removes /srv/a in another program.
        write(&bookmarks, "file:///srv/b\n");
        let second = load(&scratch.0, None);
        assert_eq!(favorite_paths(&second), [PathBuf::from("/srv/b")]);
        assert_eq!(
            std::fs::read_to_string(&bookmarks).expect("reads"),
            "file:///srv/b\n"
        );
    }

    #[test]
    fn favorites_stay_in_the_settings_when_the_bookmarks_cannot_be_written() {
        use std::os::unix::fs::PermissionsExt as _;
        let scratch = Scratch::new("bookmarks-unwritable");
        let gtk = scratch.0.join("config/gtk-3.0");
        std::fs::create_dir_all(&gtk).expect("dir creates");
        std::fs::set_permissions(&gtk, std::fs::Permissions::from_mode(0o500))
            .expect("permissions set");
        let can_still_write = std::fs::write(gtk.join("probe"), "").is_ok();
        write(
            &scratch.0.join("config/settings.toml"),
            "version = 1\nfavorites = [\"/srv/a\"]\n",
        );

        let startup = load(&scratch.0, None);
        std::fs::set_permissions(&gtk, std::fs::Permissions::from_mode(0o700))
            .expect("permissions restored");
        if can_still_write {
            return; // running as root: a read-only directory proves nothing
        }
        assert_eq!(startup.legacy_favorites, [PathBuf::from("/srv/a")]);
        assert_eq!(favorite_paths(&startup), [PathBuf::from("/srv/a")]);
        assert!(
            std::fs::read_to_string(scratch.0.join("config/settings.toml"))
                .expect("reads")
                .contains("favorites"),
            "the settings keep what the bookmarks did not receive"
        );
        assert!(
            startup.notices[0].contains("favorites"),
            "{:?}",
            startup.notices
        );
    }

    #[test]
    fn a_settings_file_that_must_not_be_written_does_not_touch_the_bookmarks() {
        let scratch = Scratch::new("bookmarks-newer");
        write(
            &scratch.0.join("config/settings.toml"),
            "version = 9\nfavorites = [\"/srv/a\"]\n",
        );
        let startup = load(&scratch.0, None);
        assert!(startup.favorites.is_empty());
        assert!(!scratch.0.join("config/gtk-3.0/bookmarks").exists());
    }

    #[test]
    fn version_2_settings_never_re_add_a_favorite_removed_from_the_bookmarks() {
        let scratch = Scratch::new("bookmarks-removed");
        write(
            &scratch.0.join("config/gtk-3.0/bookmarks"),
            "file:///srv/b\n",
        );
        write(&scratch.0.join("config/settings.toml"), "version = 2\n");
        let startup = load(&scratch.0, None);
        assert_eq!(favorite_paths(&startup), [PathBuf::from("/srv/b")]);
    }

    #[test]
    fn an_unwritable_corrupt_file_is_never_rewritten() {
        use std::os::unix::fs::PermissionsExt as _;
        let scratch = Scratch::new("no-aside");
        let dir = scratch.0.join("config");
        std::fs::create_dir_all(&dir).expect("dir creates");
        let path = dir.join("settings.toml");
        std::fs::write(&path, "garbage [[[").expect("fixture writes");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500))
            .expect("permissions set");
        let can_still_write = std::fs::write(dir.join("probe"), "").is_ok();

        let mut startup = load(&scratch.0, None);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("permissions restored");
        if can_still_write {
            return; // running as root: a read-only directory proves nothing
        }
        assert!(startup.notices[0].contains("could not keep a copy"));
        assert!(startup.persistence.settings_path.is_none());
        assert!(
            startup
                .persistence
                .flush(settings_with("/a"), Session::default())
                .is_none()
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("file reads"),
            "garbage [[["
        );
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
