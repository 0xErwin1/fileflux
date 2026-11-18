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

use std::cmp::Ordering;
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

/// What the list is sorted by.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum SortKey {
    Name,
    Size,
    Modified,
}

impl SortKey {
    /// The sort key of column `column`, or `None` past the columns.
    pub(crate) fn of_column(column: usize) -> Option<Self> {
        match column {
            0 => Some(Self::Name),
            1 => Some(Self::Size),
            2 => Some(Self::Modified),
            _ => None,
        }
    }
}

/// The direction of the sort.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum SortDir {
    Ascending,
    Descending,
}

impl SortDir {
    /// The arrow drawn beside the sorted header.
    pub(crate) fn arrow(self) -> &'static str {
        match self {
            Self::Ascending => "\u{2191}",
            Self::Descending => "\u{2193}",
        }
    }

    fn flipped(self) -> Self {
        match self {
            Self::Ascending => Self::Descending,
            Self::Descending => Self::Ascending,
        }
    }
}

/// The list's sort: a key and a direction. Starts at name ascending, the
/// order the scan already delivers.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) struct SortSpec {
    pub(crate) key: SortKey,
    pub(crate) dir: SortDir,
}

impl Default for SortSpec {
    fn default() -> Self {
        Self {
            key: SortKey::Name,
            dir: SortDir::Ascending,
        }
    }
}

impl SortSpec {
    /// The sort after a click on `key`'s header: the active key flips its
    /// direction, any other key starts ascending.
    pub(crate) fn clicked(self, key: SortKey) -> Self {
        if self.key == key {
            Self {
                key,
                dir: self.dir.flipped(),
            }
        } else {
            Self {
                key,
                dir: SortDir::Ascending,
            }
        }
    }
}

/// Orders two entries under `spec`. Folders always come first, whatever the
/// key or direction; within a group the key decides (a missing size or time
/// sorts lowest) and the direction reverses only that; ties fall back to
/// the case-insensitive name and then the raw path, ascending, so the order
/// is total and stable.
pub(crate) fn compare_entries(a: &Entry, b: &Entry, spec: SortSpec) -> Ordering {
    let by_key = match spec.key {
        SortKey::Name => Ordering::Equal,
        SortKey::Size => a.size.cmp(&b.size),
        SortKey::Modified => a.modified.cmp(&b.modified),
    };
    let by_key = match spec.dir {
        SortDir::Ascending => by_key,
        SortDir::Descending => by_key.reverse(),
    };
    let by_name = a
        .name
        .to_string_lossy()
        .to_lowercase()
        .cmp(&b.name.to_string_lossy().to_lowercase());
    // Name is its own key, so the direction applies to the name comparison.
    let by_name = if spec.key == SortKey::Name && spec.dir == SortDir::Descending {
        by_name.reverse()
    } else {
        by_name
    };
    b.is_directory()
        .cmp(&a.is_directory())
        .then(by_key)
        .then(by_name)
        .then_with(|| a.path.cmp(&b.path))
}

/// The `visible` snapshot indices reordered under `spec`.
pub(crate) fn sorted_indices(entries: &[Entry], visible: &[usize], spec: SortSpec) -> Vec<usize> {
    let mut order = visible.to_vec();
    order.sort_by(|&a, &b| match (entries.get(a), entries.get(b)) {
        (Some(a), Some(b)) => compare_entries(a, b, spec),
        (a, b) => a.is_some().cmp(&b.is_some()).reverse(),
    });
    order
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
    /// A background refresh of the shown folder: the commit keeps the
    /// selection (of entries that still exist), the filter and the notice.
    pub(crate) live: bool,
    /// The status message the commit shows, for a fallback navigation.
    pub(crate) notice: Option<String>,
}

/// The selected paths that still name an entry of `entries`.
pub(crate) fn retain_existing(
    selected: &BTreeSet<PathBuf>,
    entries: &[Entry],
) -> BTreeSet<PathBuf> {
    entries
        .iter()
        .filter(|entry| selected.contains(&entry.path))
        .map(|entry| entry.path.clone())
        .collect()
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
    /// The list's sort: the single source of truth for the displayed order
    /// the keyboard, range selection and the table all follow.
    sort: SortSpec,
    /// The entry a Shift-range selection extends from.
    anchor: Option<PathBuf>,
    /// The moving end of the selection: the entry the last click, arrow or
    /// Shift step landed on. Keyboard steps start here.
    cursor: Option<PathBuf>,
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
    /// An operation finished while a navigation was pending: its refresh
    /// runs once that navigation has committed.
    op_refresh_deferred: bool,
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
            sort: SortSpec::default(),
            anchor: None,
            cursor: None,
            query: String::new(),
            selected: BTreeSet::new(),
            favorites: Vec::new(),
            pending: None,
            op_refresh_deferred: false,
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

    /// Shows a status note until the next folder change or click.
    pub(crate) fn set_notice(&mut self, text: String) {
        self.notice = Some(text);
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
        self.begin_scan(transition, false, None)
    }

    /// A background re-read of the shown folder after the watcher saw a
    /// change. `None` while another scan is pending (it must not be
    /// cancelled) and for the Trash, which is not watched.
    pub(crate) fn begin_live_refresh(&mut self) -> Option<Scan> {
        if self.pending.is_some() {
            return None;
        }
        let current = match self.nav.current() {
            Location::Directory(path) => Location::Directory(path.clone()),
            Location::Trash => return None,
        };
        self.begin_scan(Transition::Visit(current), true, None)
    }

    /// A re-read of the shown location after one of our own operations
    /// finished, Trash included (it is not watched). Unlike the watcher's
    /// refresh it supersedes a scan in flight: that scan may have read the
    /// location before the operation changed it, and a newer generation
    /// discards it. The commit keeps the selection of entries that remain.
    ///
    /// A pending *navigation* (or fallback) is different: the user asked
    /// for another folder, and that wins. The refresh is postponed
    /// ([`Self::begin_deferred_op_refresh`]) and returns `None` now.
    pub(crate) fn begin_op_refresh(&mut self) -> Option<Scan> {
        if self.pending.as_ref().is_some_and(|scan| !scan.live) {
            self.op_refresh_deferred = true;
            return None;
        }
        let current = self.nav.current().clone();
        self.begin_scan(Transition::Visit(current), true, None)
    }

    /// The operation refresh a navigation postponed, now that the
    /// navigation has committed: re-reads the folder that is shown now.
    /// `None` when nothing was postponed or another scan is pending.
    pub(crate) fn begin_deferred_op_refresh(&mut self) -> Option<Scan> {
        if !self.op_refresh_deferred || self.pending.is_some() {
            return None;
        }
        self.op_refresh_deferred = false;
        self.begin_op_refresh()
    }

    /// Navigates to `ancestor` after the shown folder vanished; the commit
    /// shows `notice`. It supersedes any pending scan.
    pub(crate) fn begin_fallback(&mut self, ancestor: PathBuf, notice: String) -> Option<Scan> {
        self.begin_scan(
            Transition::Visit(Location::Directory(ancestor)),
            false,
            Some(notice),
        )
    }

    fn begin_scan(
        &mut self,
        transition: Transition,
        live: bool,
        notice: Option<String>,
    ) -> Option<Scan> {
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
            live,
            notice,
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
        if scan.live {
            self.selected = retain_existing(&self.selected, &self.listing.entries);
            let still_there = |path: &Option<PathBuf>| {
                path.as_ref()
                    .is_some_and(|path| self.listing.entries.iter().any(|e| e.path == *path))
            };
            if !still_there(&self.cursor) {
                self.cursor = None;
            }
            if !still_there(&self.anchor) {
                self.anchor = None;
            }
        } else {
            self.query.clear();
            self.selected.clear();
            self.cursor = None;
            self.notice = scan.notice;
        }
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

    /// Indices into [`Self::listing`] that pass the current query, in the
    /// **displayed** order: the list follows its sort, the grid keeps the
    /// scan's name order, folders first. Keyboard navigation, range
    /// selection and the table all read this one order.
    pub(crate) fn visible_indices(&self) -> Vec<usize> {
        let visible = filtered_indices(&self.listing.entries, &self.query);
        match self.view {
            View::List => sorted_indices(&self.listing.entries, &visible, self.sort),
            View::Grid => visible,
        }
    }

    /// The visible entries in displayed order; used by the UI per frame and
    /// by tests through the public API.
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
        self.anchor = Some(path.to_path_buf());
        self.cursor = Some(path.to_path_buf());
    }

    /// Adds or removes the entry at this path (Ctrl+click).
    pub(crate) fn toggle(&mut self, path: &Path) {
        if !self.selected.remove(path) {
            self.selected.insert(path.to_path_buf());
        }
        self.anchor = Some(path.to_path_buf());
        self.cursor = Some(path.to_path_buf());
    }

    /// Shift+click: selects the visible entries between the anchor and
    /// `path`, in displayed order, replacing the selection. Without a
    /// visible anchor it selects `path` alone.
    pub(crate) fn select_range_to(&mut self, path: &Path) {
        let Some(to) = self.visible_position_of(path) else {
            return;
        };
        let from = self
            .anchor
            .as_deref()
            .and_then(|anchor| self.visible_position_of(anchor));
        let Some(from) = from else {
            self.select_only(path);
            return;
        };
        let (start, end) = (from.min(to), from.max(to));
        let range: BTreeSet<PathBuf> = self
            .visible_entries()
            .iter()
            .skip(start)
            .take(end - start + 1)
            .map(|entry| entry.path.clone())
            .collect();
        self.selected = range;
        self.cursor = Some(path.to_path_buf());
    }

    /// The visible position of the selection's moving end: the cursor, else
    /// the first selected visible entry.
    pub(crate) fn cursor_position(&self) -> Option<usize> {
        self.cursor
            .as_deref()
            .and_then(|cursor| self.visible_position_of(cursor))
            .or_else(|| {
                self.selected
                    .iter()
                    .find_map(|path| self.visible_position_of(path))
            })
    }

    /// Shift+Arrow: moves the selection's moving end by `delta` visible
    /// positions (clamped) and selects everything between the anchor and
    /// it, in displayed order. Without a selection it selects the first
    /// entry, like a plain arrow.
    pub(crate) fn extend_selection_by(&mut self, delta: isize) {
        let count = self.visible_indices().len();
        if count == 0 {
            return;
        }
        let Some(current) = self.cursor_position() else {
            self.select_visible(0);
            return;
        };
        let target = current.saturating_add_signed(delta).min(count - 1);
        self.extend_selection_to(target);
    }

    /// Shift+Home / Shift+End: extends the selection from the anchor to the
    /// visible `position` (clamped to the last entry).
    pub(crate) fn extend_selection_to(&mut self, position: usize) {
        let visible = self.visible_entries();
        let Some(last) = visible.len().checked_sub(1) else {
            return;
        };
        let Some(path) = visible.get(position.min(last)).map(|e| e.path.clone()) else {
            return;
        };
        if self.anchor_visible() {
            self.select_range_to(&path);
        } else {
            self.select_only(&path);
        }
    }

    fn anchor_visible(&self) -> bool {
        self.anchor
            .as_deref()
            .is_some_and(|anchor| self.visible_position_of(anchor).is_some())
    }

    /// The combined size of the selected files; folders carry no size.
    pub(crate) fn selected_size(&self) -> u64 {
        self.listing
            .entries
            .iter()
            .filter(|entry| self.selected.contains(&entry.path))
            .filter_map(|entry| entry.size)
            .sum()
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

    /// Ctrl+A: selects every entry on screen under the current filter, in
    /// displayed order. Entries the filter hides are not selected, so an
    /// operation never reaches what the user cannot see.
    pub(crate) fn select_all_visible(&mut self) {
        let paths: Vec<PathBuf> = self
            .visible_entries()
            .iter()
            .map(|entry| entry.path.clone())
            .collect();
        let (Some(first), Some(last)) = (paths.first().cloned(), paths.last().cloned()) else {
            return;
        };
        self.selected = paths.into_iter().collect();
        self.anchor = Some(first);
        self.cursor = Some(last);
    }

    /// Empties the selection (Escape over the entries, folder change).
    pub(crate) fn clear_selection(&mut self) {
        self.selected.clear();
        self.cursor = None;
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

    /// The list's sort.
    pub(crate) fn sort(&self) -> SortSpec {
        self.sort
    }

    /// Applies a header click: sets or flips the sort. The selection and
    /// anchor are paths, so they survive the reorder.
    pub(crate) fn click_sort(&mut self, key: SortKey) {
        self.sort = self.sort.clicked(key);
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

    /// Entries with distinct sizes: displayed under size-descending the
    /// order is the reverse of the scan's name order, so any navigation that
    /// still follows listing order is observably wrong.
    fn sized_state() -> Result<ExplorerState, Box<dyn std::error::Error>> {
        let mut state = state();
        let scan = state
            .begin(Transition::Visit(dir("/fixture/a")))
            .ok_or("begin must return a scan")?;
        let mut files = Vec::new();
        for (name, size) in [("a", 10), ("b", 20), ("c", 30), ("d", 40)] {
            let mut file = entry(name, EntryKind::File);
            file.size = Some(size);
            files.push(file);
        }
        assert!(state.commit(
            scan,
            Listing {
                entries: files,
                warnings: Vec::new()
            }
        ));
        state.set_view(View::List);
        Ok(state)
    }

    fn displayed(state: &ExplorerState) -> Vec<String> {
        state
            .visible_entries()
            .iter()
            .map(|entry| entry.display_name().into_owned())
            .collect()
    }

    #[test]
    fn select_all_selects_what_is_visible_and_nothing_else() -> TestResult {
        let mut state = sized_state()?;
        state.select_all_visible();
        assert_eq!(state.selection().len(), 4);

        state.set_query("b".into());
        state.clear_selection();
        state.select_all_visible();
        let selected: Vec<_> = state.selection().iter().cloned().collect();
        assert_eq!(selected, [PathBuf::from("/fixture/b")], "the filter holds");

        // Empty folder or no match: nothing to select, nothing breaks.
        state.set_query("zzz".into());
        state.clear_selection();
        state.select_all_visible();
        assert!(state.selection().is_empty());
        Ok(())
    }

    #[test]
    fn sort_order_drives_keyboard_navigation_and_range_selection() -> TestResult {
        let mut state = sized_state()?;
        state.click_sort(SortKey::Size);
        state.click_sort(SortKey::Size);
        assert_eq!(state.sort().dir, SortDir::Descending);
        assert_eq!(displayed(&state), ["d", "c", "b", "a"]);

        // Home/Down/End select by displayed position.
        state.select_visible(0);
        assert_eq!(
            state.selection().iter().next(),
            Some(&PathBuf::from("/fixture/d")),
            "the first displayed row is the largest file"
        );
        let next = state
            .visible_position_of(Path::new("/fixture/d"))
            .map(|position| position + 1)
            .ok_or("d is visible")?;
        state.select_visible(next);
        assert_eq!(
            state.selection().iter().next(),
            Some(&PathBuf::from("/fixture/c")),
            "Down from the first displayed row lands on the second"
        );
        state.select_visible(state.visible_indices().len() - 1);
        assert_eq!(
            state.selection().iter().next(),
            Some(&PathBuf::from("/fixture/a")),
            "End is the last displayed row"
        );

        // Shift-range from the second displayed row to the fourth.
        state.select_only(Path::new("/fixture/c"));
        state.select_range_to(Path::new("/fixture/a"));
        let range: Vec<_> = state.selection().iter().cloned().collect();
        assert_eq!(
            range,
            [
                PathBuf::from("/fixture/a"),
                PathBuf::from("/fixture/b"),
                PathBuf::from("/fixture/c"),
            ],
            "c..a in displayed order covers c, b, a"
        );
        state.select_only(Path::new("/fixture/d"));
        state.select_range_to(Path::new("/fixture/b"));
        assert_eq!(
            state.selection().len(),
            3,
            "d..b in displayed order is d, c, b"
        );
        assert!(!state.selection().contains(Path::new("/fixture/a")));
        Ok(())
    }

    #[test]
    fn the_grid_keeps_the_scan_order_whatever_the_list_sort() -> TestResult {
        let mut state = sized_state()?;
        state.click_sort(SortKey::Size);
        state.click_sort(SortKey::Size);
        state.set_view(View::Grid);
        assert_eq!(displayed(&state), ["a", "b", "c", "d"]);
        assert_eq!(state.selected_size(), 0);
        state.select_only(Path::new("/fixture/b"));
        state.toggle(Path::new("/fixture/c"));
        assert_eq!(state.selected_size(), 50, "sizes of the selected files");
        Ok(())
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
    fn a_notice_shows_until_the_next_folder() -> TestResult {
        let mut state = committed()?;
        state.set_notice("Copied the path".to_owned());
        assert_eq!(state.notice(), Some("Copied the path"));

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
    /// Five files a..e in listing order, list view, name ascending.
    fn five() -> Result<ExplorerState, Box<dyn std::error::Error>> {
        let mut state = state();
        let scan = state
            .begin(Transition::Visit(dir("/fixture/a")))
            .ok_or("begin must return a scan")?;
        let names = ["a", "b", "c", "d", "e"];
        assert!(state.commit(
            scan,
            Listing {
                entries: names.iter().map(|n| entry(n, EntryKind::File)).collect(),
                warnings: Vec::new()
            }
        ));
        Ok(state)
    }

    fn selected_names(state: &ExplorerState) -> Vec<String> {
        state
            .visible_entries()
            .iter()
            .filter(|entry| state.selection().contains(&entry.path))
            .map(|entry| entry.display_name().into_owned())
            .collect()
    }

    #[test]
    fn shift_arrows_extend_from_the_anchor_and_shrink_back() -> TestResult {
        let mut state = five()?;
        state.select_visible(1); // b is anchor and cursor
        state.extend_selection_by(1);
        assert_eq!(selected_names(&state), ["b", "c"]);
        state.extend_selection_by(1);
        assert_eq!(selected_names(&state), ["b", "c", "d"]);
        state.extend_selection_by(-1);
        assert_eq!(selected_names(&state), ["b", "c"], "moving back shrinks");
        state.extend_selection_by(-2);
        assert_eq!(
            selected_names(&state),
            ["a", "b"],
            "crossing the anchor flips the side"
        );
        Ok(())
    }

    #[test]
    fn shift_extension_clamps_at_the_ends_and_supports_home_end() -> TestResult {
        let mut state = five()?;
        state.select_visible(3);
        state.extend_selection_by(10);
        assert_eq!(selected_names(&state), ["d", "e"]);
        state.extend_selection_to(0);
        assert_eq!(selected_names(&state), ["a", "b", "c", "d"], "Shift+Home");
        state.extend_selection_to(4);
        assert_eq!(selected_names(&state), ["d", "e"], "Shift+End");
        Ok(())
    }

    #[test]
    fn shift_extension_without_a_selection_selects_the_first_entry() -> TestResult {
        let mut state = five()?;
        state.extend_selection_by(1);
        assert_eq!(selected_names(&state), ["a"]);
        let mut empty = ExplorerState::new(dir("/fixture"), None);
        empty.extend_selection_by(1);
        empty.extend_selection_to(0);
        assert!(empty.selection().is_empty(), "nothing to select");
        Ok(())
    }

    #[test]
    fn shift_extension_follows_the_displayed_sort_order() -> TestResult {
        let mut state = sized_state()?;
        state.click_sort(SortKey::Size);
        state.click_sort(SortKey::Size);
        assert_eq!(displayed(&state), ["d", "c", "b", "a"]);
        state.select_visible(0);
        state.extend_selection_by(1);
        assert_eq!(selected_names(&state), ["d", "c"]);
        Ok(())
    }

    #[test]
    fn a_plain_arrow_after_a_range_moves_from_the_cursor() -> TestResult {
        let mut state = five()?;
        state.select_visible(1);
        state.extend_selection_by(2); // b..d, cursor d
        assert_eq!(
            state.cursor_position(),
            Some(3),
            "the cursor is the moving end"
        );
        Ok(())
    }

    #[test]
    fn retaining_a_selection_keeps_only_paths_that_still_exist() {
        let entries = listing(&[("a", EntryKind::File), ("c", EntryKind::File)]).entries;
        let selected: BTreeSet<PathBuf> = ["/fixture/a", "/fixture/b", "/fixture/c"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let kept = retain_existing(&selected, &entries);
        let names: Vec<_> = kept.iter().map(|path| path.to_string_lossy()).collect();
        assert_eq!(names, ["/fixture/a", "/fixture/c"]);
    }

    #[test]
    fn a_live_refresh_keeps_selection_query_and_notice_of_surviving_entries() -> TestResult {
        let mut state = state();
        let scan = state
            .begin(Transition::Visit(dir("/fixture")))
            .ok_or("scan")?;
        assert!(state.commit(
            scan,
            listing(&[
                ("a", EntryKind::File),
                ("b", EntryKind::File),
                ("c", EntryKind::File)
            ])
        ));
        state.select_only(Path::new("/fixture/b"));
        state.toggle(Path::new("/fixture/c"));
        state.set_query("b".to_owned());
        let history = state.navigation().can_back();

        let scan = state.begin_live_refresh().ok_or("a live scan")?;
        assert!(state.is_loading());
        assert!(state.commit(
            scan,
            listing(&[("a", EntryKind::File), ("b", EntryKind::File)])
        ));
        assert_eq!(
            state.selection().iter().cloned().collect::<Vec<_>>(),
            [PathBuf::from("/fixture/b")],
            "the vanished entry leaves the selection, the survivor stays"
        );
        assert_eq!(state.query(), "b", "the filter is kept");
        assert_eq!(state.navigation().can_back(), history, "no history entry");
        Ok(())
    }

    #[test]
    fn a_live_refresh_never_overlaps_a_pending_scan_or_the_trash() -> TestResult {
        let mut state = state();
        let navigating = state
            .begin(Transition::Visit(dir("/fixture/a")))
            .ok_or("scan")?;
        assert!(
            state.begin_live_refresh().is_none(),
            "a refresh must not cancel a navigation in flight"
        );
        assert!(state.commit(navigating, listing(&[])));
        let trash = state
            .begin(Transition::Visit(Location::Trash))
            .ok_or("scan")?;
        assert!(state.commit(trash, listing(&[])));
        assert!(
            state.begin_live_refresh().is_none(),
            "the Trash is not live"
        );
        Ok(())
    }

    /// Our own operation refreshes even the Trash and supersedes a scan in
    /// flight, which may have read the location before the operation ran.
    #[test]
    fn an_operation_refresh_supersedes_a_pending_refresh_and_covers_the_trash() -> TestResult {
        let mut state = state();
        let stale = state.begin_live_refresh().ok_or("a live scan")?;
        let fresh = state.begin_op_refresh().ok_or("an operation refresh")?;
        assert!(fresh.live);
        assert!(!state.commit(stale, listing(&[("old", EntryKind::File)])));
        assert!(state.commit(fresh, listing(&[("new", EntryKind::File)])));
        assert_eq!(state.listing().entries.len(), 1);

        let trash = state
            .begin(Transition::Visit(Location::Trash))
            .ok_or("scan")?;
        assert!(state.commit(trash, listing(&[("t", EntryKind::File)])));
        state.select_only(Path::new("/fixture/t"));
        let again = state.begin_op_refresh().ok_or("a Trash refresh")?;
        assert_eq!(again.location, Location::Trash);
        assert!(state.commit(again, listing(&[("t", EntryKind::File)])));
        assert_eq!(state.selection().len(), 1, "the selection survives");
        Ok(())
    }

    /// A job finishing while the user navigates must not cancel the
    /// navigation: the navigation wins and the operation's refresh is
    /// postponed until the new folder is the one shown.
    #[test]
    fn an_operation_refresh_never_drops_a_pending_navigation() -> TestResult {
        let mut state = state();
        let navigating = state
            .begin(Transition::Visit(dir("/fixture/a")))
            .ok_or("scan")?;
        assert!(
            state.begin_op_refresh().is_none(),
            "the operation refresh must not supersede a navigation"
        );
        assert!(state.is_loading(), "the navigation is still pending");
        assert!(
            state.commit(navigating, listing(&[("n", EntryKind::File)])),
            "the navigation still commits"
        );
        assert_eq!(state.current(), &dir("/fixture/a"));
        assert!(!state.navigation().can_forward());

        let after = state
            .begin_deferred_op_refresh()
            .ok_or("the postponed refresh")?;
        assert_eq!(after.location, dir("/fixture/a"), "the folder now shown");
        assert!(after.live);
        assert!(state.commit(after, listing(&[("n", EntryKind::File)])));
        assert!(
            state.begin_deferred_op_refresh().is_none(),
            "it is postponed once"
        );
        Ok(())
    }

    #[test]
    fn an_operation_refresh_never_drops_a_pending_fallback_either() -> TestResult {
        let mut state = state();
        let fallback = state
            .begin_fallback(PathBuf::from("/fixture"), "gone".to_owned())
            .ok_or("fallback")?;
        assert!(state.begin_op_refresh().is_none());
        assert!(state.commit(fallback, listing(&[])));
        assert_eq!(state.notice(), Some("gone"), "the fallback notice survives");
        assert!(state.begin_deferred_op_refresh().is_some());
        Ok(())
    }

    #[test]
    fn without_an_operation_refresh_nothing_is_postponed() -> TestResult {
        let mut state = state();
        let navigating = state
            .begin(Transition::Visit(dir("/fixture/a")))
            .ok_or("scan")?;
        assert!(state.commit(navigating, listing(&[])));
        assert!(state.begin_deferred_op_refresh().is_none());
        Ok(())
    }

    #[test]
    fn a_fallback_navigates_to_the_ancestor_and_keeps_its_notice() -> TestResult {
        let mut state = committed()?;
        state.select_only(Path::new("/fixture/notes.txt"));
        let scan = state.begin_fallback(PathBuf::from("/fixture"), "gone".to_owned());
        let scan = scan.ok_or("a fallback scan")?;
        assert!(state.commit(scan, listing(&[])));
        assert_eq!(state.current(), &dir("/fixture"));
        assert_eq!(state.notice(), Some("gone"), "the commit keeps the message");
        assert!(state.selection().is_empty(), "a folder change clears it");
        Ok(())
    }

    #[test]
    fn a_plain_reload_still_clears_the_selection() -> TestResult {
        let mut state = committed()?;
        state.select_only(Path::new("/fixture/notes.txt"));
        let scan = visit_scan(&mut state, "/fixture/a")?;
        assert!(state.commit(scan, listing(&[("notes.txt", EntryKind::File)])));
        assert!(state.selection().is_empty());
        Ok(())
    }
}
