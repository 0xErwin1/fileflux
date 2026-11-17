//! Pure explorer state: navigation, filtering, selection, session favorites
//! and the asynchronous scan guard, testable without GPUI or the filesystem.
//!
//! The UI drives it in three steps: [`ExplorerState::begin`] turns a
//! navigation transition into a [`Scan`] the caller performs off the UI
//! thread (via [`fileflux::core::fs::read_directory`] or
//! [`fileflux::core::trash::read_trash`]); [`ExplorerState::commit`] or
//! [`ExplorerState::commit_failure`] settles the scan. A stale or failed
//! completion never touches the committed location, history, selection or
//! query: every scan carries the generation it was started under, and only a
//! completion matching the newest generation is accepted.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use fileflux::core::fs::filtered_indices;
use fileflux::core::navigation::Navigation;
use fileflux::core::{Entry, Listing, Location};

/// The two presentations of the folder contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum View {
    /// Icon grid, folders first.
    Grid,
    /// Details list: Name, Type, Size, Modified.
    List,
}

/// A navigation transition the user asked for, resolved against the
/// committed state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Transition {
    /// Go to this location (sidebar place, favorite, folder entry or a typed
    /// path). Visiting the current location is an explicit reload.
    Visit(Location),
    /// One step back in the history.
    Back,
    /// One step forward in the history.
    Forward,
    /// The parent of the current directory.
    Up,
}

/// Why a typed path cannot be a navigable location. The message is the
/// UI's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PathArgument {
    /// The path field was left empty.
    Empty,
    /// The path could not be resolved to a directory location; the detail
    /// explains what the input was missing (`HOME` unknown, relative to
    /// Trash, ...).
    Unresolvable(String),
}

/// A directory scan the state asked for, still to be performed and settled.
///
/// The `candidate` carries the navigation transition (with its history
/// change) applied to a clone of the committed [`Navigation`]. It is only
/// adopted by [`ExplorerState::commit`], so a failed scan leaves the
/// committed navigation untouched.
#[derive(Clone, Debug)]
pub(crate) struct Scan {
    /// The generation this scan was started under; only a completion that
    /// still matches [`ExplorerState::generation`] may settle.
    pub generation: u64,
    /// The location to read.
    pub location: Location,
    /// The navigation with the transition already applied.
    pub(crate) candidate: Navigation,
    /// Whether the visit re-reads the location the explorer already shows.
    pub(crate) refresh: bool,
}

/// Selection, view, query and history for one explorer window, plus the
/// scan-in-progress guard. All locations and entry paths are kept raw; only
/// display strings are lossy.
#[derive(Clone, Debug)]
pub(crate) struct ExplorerState {
    /// The committed navigation: current location, back and forward history.
    nav: Navigation,
    /// The listing of the committed location, as the scan returned it.
    listing: Listing,
    /// Grid or list presentation.
    view: View,
    /// The current-folder filename filter, as typed (case-insensitive
    /// matching happens at query time).
    query: String,
    /// Selected entry paths of the current location, raw.
    selected: BTreeSet<PathBuf>,
    /// Session favorites, in the order they were added; never persisted,
    /// unique, never the virtual Trash.
    favorites: Vec<PathBuf>,
    /// The scan waiting for its listing, if any.
    pending: Option<Scan>,
    /// Bumped on every scan that starts; completions compare against it.
    generation: u64,
    /// The home directory, when known, which `~` paths resolve against.
    home: Option<PathBuf>,
    /// Why the last scan failed; cleared by the next scan start.
    load_error: Option<String>,
    /// Why the last submitted path could not be navigated to.
    path_error: Option<PathArgument>,
    /// A non-error status note, such as the honest read-only open answer.
    notice: Option<String>,
}

impl ExplorerState {
    /// A new explorer committed to `initial` with an (unscanned) empty
    /// listing. The UI kicks off its first scan right away.
    pub(crate) fn new(initial: Location, home: Option<PathBuf>) -> Self {
        Self {
            nav: Navigation::new(initial),
            listing: Listing::default(),
            view: View::Grid,
            query: String::new(),
            selected: BTreeSet::new(),
            favorites: Vec::new(),
            pending: None,
            generation: 1,
            home,
            load_error: None,
            path_error: None,
            notice: None,
        }
    }

    /// The committed current location.
    pub(crate) fn current(&self) -> &Location {
        self.nav.current()
    }

    /// The committed navigation, for back/forward/up availability.
    pub(crate) fn navigation(&self) -> &Navigation {
        &self.nav
    }

    /// The listing of the committed location.
    pub(crate) fn listing(&self) -> &Listing {
        &self.listing
    }

    /// Warnings of the last successful scan (partial listing problems).
    pub(crate) fn warnings(&self) -> &[String] {
        &self.listing.warnings
    }

    /// Whether a scan is waiting for its result.
    pub(crate) fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    /// Why the last scan failed, if it did.
    pub(crate) fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    /// Why the last submitted path could not be navigated to.
    pub(crate) fn path_error(&self) -> Option<&PathArgument> {
        self.path_error.as_ref()
    }

    /// The honest read-only note, when one is showing.
    pub(crate) fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Shows the honest read-only note for an entry the MVP will not open.
    pub(crate) fn set_open_notice(&mut self, name: &str) {
        self.notice = Some(format!(
            "Fileflux is read-only: \"{name}\" cannot be opened or previewed here."
        ));
    }

    /// Clears the status note.
    pub(crate) fn clear_notice(&mut self) {
        self.notice = None;
    }

    /// Clears the path error; the UI does this as soon as the field changes.
    pub(crate) fn clear_path_error(&mut self) {
        self.path_error = None;
    }

    /// Dismisses errors from a canceled path edit without changing the
    /// committed location, history, listing, query or selection.
    pub(crate) fn cancel_path_edit(&mut self) {
        self.path_error = None;
        self.load_error = None;
    }

    /// Reports why a submitted path cannot be navigated to; the status bar
    /// shows it and the field returns to the committed path on Escape.
    pub(crate) fn set_path_error(&mut self, argument: PathArgument) {
        self.path_error = Some(argument);
    }

    // --- scanning -----------------------------------------------------------

    /// Turns a transition into a scan. `None` when the transition has no
    /// effect (back/forward without history, up at the root or in Trash);
    /// nothing changes then. Starting a scan drops the error message of an
    /// older scan; the path error is cleared only by
    /// [`Self::clear_path_error`], since it describes the field, not a scan.
    pub(crate) fn begin(&mut self, transition: Transition) -> Option<Scan> {
        let (location, candidate, refresh) = match transition {
            Transition::Visit(location) => {
                let refresh = *self.nav.current() == location;
                let mut candidate = self.nav.clone();
                candidate.visit(location.clone());
                (location, candidate, refresh)
            }
            Transition::Back => {
                let mut candidate = self.nav.clone();
                if !candidate.back() {
                    return None;
                }
                let location = candidate.current().clone();
                (location, candidate, false)
            }
            Transition::Forward => {
                let mut candidate = self.nav.clone();
                if !candidate.forward() {
                    return None;
                }
                let location = candidate.current().clone();
                (location, candidate, false)
            }
            Transition::Up => {
                let mut candidate = self.nav.clone();
                if !candidate.up() {
                    return None;
                }
                let location = candidate.current().clone();
                (location, candidate, false)
            }
        };
        self.generation += 1;
        self.load_error = None;
        self.pending = Some(Scan {
            generation: self.generation,
            location: location.clone(),
            candidate,
            refresh,
        });
        self.pending.clone()
    }

    /// Settles a scan with its listing. `false` (state unchanged) when the
    /// scan is stale, that is, when a newer scan has started since,
    /// including a newer scan that itself failed. On success the candidate
    /// navigation becomes committed, the listing is replaced, and the query,
    /// selection and notice are cleared: the folder (or its content) moved.
    pub(crate) fn commit(&mut self, scan: Scan, listing: Listing) -> bool {
        if scan.generation != self.generation {
            return false;
        }
        self.nav = scan.candidate;
        self.listing = listing;
        self.query.clear();
        self.selected.clear();
        self.notice = None;
        self.pending = None;
        self.load_error = None;
        true
    }

    /// Settles a scan with an error. `false` (state unchanged) when stale.
    /// The committed location, history, listing, selection and query are
    /// preserved; `load_error` is set and shown until the next scan starts.
    pub(crate) fn commit_failure(&mut self, scan: Scan, error: String) -> bool {
        if scan.generation != self.generation {
            return false;
        }
        self.pending = None;
        self.load_error = Some(error);
        true
    }

    // --- filtering ----------------------------------------------------------

    /// The current filename query, as typed.
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    /// Replaces the query. Filtering is local and nonrecursive; selection is
    /// not touched, so a filtered-out entry stays selected.
    pub(crate) fn set_query(&mut self, query: String) {
        self.query = query;
    }

    /// Clears the query; every entry of the listing is visible again.
    pub(crate) fn clear_query(&mut self) {
        self.query.clear();
    }

    /// Indices into [`Self::listing`] that pass the current query, in
    /// listing order.
    pub(crate) fn visible_indices(&self) -> Vec<usize> {
        filtered_indices(&self.listing.entries, &self.query)
    }

    /// The visible entries in listing order; used by the UI per frame and by
    /// tests through the public API.
    pub(crate) fn visible_entries(&self) -> Vec<&Entry> {
        self.visible_indices()
            .into_iter()
            .filter_map(|index| self.listing.entries.get(index))
            .collect()
    }

    /// The folder has no entries at all: a distinct state from "no matches".
    pub(crate) fn folder_is_empty(&self) -> bool {
        self.listing.entries.is_empty()
    }

    /// The query is set and filters everything out; the UI shows a "no
    /// matches" state instead of "empty folder".
    pub(crate) fn is_no_result(&self) -> bool {
        !self.query.is_empty() && self.visible_indices().is_empty()
    }

    // --- selection ----------------------------------------------------------

    /// The selected entry paths, raw.
    pub(crate) fn selection(&self) -> &BTreeSet<PathBuf> {
        &self.selected
    }

    /// Selects exactly the entry at this path (single click).
    pub(crate) fn select_only(&mut self, path: &Path) {
        self.selected.clear();
        self.selected.insert(path.to_path_buf());
    }

    /// Adds or removes the entry at this path (Ctrl+click).
    pub(crate) fn toggle(&mut self, path: &Path) {
        if !self.selected.remove(path) {
            self.selected.insert(path.to_path_buf());
        }
    }

    /// Selects the visible entry at `position` (keyboard navigation); the
    /// selection is unchanged when the position is out of range.
    pub(crate) fn select_visible(&mut self, position: usize) {
        let visible = self.visible_entries();
        let Some(entry) = visible.get(position) else {
            return;
        };
        self.select_only(&entry.path.clone());
    }

    /// The visible position of `path`, if it is on screen.
    pub(crate) fn visible_position_of(&self, path: &Path) -> Option<usize> {
        self.visible_entries()
            .iter()
            .position(|entry| entry.path == path)
    }

    /// The selected entry to activate on Enter or double click: the first
    /// selected entry that is **visible** under the current filter, or
    /// `None` when the selection holds only hidden entries (a hidden
    /// selection is never activated). An empty query makes every selected
    /// entry visible again.
    ///
    /// The selection itself is untouched: filtering out does not deselect.
    pub(crate) fn activation_candidate(&self) -> Option<PathBuf> {
        self.selected
            .iter()
            .find(|path| self.visible_position_of(path).is_some())
            .cloned()
    }

    /// Empties the selection (Escape over the entries, folder change).
    pub(crate) fn clear_selection(&mut self) {
        self.selected.clear();
    }

    // --- presentation -------------------------------------------------------

    /// Grid or list.
    pub(crate) fn view(&self) -> View {
        self.view
    }

    /// Switches the presentation; selection, folder and query are kept.
    pub(crate) fn set_view(&mut self, view: View) {
        self.view = view;
    }

    // --- favorites (session only) -------------------------------------------

    /// The session favorites, in the order they were added.
    pub(crate) fn favorites(&self) -> &[PathBuf] {
        &self.favorites
    }

    /// Whether the committed current directory is a favorite.
    pub(crate) fn is_favorite(&self) -> bool {
        match self.nav.current() {
            Location::Directory(path) => self.favorites.contains(path),
            Location::Trash => false,
        }
    }

    /// Adds the current directory to the favorites or removes it again.
    /// The virtual Trash is never favorited. Favorites live in this state
    /// only: nothing is written anywhere.
    pub(crate) fn toggle_favorite(&mut self) {
        let Location::Directory(path) = self.nav.current() else {
            return;
        };
        let path = path.clone();
        if self.favorites.contains(&path) {
            self.favorites.retain(|favorite| *favorite != path);
        } else {
            self.favorites.push(path);
        }
    }

    // --- typed paths ----------------------------------------------------------

    /// Resolves what the path field holds into a navigable location. `~`
    /// expands against the known home; relative input resolves against the
    /// current directory when there is one (never against the Trash). No
    /// shell is involved and nothing is read from the filesystem here: the
    /// scan itself reports whether the directory exists.
    pub(crate) fn resolve_input(&self, text: &str) -> Result<Location, PathArgument> {
        let text = text.trim();
        if text.is_empty() {
            return Err(PathArgument::Empty);
        }
        // `~` expands against the known home only; there is no shell here.
        if text == "~" || text.starts_with("~/") {
            let Some(home) = self.home.as_deref() else {
                return Err(PathArgument::Unresolvable(
                    "~ needs a known home".to_owned(),
                ));
            };
            let rest = text.strip_prefix("~").unwrap_or("");
            let rest = rest.strip_prefix('/').unwrap_or(rest);
            return Ok(Location::Directory(home.join(rest)));
        }
        let candidate = PathBuf::from(text);
        if candidate.is_absolute() {
            return Ok(Location::Directory(candidate));
        }
        match self.nav.current() {
            Location::Directory(current) => Ok(Location::Directory(current.join(candidate))),
            Location::Trash => Err(PathArgument::Unresolvable(
                "relative paths need a current directory".to_owned(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fileflux::core::EntryKind;
    use fileflux::core::Listing;
    use std::ffi::OsString;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn dir(path: &str) -> Location {
        Location::Directory(PathBuf::from(path))
    }

    fn entry(name: &str, kind: EntryKind) -> Entry {
        Entry {
            name: OsString::from(name),
            path: PathBuf::from(format!("/fixture/{name}")),
            kind,
            is_symlink: false,
            size: Some(10),
            modified: None,
            origin: None,
        }
    }

    fn listing(names: &[(&str, EntryKind)]) -> Listing {
        Listing {
            entries: names
                .iter()
                .map(|(name, kind)| entry(name, *kind))
                .collect(),
            warnings: Vec::new(),
        }
    }

    fn state() -> ExplorerState {
        ExplorerState::new(dir("/fixture"), Some(PathBuf::from("/fixture/home")))
    }

    /// Commits a first scan so the explorer has a real listing and history,
    /// then returns the state.
    fn committed() -> Result<ExplorerState, Box<dyn std::error::Error>> {
        let mut state = state();
        let scan = state
            .begin(Transition::Visit(dir("/fixture/a")))
            .ok_or("begin must return a scan")?;
        assert!(
            state.commit(scan, listing(&[("notes.txt", EntryKind::File)])),
            "first commit must be accepted"
        );
        Ok(state)
    }

    /// The scan of visiting `to`, committed or returned unfired per `commit`.
    fn visit_scan(state: &mut ExplorerState, to: &str) -> Result<Scan, Box<dyn std::error::Error>> {
        state
            .begin(Transition::Visit(dir(to)))
            .ok_or_else(|| "visit must produce a scan".into())
    }

    #[test]
    fn visit_commits_listing_clears_query_and_selection_and_pushes_history() -> TestResult {
        let mut state = committed()?;
        assert_eq!(state.current(), &dir("/fixture/a"));
        assert!(
            state.navigation().can_back(),
            "the visited-from place is history"
        );
        assert_eq!(state.listing().entries.len(), 1);
        assert!(state.listing().warnings.is_empty());

        // Query and selection set before moving are cleared by the commit.
        state.set_query("notes".into());
        state.select_only(Path::new("/fixture/a/notes.txt"));
        let scan = visit_scan(&mut state, "/fixture/a/b")?;
        assert!(state.commit(scan, listing(&[("z", EntryKind::Directory)])));
        assert_eq!(state.current(), &dir("/fixture/a/b"));
        assert_eq!(state.listing().entries.len(), 1);
        assert_eq!(state.query(), "", "navigation clears the query");
        assert!(
            state.selection().is_empty(),
            "navigation clears the selection"
        );
        assert!(state.navigation().can_back(), "history kept back");
        assert!(
            !state.navigation().can_forward(),
            "a new visit truncates the forward branch"
        );
        Ok(())
    }

    #[test]
    fn visiting_the_current_location_reloads_without_growing_history() -> TestResult {
        let mut state = committed()?;
        let history_before = state.navigation().can_back();

        let scan = visit_scan(&mut state, "/fixture/a")?;
        assert!(scan.refresh, "a visit to the current location is a reload");
        assert!(state.commit(scan, listing(&[])));
        assert_eq!(state.current(), &dir("/fixture/a"));
        assert_eq!(
            state.navigation().can_back(),
            history_before,
            "a reload must not push history"
        );
        Ok(())
    }

    #[test]
    fn cancel_path_edit_clears_failed_scan_error_without_changing_committed_state() -> TestResult {
        let mut state = committed()?;
        state.set_query("notes".into());
        state.select_only(Path::new("/fixture/notes.txt"));
        let current = state.current().clone();
        let selection = state.selection().clone();
        let history = (
            state.navigation().can_back(),
            state.navigation().can_forward(),
        );
        let scan = visit_scan(&mut state, "/fixture/missing")?;
        assert!(state.commit_failure(scan, "Could not open folder".into()));
        state.set_path_error(PathArgument::Empty);
        assert!(state.load_error().is_some());

        state.cancel_path_edit();

        assert!(
            state.load_error().is_none(),
            "Escape dismisses the failed scan error"
        );
        assert!(state.path_error().is_none());
        assert_eq!(state.current(), &current);
        assert_eq!(state.listing().entries.len(), 1);
        assert_eq!(state.query(), "notes");
        assert_eq!(state.selection(), &selection);
        assert_eq!(
            (
                state.navigation().can_back(),
                state.navigation().can_forward()
            ),
            history
        );
        Ok(())
    }

    #[test]
    fn failed_scan_preserves_committed_state_and_reports_the_error() -> TestResult {
        let mut state = committed()?;
        let could_back = state.navigation().can_back();
        let could_forward = state.navigation().can_forward();
        let listing_len = state.listing().entries.len();

        state.set_query("notes".into());
        state.select_only(Path::new("/fixture/a/notes.txt"));
        let scan = visit_scan(&mut state, "/fixture/missing")?;
        assert!(state.commit_failure(scan, "no such directory".to_owned()));

        assert_eq!(state.current(), &dir("/fixture/a"), "location unchanged");
        assert_eq!(
            state.listing().entries.len(),
            listing_len,
            "listing unchanged"
        );
        assert_eq!(state.query(), "notes", "failure preserves the query");
        assert_eq!(
            state.selection().clone(),
            [PathBuf::from("/fixture/a/notes.txt")].into(),
            "failure preserves the selection"
        );
        assert_eq!(
            state.navigation().can_back(),
            could_back,
            "failure preserves history"
        );
        assert_eq!(state.navigation().can_forward(), could_forward);
        assert_eq!(state.load_error(), Some("no such directory"));
        assert!(!state.is_loading(), "the failed scan is settled");

        // The next scan starts cleanly and can still commit.
        let scan = visit_scan(&mut state, "/fixture/a/c")?;
        assert!(state.commit(scan, listing(&[])));
        assert_eq!(state.load_error(), None, "a new scan clears the old error");
        Ok(())
    }

    #[test]
    fn stale_success_never_commits_and_stale_failure_after_newer_failure_is_ignored() -> TestResult
    {
        let mut state = committed()?;
        let scan_a = visit_scan(&mut state, "/fixture/older")?;
        let scan_b = visit_scan(&mut state, "/fixture/newer")?;

        // The older scan finishes after the newer one started: ignored.
        assert!(state.is_loading(), "a scan is pending");
        assert!(!state.commit(scan_a.clone(), listing(&[("x", EntryKind::File)])));
        assert_eq!(state.current(), &dir("/fixture/a"));
        assert!(state.is_loading(), "the newer scan is still waiting");

        // The newer scan fails: the error wins, loading settles.
        assert!(state.commit_failure(scan_b, "newer failed".to_owned()));
        assert_eq!(state.load_error(), Some("newer failed"));
        assert!(!state.is_loading());

        // Even the failed newer request cannot be resurrected by the stale one.
        // The stale scanner is replayed after the newer one already failed, so it
        // is cloned on the first (rejected) attempt.
        assert!(!state.commit(scan_a.clone(), listing(&[("y", EntryKind::File)])));
        assert_eq!(state.current(), &dir("/fixture/a"));
        assert!(
            !state.is_loading(),
            "a stale completion must not set loading"
        );
        assert_eq!(state.load_error(), Some("newer failed"));
        Ok(())
    }

    #[test]
    fn back_forward_and_up_are_scans_that_fail_probably_without_changes() -> TestResult {
        let mut state = committed()?;

        // With history, back produces a scan.
        let scan = state
            .begin(Transition::Back)
            .ok_or("back with history produces a scan")?;
        assert_eq!(scan.location, dir("/fixture"));
        assert!(state.commit(scan, listing(&[])));
        assert_eq!(state.current(), &dir("/fixture"));

        // drained back stack: no scan, nothing changes.
        assert!(state.begin(Transition::Back).is_none());
        assert_eq!(state.current(), &dir("/fixture"));

        // Forward after the back step: to /fixture/a.
        let scan = state
            .begin(Transition::Forward)
            .ok_or("forward with history produces a scan")?;
        assert!(state.commit(scan, listing(&[])));
        assert_eq!(state.current(), &dir("/fixture/a"));

        // Up from /fixture/a goes to /fixture.
        let scan = state
            .begin(Transition::Up)
            .ok_or("up from a non-root directory produces a scan")?;
        assert!(state.commit(scan, listing(&[])));
        assert_eq!(state.current(), &dir("/fixture"));

        // Up at the root and in Trash is a no-op.
        let mut root = ExplorerState::new(dir("/"), None);
        assert!(root.begin(Transition::Up).is_none());
        assert_eq!(root.current(), &dir("/"));
        let mut trash = ExplorerState::new(Location::Trash, None);
        assert!(trash.begin(Transition::Up).is_none());
        assert_eq!(trash.current(), &Location::Trash);
        Ok(())
    }

    #[test]
    fn query_filters_locally_and_clearing_recovers() -> TestResult {
        let mut state = state();
        let scan = visit_scan(&mut state, "/fixture/a")?;
        state.commit(
            scan,
            listing(&[
                ("Alpha.txt", EntryKind::File),
                ("beta.md", EntryKind::File),
                ("Notes", EntryKind::Directory),
            ]),
        );

        state.set_query("NOTE".into());
        assert_eq!(
            state
                .visible_entries()
                .iter()
                .map(|entry| entry.name.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            ["Notes"],
            "matching is case-insensitive"
        );
        assert!(!state.is_no_result(), "there is a match");

        state.set_query("zzz".into());
        assert!(state.visible_entries().is_empty());
        assert!(state.is_no_result(), "no matches is distinct from empty");
        assert!(!state.folder_is_empty(), "the folder itself has entries");

        state.clear_query();
        assert_eq!(state.visible_entries().len(), 3);
        assert!(!state.is_no_result());
        assert!(!state.folder_is_empty());
        Ok(())
    }

    #[test]
    fn selection_survives_view_toggle_and_stays_selected_while_filtered_out() -> TestResult {
        let mut state = state();
        let scan = visit_scan(&mut state, "/fixture/a")?;
        state.commit(
            scan,
            listing(&[("alpha", EntryKind::Directory), ("beta", EntryKind::File)]),
        );
        state.select_only(Path::new("/fixture/a/beta"));

        state.set_view(View::List);
        assert_eq!(state.view(), View::List);
        assert_eq!(
            state.selection().clone(),
            [PathBuf::from("/fixture/a/beta")].into(),
            "the view toggle keeps the selection"
        );

        state.set_query("alpha".into());
        assert!(
            state
                .visible_entries()
                .iter()
                .all(|entry| entry.name == "alpha")
        );
        assert_eq!(
            state.selection().clone(),
            [PathBuf::from("/fixture/a/beta")].into(),
            "a filtered-out entry stays selected"
        );

        state.clear_query();
        assert_eq!(
            state.selection().clone(),
            [PathBuf::from("/fixture/a/beta")].into(),
            "clearing the query restores its visibility"
        );
        Ok(())
    }

    #[test]
    fn selection_apis_select_toggle_and_clear() -> TestResult {
        let mut state = state();
        let scan = visit_scan(&mut state, "/fixture/a")?;
        state.commit(
            scan,
            listing(&[("one", EntryKind::File), ("two", EntryKind::File)]),
        );

        state.select_only(Path::new("/fixture/a/one"));
        assert_eq!(
            state.selection().clone(),
            [PathBuf::from("/fixture/a/one")].into()
        );

        state.toggle(Path::new("/fixture/a/two"));
        assert_eq!(state.selection().len(), 2);
        state.toggle(Path::new("/fixture/a/two"));
        assert_eq!(state.selection().len(), 1, "toggle removes again");

        state.select_visible(1);
        assert_eq!(
            state.selection().clone(),
            [PathBuf::from("/fixture/two")].into(),
            "the second visible entry is selected by position"
        );
        assert!(
            state
                .visible_position_of(Path::new("/fixture/two"))
                .is_some(),
            "the selected position resolves"
        );

        state.clear_selection();
        assert!(state.selection().is_empty());
        assert!(
            state
                .visible_position_of(Path::new("/fixture/nonesuch"))
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn favorites_are_session_only_unique_and_never_the_trash() -> TestResult {
        let mut state = committed()?;
        assert!(state.favorites().is_empty());
        assert!(!state.is_favorite());

        state.toggle_favorite();
        assert_eq!(state.favorites(), [PathBuf::from("/fixture/a")]);
        assert!(state.is_favorite());

        // Re-adding is a remove: the toggle is one button.
        state.toggle_favorite();
        assert!(state.favorites().is_empty());

        // Navigating elsewhere and favoriting the new directory, then
        // toggling again, keeps exactly one entry per directory.
        let scan = visit_scan(&mut state, "/fixture/b")?;
        state.commit(scan, listing(&[]));
        state.toggle_favorite();
        assert_eq!(state.favorites(), [PathBuf::from("/fixture/b")]);
        state.toggle_favorite();
        assert!(state.favorites().is_empty());
        state.toggle_favorite();
        assert_eq!(state.favorites(), [PathBuf::from("/fixture/b")]);

        // The virtual Trash is never favorited.
        let scan = state
            .begin(Transition::Visit(Location::Trash))
            .ok_or("a visit to the Trash produces a scan")?;
        state.commit(scan, listing(&[]));
        state.toggle_favorite();
        assert_eq!(
            state.favorites(),
            [PathBuf::from("/fixture/b")],
            "the Trash toggle is a no-op and keeps prior favorites"
        );
        Ok(())
    }

    #[test]
    fn typed_paths_resolve_absolute_relative_and_home() -> TestResult {
        let state = committed()?;

        assert_eq!(
            state.resolve_input("/etc").ok(),
            Some(dir("/etc")),
            "absolute input goes exactly there"
        );
        assert_eq!(
            state.resolve_input("sub").ok(),
            Some(dir("/fixture/a/sub")),
            "relative input resolves against the current directory"
        );
        assert_eq!(
            state.resolve_input("~/docs").ok(),
            Some(dir("/fixture/home/docs")),
            "~ expands against the home"
        );
        assert_eq!(state.resolve_input("~").ok(), Some(dir("/fixture/home")));

        assert!(
            matches!(state.resolve_input("   "), Err(PathArgument::Empty)),
            "only whitespace is an empty path"
        );
        let trash_state = ExplorerState::new(Location::Trash, None);
        assert_eq!(
            match trash_state.resolve_input("sub") {
                Err(argument) => argument,
                Ok(location) => PathArgument::Unresolvable(format!(
                    "resolved to {location:?} instead of failing"
                )),
            },
            PathArgument::Unresolvable("relative paths need a current directory".to_owned()),
            "relative input has no directory to resolve against in the Trash"
        );
        let homeless = ExplorerState::new(dir("/fixture"), None);
        assert_eq!(
            match homeless.resolve_input("~/x") {
                Err(argument) => argument,
                Ok(location) => {
                    PathArgument::Unresolvable(format!("resolved to {location:?} instead"))
                }
            },
            PathArgument::Unresolvable("~ needs a known home".to_owned())
        );
        Ok(())
    }

    #[test]
    fn open_notice_is_honest_and_clears_on_the_next_folder() -> TestResult {
        let mut state = committed()?;
        state.set_open_notice("report.pdf");
        assert!(
            state
                .notice()
                .is_some_and(|note| note.contains("read-only"))
        );

        let scan = visit_scan(&mut state, "/fixture/a/b")?;
        state.commit(scan, listing(&[]));
        assert!(state.notice().is_none(), "a folder change clears the note");
        Ok(())
    }

    /// Activation on Enter must consider only visible selected entries: a
    /// hidden-only selection activates nothing, and among a hidden and a
    /// visible selection the visible one wins. The selection itself stays
    /// intact across the filter, as the earlier tests pin.
    #[test]
    fn activation_uses_only_visible_selected_entries() -> TestResult {
        let mut state = state();
        let scan = visit_scan(&mut state, "/fixture/a")?;
        state.commit(
            scan,
            listing(&[("alpha", EntryKind::Directory), ("beta", EntryKind::File)]),
        );

        // Hidden-only selection: no activation at all. (The test fixture
        // builds entry paths as /fixture/<name>.)
        state.select_only(Path::new("/fixture/alpha"));
        state.set_query("nomatch".into());
        assert!(state.is_no_result(), "nothing is visible now");
        assert!(
            state.activation_candidate().is_none(),
            "a hidden selection is never activated"
        );

        // Hidden alpha plus visible beta: beta activates.
        state.clear_query();
        state.toggle(Path::new("/fixture/beta"));
        state.set_query("beta".into());
        assert_eq!(
            state.selection().len(),
            2,
            "the filter must not drop any selection"
        );
        assert_eq!(
            state.activation_candidate(),
            Some(PathBuf::from("/fixture/beta")),
            "the visible selection wins over the hidden one"
        );

        // Clearing the filter restores the usual first-selected order;
        // selection is unchanged.
        state.clear_query();
        assert_eq!(
            state.activation_candidate(),
            Some(PathBuf::from("/fixture/alpha")),
            "BTreeSet order puts alpha first"
        );
        assert_eq!(state.selection().len(), 2, "selection survived the cycle");
        Ok(())
    }
}
