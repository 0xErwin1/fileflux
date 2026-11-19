//! The pure tab logic: the ordered tab list with its active tab, the label a
//! tab shows, and which tabs an operation's result refreshes.
//!
//! Nothing here touches GPUI or the filesystem; [`super::view::Explorer`]
//! keeps its per-tab state in a [`TabList`] and asks it what to do.

use std::path::{Path, PathBuf};

use fileflux::core::Location;

use super::ops::Job;

/// Where a newly opened tab goes relative to the active one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Placement {
    /// Opens to the right of the active tab and becomes the active tab
    /// (Ctrl+T).
    Foreground,
    /// Opens to the right of the active tab, which stays active
    /// (middle-click, "Open in new tab").
    Background,
}

/// An ordered list of tabs with one active tab. It is never empty — the
/// last tab cannot be closed — and that is built into its shape: the active
/// tab is a field of its own, with the tabs on either side of it in two
/// lists.
#[derive(Debug)]
pub(crate) struct TabList<T> {
    /// The tabs left of the active one, in order.
    before: Vec<T>,
    active: T,
    /// The tabs right of the active one, in order.
    after: Vec<T>,
    /// How many background tabs were opened since the active tab last
    /// changed: the next one goes after them, so several middle-clicks keep
    /// their click order instead of reversing it.
    background_run: usize,
}

impl<T> TabList<T> {
    /// A list holding `first`, active.
    pub(crate) fn new(first: T) -> Self {
        Self {
            before: Vec::new(),
            active: first,
            after: Vec::new(),
            background_run: 0,
        }
    }

    /// How many tabs there are (at least one).
    pub(crate) fn len(&self) -> usize {
        self.before.len() + 1 + self.after.len()
    }

    /// The position of the active tab.
    pub(crate) fn active_index(&self) -> usize {
        self.before.len()
    }

    /// The active tab.
    pub(crate) fn active(&self) -> &T {
        &self.active
    }

    /// The active tab, mutably.
    pub(crate) fn active_mut(&mut self) -> &mut T {
        &mut self.active
    }

    /// The tabs, left to right.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> {
        self.before
            .iter()
            .chain(std::iter::once(&self.active))
            .chain(self.after.iter())
    }

    /// The tabs, left to right, mutably.
    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.before
            .iter_mut()
            .chain(std::iter::once(&mut self.active))
            .chain(self.after.iter_mut())
    }

    /// The tab at `index`.
    pub(crate) fn get(&self, index: usize) -> Option<&T> {
        self.iter().nth(index)
    }

    /// The position of the first tab `matches` accepts.
    pub(crate) fn position(&self, matches: impl Fn(&T) -> bool) -> Option<usize> {
        self.iter().position(matches)
    }

    /// Opens `item` to the right of the active tab; returns its position.
    pub(crate) fn open(&mut self, item: T, placement: Placement) -> usize {
        match placement {
            Placement::Foreground => {
                let previous = std::mem::replace(&mut self.active, item);
                self.before.push(previous);
                self.background_run = 0;
                self.before.len()
            }
            Placement::Background => {
                let at = self.background_run.min(self.after.len());
                self.after.insert(at, item);
                self.background_run += 1;
                self.before.len() + 1 + at
            }
        }
    }

    /// Closes the tab at `index` and returns it. `None` — nothing changes —
    /// for the last remaining tab and for a position past the end. Closing
    /// the active tab activates its right neighbour, else its left one;
    /// closing another tab keeps the same tab active.
    pub(crate) fn close(&mut self, index: usize) -> Option<T> {
        if self.len() <= 1 || index >= self.len() {
            return None;
        }
        self.background_run = 0;
        let active_at = self.before.len();
        if index < active_at {
            return Some(self.before.remove(index));
        }
        if index > active_at {
            return Some(self.after.remove(index - active_at - 1));
        }
        let successor = if self.after.is_empty() {
            self.before.pop()?
        } else {
            self.after.remove(0)
        };
        Some(std::mem::replace(&mut self.active, successor))
    }

    /// Makes the tab at `index` the active one; `false` past the end.
    pub(crate) fn activate(&mut self, index: usize) -> bool {
        let active_at = self.before.len();
        if index >= self.len() {
            return false;
        }
        if index < active_at {
            // The tabs from `index` on, up to the active one, shift right.
            let mut moved = self.before.split_off(index).into_iter();
            let Some(next) = moved.next() else {
                return false;
            };
            let previous = std::mem::replace(&mut self.active, next);
            let mut after: Vec<T> = moved.collect();
            after.push(previous);
            after.append(&mut self.after);
            self.after = after;
            self.background_run = 0;
        } else if index > active_at {
            // The tabs between the active one and `index` shift left.
            let mut rest = self.after.split_off(index - active_at - 1).into_iter();
            let Some(next) = rest.next() else {
                return false;
            };
            let previous = std::mem::replace(&mut self.active, next);
            self.before.push(previous);
            self.before.append(&mut self.after);
            self.after = rest.collect();
            self.background_run = 0;
        }
        true
    }

    /// Moves the active tab by `step` positions, wrapping around the ends.
    pub(crate) fn cycle(&mut self, step: isize) {
        let count = self.len() as isize;
        let next = (self.active_index() as isize + step).rem_euclid(count);
        self.activate(next as usize);
    }

    /// Alt+1..9: activates tab number `number` (counting from one); `false`
    /// when there is no such tab.
    pub(crate) fn jump(&mut self, number: usize) -> bool {
        number
            .checked_sub(1)
            .is_some_and(|index| self.activate(index))
    }
}

/// The text a tab shows: `Home` for the home directory, `Trash` for the
/// Trash, else the folder name (`/` for the filesystem root).
pub(crate) fn tab_label(location: &Location, home: Option<&Path>) -> String {
    match location {
        Location::Trash => "Trash".to_owned(),
        Location::Recent => "Recent".to_owned(),
        Location::Directory(path) if home == Some(path.as_path()) => "Home".to_owned(),
        Location::Directory(path) => path.file_name().map_or_else(
            || path.to_string_lossy().into_owned(),
            |name| name.to_string_lossy().into_owned(),
        ),
    }
}

/// The full path a tab's tooltip shows.
pub(crate) fn tab_tooltip(location: &Location, trash_root: Option<&Path>) -> String {
    match location {
        Location::Trash => trash_root.map_or_else(
            || "Trash".to_owned(),
            |root| root.to_string_lossy().into_owned(),
        ),
        Location::Recent => "Recent files".to_owned(),
        Location::Directory(path) => path.to_string_lossy().into_owned(),
    }
}

/// The folders an operation changed, which decides the tabs its result
/// refreshes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Affected {
    /// Exactly these locations.
    Folders(Vec<Location>),
    /// The operation's reach is not known up front (a restore puts items
    /// back where they came from, a paste may move out of other folders):
    /// every tab refreshes.
    Everything,
}

impl Affected {
    /// What `job` changes. A job that removes or adds items also changes the
    /// Trash view.
    pub(crate) fn of_job(job: &Job) -> Self {
        // Each parent folder once, in first-seen order.
        let parents = |paths: &[PathBuf]| -> Vec<Location> {
            let mut folders: Vec<Location> = Vec::new();
            for parent in paths.iter().filter_map(|path| path.parent()) {
                let folder = Location::Directory(parent.to_path_buf());
                if !folders.contains(&folder) {
                    folders.push(folder);
                }
            }
            folders
        };
        match job {
            Job::Create { dir, .. } => Self::Folders(vec![Location::Directory(dir.clone())]),
            Job::Rename { path, .. } => {
                let mut folders = parents(std::slice::from_ref(path));
                folders.push(Location::Recent);
                Self::Folders(folders)
            }
            Job::Trash(paths) | Job::DeletePermanently(paths) => {
                let mut folders = parents(paths);
                folders.push(Location::Trash);
                folders.push(Location::Recent);
                Self::Folders(folders)
            }
            Job::Restore(_)
            | Job::DeleteFromTrash(_)
            | Job::EmptyTrash(_)
            | Job::Undo(_)
            | Job::Paste(_) => Self::Everything,
        }
    }

    /// Whether a tab showing `location` must refresh.
    pub(crate) fn touches(&self, location: &Location) -> bool {
        match self {
            Self::Everything => true,
            Self::Folders(folders) => folders.contains(location),
        }
    }
}

/// The positions of the tabs (showing `shown`, left to right) a finished
/// operation refreshes: those showing an affected folder, and the tab the
/// operation was started from (`origin`), which waits for its result
/// whatever it shows now.
pub(crate) fn tabs_to_refresh(
    shown: &[&Location],
    affected: &Affected,
    origin: Option<usize>,
) -> Vec<usize> {
    shown
        .iter()
        .enumerate()
        .filter(|(index, location)| affected.touches(location) || origin == Some(*index))
        .map(|(index, _)| index)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::ops::NewKind;
    use crate::explorer::state::{ExplorerState, Transition};
    use fileflux::core::{Entry, EntryKind, Listing};
    use std::ffi::OsString;

    /// A list of the tabs named `names`, the first one active.
    fn list(names: &[&'static str]) -> TabList<&'static str> {
        let Some((first, rest)) = names.split_first() else {
            return TabList::new("");
        };
        let mut tabs = TabList::new(*first);
        for name in rest {
            tabs.open(*name, Placement::Foreground);
        }
        tabs.activate(0);
        tabs
    }

    fn order(tabs: &TabList<&'static str>) -> Vec<&'static str> {
        tabs.iter().copied().collect()
    }

    #[test]
    fn a_new_list_has_one_active_tab() {
        let tabs = TabList::new("a");
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs.active_index(), 0);
        assert_eq!(*tabs.active(), "a");
    }

    #[test]
    fn a_foreground_tab_opens_right_of_the_active_one_and_activates() {
        let mut tabs = list(&["a", "b", "c"]);
        tabs.activate(1);
        let at = tabs.open("n", Placement::Foreground);
        assert_eq!(at, 2);
        assert_eq!(order(&tabs), ["a", "b", "n", "c"]);
        assert_eq!(*tabs.active(), "n");
    }

    #[test]
    fn a_background_tab_opens_right_of_the_active_one_and_keeps_it_active() {
        let mut tabs = list(&["a", "b", "c"]);
        let at = tabs.open("n", Placement::Background);
        assert_eq!(at, 1);
        assert_eq!(order(&tabs), ["a", "n", "b", "c"]);
        assert_eq!(*tabs.active(), "a");
    }

    #[test]
    fn successive_background_tabs_keep_their_click_order() {
        let mut tabs = list(&["a", "b"]);
        tabs.open("x", Placement::Background);
        tabs.open("y", Placement::Background);
        tabs.open("z", Placement::Background);
        assert_eq!(order(&tabs), ["a", "x", "y", "z", "b"]);
        assert_eq!(*tabs.active(), "a");
    }

    /// Opens a restored session the way the explorer does: the first tab
    /// alone, the rest behind it as background tabs, then the saved active
    /// tab takes over.
    fn restore(saved: &[&'static str], active: usize) -> TabList<&'static str> {
        let Some((first, rest)) = saved.split_first() else {
            return TabList::new("");
        };
        let mut tabs = TabList::new(*first);
        for tab in rest {
            tabs.open(*tab, Placement::Background);
        }
        tabs.activate(active);
        tabs
    }

    #[test]
    fn restored_tabs_keep_their_saved_order_and_active_index() {
        let saved = ["a", "b", "c", "d"];
        for active in 0..saved.len() {
            let tabs = restore(&saved, active);
            assert_eq!(order(&tabs), saved, "left to right, as saved");
            assert_eq!(tabs.active_index(), active);
            assert_eq!(Some(tabs.active()), saved.get(active));
        }
        let single = restore(&["only"], 0);
        assert_eq!(order(&single), ["only"]);
        assert_eq!(single.active_index(), 0);
    }

    #[test]
    fn the_background_run_restarts_after_the_active_tab_changes() {
        let mut tabs = list(&["a", "b"]);
        tabs.open("x", Placement::Background);
        tabs.activate(2);
        tabs.activate(0);
        tabs.open("y", Placement::Background);
        assert_eq!(order(&tabs), ["a", "y", "x", "b"]);
    }

    #[test]
    fn activating_a_tab_never_reorders_the_list() {
        let mut tabs = list(&["a", "b", "c", "d"]);
        for index in [3, 1, 2, 0, 3, 0] {
            assert!(tabs.activate(index));
            assert_eq!(tabs.active_index(), index);
            assert_eq!(order(&tabs), ["a", "b", "c", "d"]);
        }
        assert!(!tabs.activate(4), "past the end changes nothing");
        assert_eq!(tabs.active_index(), 0);
    }

    #[test]
    fn the_last_tab_is_never_closed() {
        let mut tabs = TabList::new("only");
        assert_eq!(tabs.close(0), None);
        assert_eq!(tabs.len(), 1);
        assert_eq!(*tabs.active(), "only");
    }

    #[test]
    fn closing_past_the_end_changes_nothing() {
        let mut tabs = list(&["a", "b"]);
        assert_eq!(tabs.close(5), None);
        assert_eq!(order(&tabs), ["a", "b"]);
    }

    #[test]
    fn closing_the_active_tab_activates_its_right_neighbour() {
        let mut tabs = list(&["a", "b", "c"]);
        tabs.activate(1);
        assert_eq!(tabs.close(1), Some("b"));
        assert_eq!(order(&tabs), ["a", "c"]);
        assert_eq!(*tabs.active(), "c");
    }

    #[test]
    fn closing_the_active_rightmost_tab_activates_its_left_neighbour() {
        let mut tabs = list(&["a", "b", "c"]);
        tabs.activate(2);
        assert_eq!(tabs.close(2), Some("c"));
        assert_eq!(*tabs.active(), "b");
        assert_eq!(tabs.active_index(), 1);
    }

    #[test]
    fn closing_another_tab_keeps_the_same_tab_active() {
        let mut tabs = list(&["a", "b", "c", "d"]);
        tabs.activate(2);
        assert_eq!(tabs.close(0), Some("a"));
        assert_eq!(*tabs.active(), "c", "a tab left of the active one closed");
        assert_eq!(tabs.close(2), Some("d"));
        assert_eq!(*tabs.active(), "c", "a tab right of the active one closed");
    }

    #[test]
    fn cycling_wraps_around_both_ends() {
        let mut tabs = list(&["a", "b", "c"]);
        tabs.cycle(1);
        assert_eq!(*tabs.active(), "b");
        tabs.cycle(1);
        tabs.cycle(1);
        assert_eq!(*tabs.active(), "a", "forward wraps to the first tab");
        tabs.cycle(-1);
        assert_eq!(*tabs.active(), "c", "backward wraps to the last tab");
        let mut single = TabList::new("x");
        single.cycle(1);
        single.cycle(-1);
        assert_eq!(*single.active(), "x");
    }

    #[test]
    fn jumping_counts_from_one_and_ignores_missing_tabs() {
        let mut tabs = list(&["a", "b", "c"]);
        assert!(tabs.jump(3));
        assert_eq!(*tabs.active(), "c");
        assert!(tabs.jump(1));
        assert_eq!(*tabs.active(), "a");
        assert!(!tabs.jump(4), "there is no fourth tab");
        assert!(!tabs.jump(0), "tab numbers start at one");
        assert_eq!(*tabs.active(), "a", "a missing tab leaves the active one");
    }

    // --- per-tab state isolation ---------------------------------------------

    fn dir(path: &str) -> Location {
        Location::Directory(PathBuf::from(path))
    }

    fn file(name: &str) -> Entry {
        Entry {
            name: OsString::from(name),
            path: PathBuf::from(format!("/fixture/{name}")),
            kind: EntryKind::File,
            is_symlink: false,
            size: Some(1),
            modified: None,
            origin: None,
        }
    }

    fn visit(state: &mut ExplorerState, to: &str, names: &[&str]) {
        let scan = state.begin(Transition::Visit(dir(to)));
        let committed = scan.is_some_and(|scan| {
            state.commit(
                scan,
                Listing {
                    entries: names.iter().map(|name| file(name)).collect(),
                    warnings: Vec::new(),
                    ..Listing::default()
                },
            )
        });
        assert!(committed, "the visit to {to} must commit");
    }

    #[test]
    fn navigating_one_tab_leaves_the_selection_and_history_of_another() {
        let mut first = ExplorerState::new(dir("/fixture"), None);
        visit(&mut first, "/fixture", &["a", "b"]);
        first.select_only(Path::new("/fixture/b"));
        first.set_query("b".to_owned());
        let mut tabs = TabList::new(first);

        let second = tabs.active().for_new_tab(dir("/fixture"));
        tabs.open(second, Placement::Foreground);
        visit(tabs.active_mut(), "/elsewhere", &["x"]);
        tabs.active_mut().select_only(Path::new("/fixture/x"));

        tabs.activate(0);
        let first = tabs.active();
        assert_eq!(first.current(), &dir("/fixture"));
        assert!(first.selection().contains(Path::new("/fixture/b")));
        assert_eq!(first.selection().len(), 1);
        assert_eq!(first.query(), "b");
        assert!(!first.navigation().can_back());

        tabs.activate(1);
        assert_eq!(tabs.active().current(), &dir("/elsewhere"));
        assert!(tabs.active().navigation().can_back());
        assert_eq!(tabs.active().query(), "");
    }

    // --- labels -----------------------------------------------------------------

    #[test]
    fn labels_name_the_folder_and_the_trash() {
        let home = PathBuf::from("/home/me");
        assert_eq!(tab_label(&dir("/home/me/docs"), Some(&home)), "docs");
        assert_eq!(tab_label(&dir("/home/me"), Some(&home)), "Home");
        assert_eq!(tab_label(&dir("/"), Some(&home)), "/");
        assert_eq!(tab_label(&Location::Trash, Some(&home)), "Trash");
        assert_eq!(tab_label(&Location::Recent, Some(&home)), "Recent");
        assert_eq!(tab_tooltip(&Location::Recent, None), "Recent files");
        assert_eq!(tab_label(&dir("/home/me"), None), "me");
    }

    #[test]
    fn the_tooltip_is_the_full_path() {
        assert_eq!(tab_tooltip(&dir("/a/b"), None), "/a/b");
        let root = PathBuf::from("/home/me/.local/share/Trash");
        assert_eq!(
            tab_tooltip(&Location::Trash, Some(&root)),
            "/home/me/.local/share/Trash"
        );
        assert_eq!(tab_tooltip(&Location::Trash, None), "Trash");
    }

    // --- operation refresh fan-out -----------------------------------------------

    #[test]
    fn jobs_that_remove_or_rename_files_refresh_the_recent_view() {
        let trashed = Affected::of_job(&Job::Trash(vec![PathBuf::from("/home/me/docs/a.txt")]));
        assert!(trashed.touches(&Location::Recent));
        let deleted = Affected::of_job(&Job::DeletePermanently(vec![PathBuf::from("/d/a")]));
        assert!(deleted.touches(&Location::Recent));
        let renamed = Affected::of_job(&Job::Rename {
            path: PathBuf::from("/home/me/docs/a.txt"),
            new_name: OsString::from("b.txt"),
        });
        assert!(renamed.touches(&Location::Recent));
        let created = Affected::of_job(&Job::Create {
            dir: PathBuf::from("/home/me/docs"),
            kind: NewKind::File,
        });
        assert!(!created.touches(&Location::Recent));
    }

    #[test]
    fn an_operation_refreshes_every_tab_showing_an_affected_folder() {
        let docs = dir("/home/me/docs");
        let pics = dir("/home/me/pics");
        let shown = [&docs, &pics, &docs, &Location::Trash];
        let trashed = Affected::of_job(&Job::Trash(vec![PathBuf::from("/home/me/docs/a.txt")]));
        assert_eq!(
            tabs_to_refresh(&shown, &trashed, None),
            [0, 2, 3],
            "both tabs on docs and the Trash view refresh; pics does not"
        );
    }

    #[test]
    fn creating_and_renaming_affect_the_folder_that_holds_the_item() {
        let created = Affected::of_job(&Job::Create {
            dir: PathBuf::from("/a"),
            kind: NewKind::Folder,
        });
        assert_eq!(created, Affected::Folders(vec![dir("/a")]));
        let renamed = Affected::of_job(&Job::Rename {
            path: PathBuf::from("/a/x"),
            new_name: OsString::from("y"),
        });
        // A renamed file leaves the Recent list, which names it by its old path.
        assert_eq!(
            renamed,
            Affected::Folders(vec![dir("/a"), Location::Recent])
        );
    }

    #[test]
    fn an_operation_of_unknown_reach_refreshes_every_tab() {
        let a = dir("/a");
        let shown = [&a, &Location::Trash];
        let restored = Affected::of_job(&Job::Restore(vec![OsString::from("x")]));
        assert_eq!(restored, Affected::Everything);
        assert_eq!(tabs_to_refresh(&shown, &restored, None), [0, 1]);
    }

    #[test]
    fn an_operation_leaves_unrelated_tabs_alone() {
        let a = dir("/a");
        let b = dir("/b");
        let created = Affected::Folders(vec![dir("/c")]);
        assert!(tabs_to_refresh(&[&a, &b], &created, None).is_empty());
    }

    #[test]
    fn the_tab_an_operation_started_in_always_refreshes() {
        let a = dir("/a");
        let b = dir("/b");
        let created = Affected::Folders(vec![dir("/c")]);
        assert_eq!(
            tabs_to_refresh(&[&a, &b], &created, Some(1)),
            [1],
            "the origin tab waits for its result even when it moved elsewhere"
        );
    }
}

#[cfg(test)]
mod affected_tests {
    use super::*;
    use crate::explorer::ops::{Job, NewKind};

    fn dir(path: &str) -> Location {
        Location::Directory(PathBuf::from(path))
    }

    #[test]
    fn trashing_many_items_of_one_folder_lists_that_folder_once() {
        let job = Job::Trash(vec![PathBuf::from("/d/a"), PathBuf::from("/d/b")]);
        assert_eq!(
            Affected::of_job(&job),
            Affected::Folders(vec![dir("/d"), Location::Trash, Location::Recent])
        );
    }

    /// Every job that relocates files (a cut paste and a drag-move are both
    /// `Job::Paste`, a restore and an undo put items back elsewhere) takes
    /// the Recent tabs along: Recent names files by path, and a moved file's
    /// old path no longer exists.
    #[test]
    fn relocating_jobs_refresh_recent_tabs_and_leave_nothing_stale() {
        use crate::explorer::ops::paste::{ClipMode, ConflictLink, PasteJob};
        use std::sync::mpsc::channel;

        let paste = |mode| {
            let (requests, _) = channel();
            let (_, replies) = channel();
            Job::Paste(PasteJob {
                sources: vec![PathBuf::from("/home/me/docs/a.txt")],
                dest: PathBuf::from("/home/me/pics"),
                mode,
                link: ConflictLink { requests, replies },
            })
        };
        let jobs = [
            ("cut paste or drag-move", paste(ClipMode::Cut)),
            ("copy paste", paste(ClipMode::Copy)),
            (
                "restore",
                Job::Restore(vec![std::ffi::OsString::from("a.txt")]),
            ),
            (
                "delete from trash",
                Job::DeleteFromTrash(vec![std::ffi::OsString::from("a.txt")]),
            ),
            ("empty trash", Job::EmptyTrash(Vec::new())),
        ];
        let docs = dir("/home/me/docs");
        let shown = [
            &docs,
            &Location::Recent,
            &Location::Trash,
            &Location::Recent,
        ];
        for (what, job) in &jobs {
            let affected = Affected::of_job(job);
            assert!(affected.touches(&Location::Recent), "{what}");
            assert_eq!(
                tabs_to_refresh(&shown, &affected, None),
                [0, 1, 2, 3],
                "{what}: every Recent tab re-reads"
            );
        }
        // A job that cannot move anything out of Recent leaves it alone.
        let created = Affected::of_job(&Job::Create {
            dir: PathBuf::from("/home/me/docs"),
            kind: NewKind::File,
        });
        assert_eq!(tabs_to_refresh(&shown, &created, None), [0]);
    }

    #[test]
    fn a_closed_origin_tab_still_refreshes_affected_tabs() {
        let c = dir("/c");
        let t = Location::Trash;
        let trashed = Affected::of_job(&Job::Trash(vec![PathBuf::from("/c/x")]));
        assert_eq!(tabs_to_refresh(&[&c, &t], &trashed, None), [0, 1]);
    }
}
