//! The per-folder view memory as the explorer holds it: the remembered
//! views ([`fileflux::core::folder_views`]), the debounced atomic save to
//! `folders.toml`, and the empty-space "Reset View for This Folder" row.
//!
//! The save works like `session.toml`'s (see [`super::persist`]): the file is
//! written once the memory has stopped changing for [`DEBOUNCE`], off the UI
//! thread, through the generation-ordered atomic writer; the close flush
//! writes what is pending on the spot; a failed save rolls the baseline back
//! so it is retried, and only the first failure is reported.

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use fileflux::core::Location;
use fileflux::core::folder_views::{self, FolderView, FolderViews};

use super::arrange::Arrangement;
use super::ops::{MenuCommand, MenuItemSpec, MenuRow};
use super::persist::{
    DEBOUNCE, RETRY_AFTER_FAILURE, arrangement_from_prefs, arrangement_to_prefs, view_from_pref,
    view_to_pref,
};
use super::state::{ExplorerState, View};

/// The label of the empty-space row that forgets a folder's view.
pub(crate) const RESET_LABEL: &str = "Reset View for This Folder";

fn stored(view: View, arrangement: &Arrangement) -> FolderView {
    FolderView {
        view: view_to_pref(view),
        arrangement: arrangement_to_prefs(arrangement),
    }
}

fn live(stored: &FolderView) -> (View, Arrangement) {
    (
        view_from_pref(stored.view),
        arrangement_from_prefs(&stored.arrangement),
    )
}

/// A background write of the folders file.
#[derive(Debug)]
pub(crate) struct FolderSaveJob {
    generation: u64,
    path: PathBuf,
    views: FolderViews,
}

/// How a [`FolderSaveJob`] ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FolderSaveReport {
    generation: u64,
    /// `path: error` when the write failed.
    pub(crate) error: Option<String>,
}

impl FolderSaveJob {
    /// Writes the file. Blocking: run it off the UI thread, except on close.
    pub(crate) fn run(self) -> FolderSaveReport {
        let error = folder_views::save_folders_ordered(&self.path, &self.views, self.generation)
            .err()
            .map(|error| format!("{}: {error}", self.path.display()));
        FolderSaveReport {
            generation: self.generation,
            error,
        }
    }
}

#[derive(Debug)]
struct Queued {
    generation: u64,
    previous: Option<FolderViews>,
}

/// What is remembered, and when it is written.
#[derive(Debug)]
pub(crate) struct FolderMemory {
    views: FolderViews,
    /// `None`: never written (no usable directory, or written by a newer
    /// Fileflux).
    path: Option<PathBuf>,
    /// What was last written or queued. `None` never matches: the file is
    /// written at the first opportunity (an invalid file kept aside heals
    /// this way).
    saved: Option<FolderViews>,
    queued: Option<Queued>,
    next_generation: u64,
    /// Bumped on every change of `views`.
    revision: u64,
    /// The revision last seen dirty and when it was first seen: the debounce.
    seen: Option<(u64, Instant)>,
    in_flight: bool,
    retry_not_before: Option<Instant>,
    failure_reported: bool,
}

impl FolderMemory {
    /// Remembers in memory only: for tests and a missing home.
    pub(crate) fn disabled() -> Self {
        Self::with(FolderViews::default(), None, Some(FolderViews::default()))
    }

    fn with(views: FolderViews, path: Option<PathBuf>, saved: Option<FolderViews>) -> Self {
        Self {
            views,
            path,
            saved,
            queued: None,
            next_generation: 1,
            revision: 0,
            seen: None,
            in_flight: false,
            retry_not_before: None,
            failure_reported: false,
        }
    }

    /// Reads the file at `path`; `now` stamps the aside copy of a corrupt
    /// one. The notice is for the status bar.
    pub(crate) fn load(path: Option<PathBuf>, now: SystemTime) -> (Self, Option<String>) {
        let Some(path) = path else {
            return (Self::disabled(), None);
        };
        let loaded = folder_views::load_folders(&path, now);
        let saved = (!loaded.heal).then(|| loaded.value.clone());
        let writable = loaded.writable;
        let memory = Self::with(loaded.value, writable.then_some(path), saved);
        (memory, loaded.notice)
    }

    /// Whether `path` has a remembered view.
    pub(crate) fn has(&self, path: &Path) -> bool {
        self.views.get(path).is_some()
    }

    /// Remembers `view` and `arrangement` for the folder `path`.
    pub(crate) fn remember(&mut self, path: &Path, view: View, arrangement: &Arrangement) {
        let before = self.views.clone();
        self.views.record(path, &stored(view, arrangement));
        if self.views != before {
            self.revision += 1;
        }
    }

    /// Forgets the folder `path`; whether it was remembered.
    pub(crate) fn forget(&mut self, path: &Path) -> bool {
        let forgot = self.views.forget(path);
        if forgot {
            self.revision += 1;
        }
        forgot
    }

    /// The view and arrangement a tab showing `view` and `arrangement`
    /// takes when it commits to `location` (see
    /// [`folder_views::presentation_for`]).
    pub(crate) fn presentation_for(
        &self,
        location: &Location,
        view: View,
        arrangement: &Arrangement,
    ) -> (View, Arrangement) {
        live(&folder_views::presentation_for(
            location,
            &self.views,
            &stored(view, arrangement),
        ))
    }

    fn dirty(&self) -> bool {
        self.path.is_some() && self.saved.as_ref() != Some(&self.views)
    }

    fn take_job(&mut self) -> Option<FolderSaveJob> {
        let path = self.path.clone()?;
        let generation = self.next_generation;
        self.next_generation += 1;
        let previous = self.saved.replace(self.views.clone());
        self.queued = Some(Queued {
            generation,
            previous,
        });
        Some(FolderSaveJob {
            generation,
            path,
            views: self.views.clone(),
        })
    }

    /// The save to run once the memory differs from what is saved and has not
    /// changed for [`DEBOUNCE`]; the caller reports its end with
    /// [`Self::finished`].
    pub(crate) fn observe(&mut self, now: Instant) -> Option<FolderSaveJob> {
        if !self.dirty() {
            self.seen = None;
            return None;
        }
        let since = match self.seen {
            Some((revision, since)) if revision == self.revision => since,
            _ => {
                self.seen = Some((self.revision, now));
                return None;
            }
        };
        if self.in_flight
            || now.duration_since(since) < DEBOUNCE
            || self.retry_not_before.is_some_and(|until| now < until)
        {
            return None;
        }
        self.seen = None;
        self.in_flight = true;
        self.take_job()
    }

    /// The save for the window closing, without waiting. Run it on the spot.
    pub(crate) fn flush(&mut self) -> Option<FolderSaveJob> {
        self.seen = None;
        // A save still in flight may not land before the process ends: the
        // latest snapshot is written here too (it carries a newer
        // generation, so the late one is skipped).
        if self.dirty() || self.in_flight {
            self.take_job()
        } else {
            None
        }
    }

    /// A background save ended. A failed write rolls the baseline back, so
    /// the memory is still dirty and the close flush (and a later retry)
    /// writes it. Returns the notice for the **first** failure only.
    pub(crate) fn finished(&mut self, report: &FolderSaveReport, now: Instant) -> Option<String> {
        self.in_flight = false;
        let Some(error) = &report.error else {
            self.retry_not_before = None;
            return None;
        };
        if let Some(queued) = self
            .queued
            .take_if(|queued| queued.generation == report.generation)
        {
            self.saved = queued.previous;
        }
        self.retry_not_before = Some(now + RETRY_AFTER_FAILURE);
        log::warn!("folder views were not saved: {error}");
        if std::mem::replace(&mut self.failure_reported, true) {
            return None;
        }
        Some(format!("Could not save folder views ({error})"))
    }
}

/// Whether a scan that commits shows the folder's remembered view: on an
/// arrival (a visit, Back, Forward, Up) and on a tab's first scan (which is
/// a visit of its own location, so it reads as a refresh); not on a reload
/// or a background refresh, which keep what the tab shows.
pub(crate) fn applies_on_commit(live: bool, refresh: bool, first_scan: bool) -> bool {
    !live && (!refresh || first_scan)
}

/// "Reset View for This Folder" for every tab in `states` that shows `path`:
/// back to `default`, with nothing left to record again.
pub(crate) fn reset_states<'a>(
    states: impl Iterator<Item = &'a mut ExplorerState>,
    path: &Path,
    default: &(View, Arrangement),
) {
    for state in states {
        if matches!(state.current(), Location::Directory(shown) if shown == path) {
            drop(state.take_unsaved_presentation());
            state.apply_presentation(default.0, default.1.clone());
        }
    }
}

/// `rows` of a folder's empty-space menu with "Reset View for This Folder"
/// after the last of the view-related submenus (Filter, Columns, Zoom).
pub(crate) fn with_reset_row(mut rows: Vec<MenuRow>) -> Vec<MenuRow> {
    use super::ops::ArrangeMenu;
    let after = rows.iter().rposition(|row| {
        matches!(row, MenuRow::Submenu(spec)
            if matches!(spec.menu, ArrangeMenu::Filter | ArrangeMenu::Columns | ArrangeMenu::Zoom))
    });
    if let Some(at) = after {
        rows.insert(
            at + 1,
            MenuRow::Item(MenuItemSpec::new(MenuCommand::ResetFolderView, RESET_LABEL)),
        );
    }
    rows
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::explorer::arrange::{SortDir, SortKey};
    use crate::explorer::ops::{MenuTarget, menu_rows};
    use std::time::Duration;

    fn sorted_by_size() -> Arrangement {
        let mut arrangement = Arrangement::default();
        arrangement.sort.key = SortKey::Size;
        arrangement.sort.dir = SortDir::Ascending;
        arrangement.filters.hide_hidden = false;
        arrangement
    }

    fn dir(path: &str) -> Location {
        Location::Directory(PathBuf::from(path))
    }

    /// A memory that writes to `file`, as if it had been read empty.
    fn writing_to(file: PathBuf) -> FolderMemory {
        FolderMemory::with(
            FolderViews::default(),
            Some(file),
            Some(FolderViews::default()),
        )
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "fileflux-foldermem-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            drop(std::fs::remove_dir_all(&dir));
            std::fs::create_dir_all(&dir).expect("scratch directory");
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

    #[test]
    fn a_remembered_folder_takes_its_view_and_others_keep_the_tab() {
        let mut memory = FolderMemory::disabled();
        memory.remember(Path::new("/known"), View::List, &sorted_by_size());
        let tab = (View::Compact, Arrangement::default());
        assert_eq!(
            memory.presentation_for(&dir("/known"), tab.0, &tab.1),
            (View::List, sorted_by_size())
        );
        assert_eq!(
            memory.presentation_for(&dir("/other"), tab.0, &tab.1),
            tab,
            "no memory: today's behaviour"
        );
        for location in [Location::Trash, Location::Recent] {
            assert_eq!(memory.presentation_for(&location, tab.0, &tab.1), tab);
        }
        let search = Location::Search {
            root: PathBuf::from("/known"),
            query: "q".to_owned(),
        };
        assert_eq!(memory.presentation_for(&search, tab.0, &tab.1), tab);
    }

    #[test]
    fn forgetting_a_folder_returns_it_to_the_tab_presentation() {
        let mut memory = FolderMemory::disabled();
        memory.remember(Path::new("/known"), View::List, &sorted_by_size());
        assert!(memory.has(Path::new("/known")));
        assert!(memory.forget(Path::new("/known")));
        assert!(!memory.forget(Path::new("/known")));
        assert!(!memory.has(Path::new("/known")));
        let tab = (View::Grid, Arrangement::default());
        assert_eq!(memory.presentation_for(&dir("/known"), tab.0, &tab.1), tab);
    }

    #[test]
    fn a_change_is_written_once_after_the_debounce_and_reloads() {
        let scratch = Scratch::new("debounce");
        let mut memory = writing_to(scratch.file());
        let start = Instant::now();
        assert!(memory.observe(start).is_none(), "nothing to write");
        memory.remember(Path::new("/a"), View::List, &sorted_by_size());
        assert!(memory.observe(start).is_none(), "the window starts");
        assert!(
            memory.observe(start + DEBOUNCE / 2).is_none(),
            "still inside"
        );
        // Another change restarts the window.
        memory.remember(Path::new("/b"), View::Compact, &Arrangement::default());
        assert!(memory.observe(start + DEBOUNCE).is_none(), "restarted");
        let job = memory
            .observe(start + DEBOUNCE * 2)
            .expect("settled: written");
        assert!(
            memory.observe(start + DEBOUNCE * 3).is_none(),
            "one job in flight"
        );
        let report = job.run();
        assert_eq!(report.error, None);
        assert_eq!(memory.finished(&report, start + DEBOUNCE * 3), None);
        assert!(memory.observe(start + DEBOUNCE * 4).is_none(), "saved");
        let (reloaded, notice) = FolderMemory::load(Some(scratch.file()), SystemTime::now());
        assert_eq!(notice, None);
        assert_eq!(
            reloaded.presentation_for(&dir("/a"), View::Grid, &Arrangement::default()),
            (View::List, sorted_by_size())
        );
        assert!(reloaded.has(Path::new("/b")));
    }

    #[test]
    fn closing_writes_what_is_pending_without_waiting() {
        let scratch = Scratch::new("flush");
        let mut memory = writing_to(scratch.file());
        assert!(memory.flush().is_none(), "nothing changed");
        memory.remember(Path::new("/a"), View::List, &sorted_by_size());
        let report = memory.flush().expect("pending").run();
        assert_eq!(report.error, None);
        assert!(memory.flush().is_none(), "written");
        assert!(scratch.file().exists());
    }

    #[test]
    fn remembering_the_same_view_again_is_not_a_change() {
        let scratch = Scratch::new("same");
        let mut memory = writing_to(scratch.file());
        memory.remember(Path::new("/a"), View::List, &sorted_by_size());
        memory.flush().expect("pending").run();
        memory.remember(Path::new("/a"), View::List, &sorted_by_size());
        assert!(memory.flush().is_none());
    }

    #[test]
    fn a_failed_write_is_reported_once_retried_later_and_kept_for_the_close() {
        let scratch = Scratch::new("fail");
        // The directory of the file is a file: the write cannot succeed.
        let blocker = scratch.0.join("blocker");
        std::fs::write(&blocker, "x").expect("blocker");
        let mut memory = writing_to(blocker.join("folders.toml"));
        memory.remember(Path::new("/a"), View::List, &sorted_by_size());
        let start = Instant::now();
        memory.observe(start);
        let job = memory.observe(start + DEBOUNCE).expect("first try");
        let report = job.run();
        assert!(report.error.is_some());
        let notice = memory.finished(&report, start + DEBOUNCE).expect("notice");
        assert!(
            notice.starts_with("Could not save folder views ("),
            "{notice}"
        );
        memory.remember(Path::new("/b"), View::List, &sorted_by_size());
        memory.observe(start + DEBOUNCE);
        assert!(
            memory.observe(start + DEBOUNCE * 3).is_none(),
            "no retry before the pause"
        );
        let later = start + RETRY_AFTER_FAILURE + DEBOUNCE * 3;
        let retry = memory.observe(later).expect("retried after the pause");
        let report = retry.run();
        assert_eq!(memory.finished(&report, later + DEBOUNCE), None, "once");
        assert!(memory.flush().is_some(), "still dirty for the close");
    }

    #[test]
    fn a_corrupt_file_gives_one_notice_an_empty_memory_and_is_healed() {
        let scratch = Scratch::new("corrupt");
        let file = scratch.file();
        std::fs::create_dir_all(file.parent().expect("parent")).expect("dir");
        std::fs::write(&file, "[[folders").expect("written");
        let (mut memory, notice) = FolderMemory::load(Some(file.clone()), SystemTime::now());
        let notice = notice.expect("a notice");
        assert!(
            notice.starts_with(
                "Folder views file was invalid; using defaults (kept as folders.toml.bad-"
            ),
            "{notice}"
        );
        assert!(!memory.has(Path::new("/a")));
        memory.flush().expect("healed at the first chance").run();
        let (_, second) = FolderMemory::load(Some(file), SystemTime::now());
        assert_eq!(second, None, "the notice shows once");
    }

    #[test]
    fn without_a_path_nothing_is_ever_written() {
        let (mut memory, notice) = FolderMemory::load(None, SystemTime::now());
        assert_eq!(notice, None);
        memory.remember(Path::new("/a"), View::List, &sorted_by_size());
        assert!(memory.flush().is_none());
        assert!(
            memory
                .observe(Instant::now() + Duration::from_secs(10))
                .is_none()
        );
        assert!(memory.has(Path::new("/a")), "still remembered in memory");
    }

    #[test]
    fn a_remembered_folder_does_not_leak_into_the_next_unremembered_one() {
        use crate::explorer::state::ExplorerState;
        let mut memory = FolderMemory::disabled();
        memory.remember(Path::new("/a"), View::List, &sorted_by_size());
        let mut state = ExplorerState::new(dir("/start"), None);
        let (view, arrangement) = memory.presentation_for(
            &dir("/a"),
            state.own_presentation().0,
            &state.own_presentation().1,
        );
        state.apply_presentation(view, arrangement);
        assert_eq!(state.view(), View::List, "A shows its memory");
        let own = state.own_presentation();
        assert_eq!(own, (View::Grid, Arrangement::default()), "not A's");
        let (view, arrangement) = memory.presentation_for(&dir("/b"), own.0, &own.1);
        assert_eq!((view, arrangement), (View::Grid, Arrangement::default()));
        // What the user chooses is the tab's own choice from then on.
        state.set_view(View::Compact);
        assert_eq!(state.own_presentation().0, View::Compact);
    }

    #[test]
    fn closing_writes_the_latest_even_while_a_save_is_in_flight() {
        let scratch = Scratch::new("inflight");
        let mut memory = writing_to(scratch.file());
        memory.remember(Path::new("/a"), View::List, &sorted_by_size());
        let start = Instant::now();
        memory.observe(start);
        let in_flight = memory.observe(start + DEBOUNCE).expect("in flight");
        // Nothing changed since the job was queued, but it has not landed.
        let job = memory
            .flush()
            .expect("close does not rely on the background");
        assert_eq!(job.run().error, None);
        // The slow background write arrives late and is superseded.
        assert_eq!(in_flight.run().error, None);
        let (reloaded, _) = FolderMemory::load(Some(scratch.file()), SystemTime::now());
        assert!(reloaded.has(Path::new("/a")));
    }

    #[test]
    fn reset_returns_every_tab_on_the_folder_to_the_default_and_clears_unsaved() {
        use crate::explorer::state::ExplorerState;
        let default = (View::Grid, Arrangement::default());
        let mut shown_twice = [
            ExplorerState::new(dir("/a"), None),
            ExplorerState::new(dir("/a"), None),
            ExplorerState::new(dir("/b"), None),
        ];
        for state in &mut shown_twice {
            state.set_view(View::List);
        }
        reset_states(shown_twice.iter_mut(), Path::new("/a"), &default);
        for state in &shown_twice[..2] {
            assert_eq!(state.view(), View::Grid);
        }
        assert!(shown_twice[0].take_unsaved_presentation().is_none());
        assert!(shown_twice[1].take_unsaved_presentation().is_none());
        assert_eq!(shown_twice[2].view(), View::List, "another folder");
        assert!(shown_twice[2].take_unsaved_presentation().is_some());
    }

    #[test]
    fn the_memory_applies_on_arrival_only() {
        // (live, refresh, first scan of the tab)
        assert!(applies_on_commit(false, false, false), "visit, back, up");
        assert!(applies_on_commit(false, true, true), "a tab's first scan");
        assert!(!applies_on_commit(false, true, false), "reload");
        assert!(!applies_on_commit(true, false, false), "live refresh");
        assert!(!applies_on_commit(true, true, true), "live refresh");
    }

    fn empty_space() -> Vec<MenuRow> {
        let target = MenuTarget {
            in_trash: false,
            selected: 0,
            folder_is_empty: false,
            terminal: true,
            can_paste: true,
            folder_selected: false,
            single_file: false,
            folder_favorite: false,
        };
        crate::explorer::arrange_menu::with_zoom(menu_rows(&target, &Arrangement::default()))
    }

    fn reset_rows(rows: &[MenuRow]) -> usize {
        rows.iter()
            .filter(|row| matches!(row, MenuRow::Item(spec) if spec.command == MenuCommand::ResetFolderView))
            .count()
    }

    #[test]
    fn the_reset_row_follows_the_view_group_and_carries_its_label() {
        let plain = empty_space();
        assert_eq!(reset_rows(&plain), 0, "not offered by the plain menu");
        let rows = with_reset_row(plain);
        assert_eq!(reset_rows(&rows), 1);
        let at = rows
            .iter()
            .position(|row| matches!(row, MenuRow::Item(spec) if spec.command == MenuCommand::ResetFolderView))
            .expect("the row");
        assert!(matches!(
            &rows[at],
            MenuRow::Item(spec) if spec.label == "Reset View for This Folder"
        ));
        assert!(matches!(
            &rows[at - 1],
            MenuRow::Submenu(spec) if spec.label == "Zoom"
        ));
    }
}
