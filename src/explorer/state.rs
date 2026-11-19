//! Pure explorer state: navigation, filtering, selection, the favorites list
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
use std::sync::{Arc, Mutex, PoisonError};

use chrono::{DateTime, Local, TimeZone};

use fileflux::core::navigation::Navigation;
use fileflux::core::{Entry, Listing, Location};

use super::arrange::{ArrangeCache, ArrangeInputs};
pub(crate) use super::arrange::{
    ArrangeCommand, ArrangeIndex, ArrangeJob, ArrangeOutput, Arrangement, FolderCounts, SortKey,
    SortSpec,
};

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

/// The favorites, shared by every tab of the window: in the order they were
/// added, unique, never the virtual Trash. They are saved in the settings
/// file by [`super::persist`]; this type only holds them.
#[derive(Clone, Debug, Default)]
pub(crate) struct Favorites(Vec<PathBuf>);

impl Favorites {
    /// The saved favorites, in their order, without repeats.
    pub(crate) fn from_paths(paths: Vec<PathBuf>) -> Self {
        let mut unique: Vec<PathBuf> = Vec::with_capacity(paths.len());
        for path in paths {
            if !unique.contains(&path) {
                unique.push(path);
            }
        }
        Self(unique)
    }

    /// The favorites, in the order they were added.
    pub(crate) fn list(&self) -> &[PathBuf] {
        &self.0
    }

    /// Whether `location` is a favorite directory.
    pub(crate) fn contains(&self, location: &Location) -> bool {
        match location {
            Location::Directory(path) => self.0.contains(path),
            Location::Trash => false,
        }
    }

    /// Adds the directory `location` or removes it again. The virtual Trash
    /// is never favorited.
    pub(crate) fn toggle(&mut self, location: &Location) {
        let Location::Directory(path) = location else {
            return;
        };
        if self.0.contains(path) {
            self.0.retain(|favorite| favorite != path);
        } else {
            self.0.push(path.clone());
        }
    }
}

/// The folders up to this many entries are arranged on the calling thread:
/// that is well under a frame even in a debug build.
pub(crate) const INLINE_ARRANGE_ENTRIES: usize = 4_096;

/// The arrangement cache behind a lock, so the read-only accessors
/// (`visible_indices` and friends, called several times per frame) can fill
/// it. It is only ever locked for one call of its own methods.
#[derive(Debug, Default)]
struct ArrangeSlot(Mutex<ArrangeCache>);

impl ArrangeSlot {
    fn with<R>(&self, work: impl FnOnce(&mut ArrangeCache) -> R) -> R {
        work(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl Clone for ArrangeSlot {
    /// A copy keeps the results but not the job in flight, whose output
    /// belongs to the original.
    fn clone(&self) -> Self {
        let mut cache = self.with(|cache| cache.clone());
        cache.cancel();
        Self(Mutex::new(cache))
    }
}

/// Selection, view, query and history for one tab, plus the
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
    /// The sort and the filters: the single source of truth for the
    /// displayed order the keyboard, range selection, grid and table all
    /// follow. Plain data, so a new tab can inherit it as it is.
    arrangement: Arrangement,
    /// The item count of each folder of the listing, counted off the UI
    /// thread after the scan; what a folder sorts by under "Size".
    child_counts: Arc<FolderCounts>,
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
    /// The scan waiting for its listing, if any.
    pending: Option<Scan>,
    /// An operation finished while a navigation was pending: its refresh
    /// runs once that navigation has committed.
    op_refresh_deferred: bool,
    /// Bumped on every scan that starts; completions compare against it.
    generation: u64,
    /// Bumped on every committed listing: which listing an arrangement
    /// result was computed for.
    listing_epoch: u64,
    /// The index and the results of arranging the listing.
    arrange: ArrangeSlot,
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
            arrangement: Arrangement::default(),
            child_counts: Arc::new(FolderCounts::new()),
            anchor: None,
            cursor: None,
            query: String::new(),
            selected: BTreeSet::new(),
            pending: None,
            op_refresh_deferred: false,
            generation: 1,
            listing_epoch: 0,
            arrange: ArrangeSlot::default(),
            home,
            load_error: None,
            path_error: None,
            notice: None,
        }
    }

    /// The state with this presentation (view, sort and filters), as saved
    /// preferences or a restored tab ask for.
    pub(crate) fn with_presentation(mut self, view: View, arrangement: Arrangement) -> Self {
        self.view = view;
        self.arrangement = arrangement;
        self
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

    /// The location `transition` would navigate to, without navigating:
    /// `None` when it would go nowhere (no history to step through, no
    /// parent).
    pub(crate) fn target_of(&self, transition: &Transition) -> Option<Location> {
        let mut candidate = self.nav.clone();
        let moved = match transition {
            Transition::Visit(location) => return Some(location.clone()),
            Transition::Back => candidate.back(),
            Transition::Forward => candidate.forward(),
            Transition::Up => candidate.up(),
        };
        moved.then(|| candidate.current().clone())
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

    /// The folder `folder` changed on disk. Returns the background re-read
    /// of the shown folder when this tab shows it and no scan is pending.
    ///
    /// A scan in flight is never cancelled. When it reads `folder` (the
    /// navigation target, or the shown folder being refreshed) the change
    /// may have landed after the listing was taken, so a re-read is
    /// postponed until the scan commits ([`Self::begin_deferred_op_refresh`]).
    pub(crate) fn folder_changed(&mut self, folder: &Path) -> Option<Scan> {
        if let Some(pending) = &self.pending {
            if matches!(&pending.location, Location::Directory(path) if path == folder) {
                self.op_refresh_deferred = true;
            }
            return None;
        }
        match self.nav.current() {
            Location::Directory(path) if path == folder => self.begin_live_refresh(),
            _ => None,
        }
    }

    /// The directory the pending scan reads, if one is in flight.
    pub(crate) fn pending_directory(&self) -> Option<&Path> {
        match &self.pending.as_ref()?.location {
            Location::Directory(path) => Some(path),
            Location::Trash => None,
        }
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
    #[cfg(test)]
    pub(crate) fn commit(&mut self, scan: Scan, listing: Listing) -> bool {
        self.commit_with_index(scan, listing, None)
    }

    /// [`Self::commit`] with the listing's [`ArrangeIndex`], built on the
    /// scan's thread so the UI thread never walks the entries. An index of
    /// another length is ignored.
    pub(crate) fn commit_indexed(
        &mut self,
        scan: Scan,
        listing: Listing,
        index: Arc<ArrangeIndex>,
    ) -> bool {
        self.commit_with_index(scan, listing, Some(index))
    }

    fn commit_with_index(
        &mut self,
        scan: Scan,
        listing: Listing,
        index: Option<Arc<ArrangeIndex>>,
    ) -> bool {
        if scan.generation != self.generation {
            return false;
        }
        self.nav = scan.candidate;
        self.listing = listing;
        self.listing_epoch += 1;
        let index = index.filter(|index| index.len() == self.listing.entries.len());
        let epoch = self.listing_epoch;
        self.arrange.with(|cache| cache.reset(epoch, index));
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

    /// Indices into [`Self::listing`] that pass the query and the filters,
    /// in the **displayed** order: grid and list both follow the sort.
    /// Keyboard navigation, range selection and the table all read this one
    /// order.
    #[cfg(test)]
    pub(crate) fn visible_indices(&self) -> Vec<usize> {
        self.visible_indices_at(&Local::now())
    }

    /// [`Self::visible_indices`] at an explicit `now`, for the date filters.
    #[cfg(test)]
    pub(crate) fn visible_indices_at<Tz>(&self, now: &DateTime<Tz>) -> Vec<usize>
    where
        Tz: TimeZone + Send + Sync + 'static,
        Tz::Offset: Send + Sync + 'static,
    {
        self.visible_shared_at(now).as_ref().clone()
    }

    /// [`Self::visible_indices`] without copying: the cached order, shared.
    /// Cheap to call every frame. With an arrangement job in flight (see
    /// [`Self::begin_arrange`]) it is the last result of this listing, and
    /// empty for a listing that has none yet; without one it is computed
    /// here when the cache is not current.
    pub(crate) fn visible_shared(&self) -> Arc<Vec<usize>> {
        self.visible_shared_at(&Local::now())
    }

    /// [`Self::visible_shared`] at an explicit `now`.
    pub(crate) fn visible_shared_at<Tz>(&self, now: &DateTime<Tz>) -> Arc<Vec<usize>>
    where
        Tz: TimeZone + Send + Sync + 'static,
        Tz::Offset: Send + Sync + 'static,
    {
        let inputs = self.arrange_inputs(now);
        self.arrange
            .with(|cache| cache.resolve(&inputs, &self.listing.entries))
    }

    fn arrange_inputs<Tz>(&self, now: &DateTime<Tz>) -> ArrangeInputs
    where
        Tz: TimeZone + Send + Sync + 'static,
        Tz::Offset: Send + Sync + 'static,
    {
        ArrangeInputs::new(
            self.listing_epoch,
            self.arrangement.clone(),
            self.query.clone(),
            Arc::clone(&self.child_counts),
            now,
        )
    }

    /// The arrangement job that makes the displayed order current, to run
    /// off the UI thread (`ArrangeJob::run`) and hand back through
    /// [`Self::commit_arrange`]; `None` when the order already is current or
    /// a job for exactly the current query, sort, filters and counts is
    /// running. Call it after every change of those, and after a commit of a
    /// listing. Until the job lands, [`Self::visible_shared`] keeps the
    /// previous order instead of computing on the calling thread.
    pub(crate) fn begin_arrange(&mut self) -> Option<ArrangeJob> {
        let inputs = self.arrange_inputs(&Local::now());
        self.arrange
            .with(|cache| cache.begin(inputs, &self.listing.entries))
    }

    /// Makes the displayed order current. A folder of up to
    /// [`INLINE_ARRANGE_ENTRIES`] entries is arranged right here (it costs
    /// less than a frame, so the grid never flashes empty) and `None` is
    /// returned; a larger one returns the job from [`Self::begin_arrange`]
    /// for the caller to run off the UI thread. Call it after a listing
    /// commit and after every change of the query, sort, filters or counts.
    pub(crate) fn refresh_arrangement(&mut self) -> Option<ArrangeJob> {
        if self.listing.entries.len() > INLINE_ARRANGE_ENTRIES {
            return self.begin_arrange();
        }
        self.cancel_arrange();
        drop(self.visible_shared());
        None
    }

    /// Settles an arrangement job. `false` (the order is untouched) when it
    /// is stale: a newer job began since, the listing was replaced, or the
    /// query, sort, filters or counts changed while it ran. After a `false`
    /// call [`Self::begin_arrange`] again.
    pub(crate) fn commit_arrange(&mut self, output: ArrangeOutput) -> bool {
        let current = self.arrange_inputs(&Local::now());
        self.arrange.with(|cache| cache.commit(output, &current))
    }

    /// Settles an arrangement job and says what to run next: commits
    /// `output` (a stale result changes nothing) and returns the job for
    /// whatever the query, sort, filters, counts or listing became while it
    /// ran, so the last change always ends up shown. `None` when the order
    /// is current.
    pub(crate) fn settle_arrange(&mut self, output: ArrangeOutput) -> Option<ArrangeJob> {
        let _landed = self.commit_arrange(output);
        self.begin_arrange()
    }

    /// An arrangement job is in flight.
    pub(crate) fn is_arranging(&self) -> bool {
        self.arrange.with(|cache| cache.is_pending())
    }

    /// Forgets the arrangement job in flight (its task was dropped), so
    /// [`Self::begin_arrange`] starts a new one.
    pub(crate) fn cancel_arrange(&mut self) {
        self.arrange.with(ArrangeCache::cancel);
    }

    /// The visible entries in displayed order; used by the UI per frame and
    /// by tests through the public API.
    pub(crate) fn visible_entries(&self) -> Vec<&Entry> {
        self.visible_shared()
            .iter()
            .filter_map(|&index| self.listing.entries.get(index))
            .collect()
    }

    /// The folder has no entries at all: a distinct state from "no matches".
    pub(crate) fn folder_is_empty(&self) -> bool {
        self.listing.entries.is_empty()
    }

    /// The name search or a filter is narrowing the listing.
    pub(crate) fn is_filtering(&self) -> bool {
        !self.query.is_empty() || !self.arrangement.filters.is_default()
    }

    /// The search or the filters hide every entry of a folder that has
    /// some; the UI shows a "no matches" state instead of "empty folder".
    pub(crate) fn is_no_result(&self) -> bool {
        self.is_filtering() && !self.is_arranging() && self.visible_shared().is_empty()
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
        let count = self.visible_shared().len();
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
        self.selected_entries()
            .into_iter()
            .filter_map(|entry| entry.size)
            .sum()
    }

    /// The selected entries of the committed listing, in listing order. The
    /// walk stops at the last one found, so an empty or small selection
    /// never reads a whole huge folder on every frame.
    pub(crate) fn selected_entries(&self) -> Vec<&Entry> {
        let wanted = self.selected.len();
        let mut found = Vec::with_capacity(wanted);
        if wanted == 0 {
            return found;
        }
        for entry in &self.listing.entries {
            if self.selected.contains(&entry.path) {
                found.push(entry);
                if found.len() == wanted {
                    break;
                }
            }
        }
        found
    }

    /// The selected entries that are on screen under the query and the
    /// filters, in the displayed order. Stops at the last one found, like
    /// [`Self::selected_entries`].
    pub(crate) fn visible_selected(&self) -> Vec<&Entry> {
        let wanted = self.selected.len();
        let mut found = Vec::with_capacity(wanted);
        if wanted == 0 {
            return found;
        }
        for &index in self.visible_shared().iter() {
            let Some(entry) = self.listing.entries.get(index) else {
                continue;
            };
            if self.selected.contains(&entry.path) {
                found.push(entry);
                if found.len() == wanted {
                    break;
                }
            }
        }
        found
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

    /// The sort.
    pub(crate) fn sort(&self) -> SortSpec {
        self.arrangement.sort
    }

    /// The sort and the filters.
    pub(crate) fn arrangement(&self) -> &Arrangement {
        &self.arrangement
    }

    /// Applies a header click: sets or flips the sort. The selection and
    /// anchor are paths, so they survive the reorder.
    pub(crate) fn click_sort(&mut self, key: SortKey) {
        self.arrangement.sort = self.arrangement.sort.clicked(key);
    }

    /// Applies a sort or filter change from a menu, chip or shortcut. The
    /// selection is untouched: a filtered-out entry stays selected.
    pub(crate) fn arrange(&mut self, command: ArrangeCommand) {
        self.arrangement.apply(command);
    }

    /// The folder item counts the size sort reads.
    pub(crate) fn child_counts(&self) -> &Arc<FolderCounts> {
        &self.child_counts
    }

    /// Publishes new folder counts; the order may change under "Size".
    pub(crate) fn set_child_counts(&mut self, counts: Arc<FolderCounts>) {
        self.child_counts = counts;
    }

    // --- tabs ---------------------------------------------------------------

    /// The state of a new tab opened on `location`: a fresh history,
    /// selection and name search, and the presentation (view, sort and
    /// filters) of this tab.
    pub(crate) fn for_new_tab(&self, location: Location) -> Self {
        Self {
            view: self.view,
            arrangement: self.arrangement.clone(),
            ..Self::new(location, self.home.clone())
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
        assert_eq!(
            state.sort().dir,
            crate::explorer::arrange::SortDir::Descending
        );
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
    fn the_grid_follows_the_same_sort_as_the_list() -> TestResult {
        let mut state = sized_state()?;
        state.click_sort(SortKey::Size);
        state.click_sort(SortKey::Size);
        state.set_view(View::Grid);
        assert_eq!(displayed(&state), ["d", "c", "b", "a"]);
        assert_eq!(state.selected_size(), 0);
        state.select_only(Path::new("/fixture/b"));
        state.toggle(Path::new("/fixture/c"));
        assert_eq!(state.selected_size(), 50, "sizes of the selected files");
        Ok(())
    }

    /// A listing of images and a note, with a dotfile and a folder, for the
    /// filter tests: `IMG_10.png`, `IMG_2.png`, `.secret.png`, `todo.txt`,
    /// `pics/`.
    fn mixed_state() -> Result<ExplorerState, Box<dyn std::error::Error>> {
        let mut state = state();
        let scan = state
            .begin(Transition::Visit(dir("/fixture/a")))
            .ok_or("begin must return a scan")?;
        let entries = [
            ("IMG_10.png", EntryKind::File),
            ("IMG_2.png", EntryKind::File),
            (".secret.png", EntryKind::File),
            ("todo.txt", EntryKind::File),
            ("pics", EntryKind::Directory),
        ]
        .iter()
        .map(|(name, kind)| entry(name, *kind))
        .collect();
        assert!(state.commit(
            scan,
            Listing {
                entries,
                warnings: Vec::new()
            }
        ));
        Ok(state)
    }

    #[test]
    fn grid_and_list_show_the_same_natural_order_and_filters() -> TestResult {
        use crate::explorer::arrange::ArrangeCommand;
        use fileflux::core::kinds::FileCategory;
        let mut state = mixed_state()?;
        for view in [View::Grid, View::List] {
            state.set_view(view);
            assert_eq!(
                displayed(&state),
                ["pics", ".secret.png", "IMG_2.png", "IMG_10.png", "todo.txt"],
                "{view:?}"
            );
        }
        state.arrange(ArrangeCommand::ToggleCategory(FileCategory::Images));
        state.arrange(ArrangeCommand::ToggleHidden);
        for view in [View::Grid, View::List] {
            state.set_view(view);
            assert_eq!(displayed(&state), ["IMG_2.png", "IMG_10.png"], "{view:?}");
        }
        assert!(state.is_filtering());
        assert!(!state.is_no_result());
        Ok(())
    }

    #[test]
    fn selection_navigation_and_activation_follow_the_filtered_result() -> TestResult {
        use crate::explorer::arrange::ArrangeCommand;
        let mut state = mixed_state()?;
        state.select_only(Path::new("/fixture/todo.txt"));
        state.arrange(ArrangeCommand::ToggleHidden);
        assert_eq!(
            displayed(&state),
            ["pics", "IMG_2.png", "IMG_10.png", "todo.txt"]
        );
        // Shift-range and Ctrl+A cover what is shown and never the hidden file.
        state.select_only(Path::new("/fixture/pics"));
        state.select_range_to(Path::new("/fixture/IMG_10.png"));
        let range: Vec<_> = state.selection().iter().cloned().collect();
        assert_eq!(
            range,
            [
                PathBuf::from("/fixture/IMG_10.png"),
                PathBuf::from("/fixture/IMG_2.png"),
                PathBuf::from("/fixture/pics"),
            ]
        );
        state.select_all_visible();
        assert_eq!(state.selection().len(), 4);
        assert!(
            !state
                .selection()
                .contains(Path::new("/fixture/.secret.png"))
        );
        // A selected entry the filter hides is kept but never activated.
        state.select_only(Path::new("/fixture/todo.txt"));
        state.arrange(ArrangeCommand::ToggleCategory(
            fileflux::core::kinds::FileCategory::Images,
        ));
        assert_eq!(state.selection().len(), 1, "filtering does not deselect");
        assert_eq!(state.activation_candidate(), None);
        // Keyboard steps and the viewer's image list use the same order.
        state.select_visible(0);
        assert_eq!(
            state.selection().iter().next(),
            Some(&PathBuf::from("/fixture/IMG_2.png"))
        );
        let shown = state.visible_entries();
        let target = crate::explorer::ops::open_target(Path::new("/fixture/IMG_10.png"), &shown);
        assert_eq!(
            target,
            crate::explorer::ops::OpenTarget::Viewer {
                list: vec![
                    PathBuf::from("/fixture/IMG_2.png"),
                    PathBuf::from("/fixture/IMG_10.png")
                ],
                index: 1
            }
        );
        Ok(())
    }

    #[test]
    fn filters_that_hide_everything_are_a_no_result_not_an_empty_folder() -> TestResult {
        use crate::explorer::arrange::{ArrangeCommand, SizeRange};
        let mut state = mixed_state()?;
        assert!(!state.is_filtering());
        state.arrange(ArrangeCommand::ToggleSize(SizeRange::Over1Gb));
        assert_eq!(displayed(&state), ["pics"], "folders ignore byte sizes");
        state.arrange(ArrangeCommand::ToggleCategory(
            fileflux::core::kinds::FileCategory::Audio,
        ));
        assert!(state.visible_indices().is_empty());
        assert!(state.is_no_result());
        assert!(!state.folder_is_empty());
        state.arrange(ArrangeCommand::ClearFilters);
        assert!(!state.is_filtering());
        assert_eq!(state.visible_indices().len(), 5);
        Ok(())
    }

    #[test]
    fn folder_counts_arriving_later_reorder_the_size_sort() -> TestResult {
        let mut state = mixed_state()?;
        state.click_sort(SortKey::Size);
        let before = displayed(&state);
        assert_eq!(before.first().map(String::as_str), Some("pics"));
        let mut counts = FolderCounts::new();
        counts.insert(PathBuf::from("/fixture/pics"), Some(99));
        state.set_child_counts(Arc::new(counts));
        assert_eq!(
            displayed(&state).first().map(String::as_str),
            Some("pics"),
            "folders stay first"
        );
        state.arrange(ArrangeCommand::ToggleFoldersFirst);
        assert_eq!(
            displayed(&state).last().map(String::as_str),
            Some("pics"),
            "99 items outsize the 10-byte files once folders are mixed in"
        );
        Ok(())
    }

    #[test]
    fn a_new_tab_inherits_sort_and_filters_then_diverges() -> TestResult {
        use crate::explorer::arrange::{ArrangeCommand, DateRange};
        let mut source = mixed_state()?;
        source.click_sort(SortKey::Modified);
        source.arrange(ArrangeCommand::ToggleHidden);
        source.arrange(ArrangeCommand::ToggleDate(DateRange::Older));
        let mut fresh = source.for_new_tab(dir("/fixture/a"));
        assert_eq!(fresh.arrangement(), source.arrangement());
        fresh.arrange(ArrangeCommand::ClearFilters);
        fresh.click_sort(SortKey::Size);
        assert_ne!(fresh.arrangement(), source.arrangement());
        assert!(
            source.arrangement().filters.hide_hidden,
            "the source keeps its own"
        );
        assert_eq!(source.sort().key, SortKey::Modified);
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
    fn favorites_are_unique_and_never_the_trash() {
        let mut favorites = Favorites::default();
        assert!(favorites.list().is_empty());
        assert!(!favorites.contains(&dir("/fixture/a")));

        favorites.toggle(&dir("/fixture/a"));
        assert_eq!(favorites.list(), [PathBuf::from("/fixture/a")]);
        assert!(favorites.contains(&dir("/fixture/a")));

        // Re-adding is a remove: the toggle is one button.
        favorites.toggle(&dir("/fixture/a"));
        assert!(favorites.list().is_empty());

        // Toggling another directory keeps exactly one entry per directory.
        favorites.toggle(&dir("/fixture/b"));
        assert_eq!(favorites.list(), [PathBuf::from("/fixture/b")]);
        favorites.toggle(&dir("/fixture/b"));
        assert!(favorites.list().is_empty());
        favorites.toggle(&dir("/fixture/b"));
        assert_eq!(favorites.list(), [PathBuf::from("/fixture/b")]);

        // The virtual Trash is never favorited.
        favorites.toggle(&Location::Trash);
        assert_eq!(
            favorites.list(),
            [PathBuf::from("/fixture/b")],
            "the Trash toggle is a no-op and keeps prior favorites"
        );
        assert!(!favorites.contains(&Location::Trash));
    }

    #[test]
    fn a_new_tab_starts_on_the_current_folder_with_a_fresh_history() -> TestResult {
        let mut state = committed()?;
        let scan = visit_scan(&mut state, "/fixture/b")?;
        state.commit(scan, listing(&[("x", EntryKind::File)]));
        state.set_view(View::List);
        state.click_sort(SortKey::Size);
        state.select_only(Path::new("/fixture/x"));
        state.set_query("x".to_owned());

        let fresh = state.for_new_tab(dir("/fixture/b"));
        assert_eq!(fresh.current(), &dir("/fixture/b"));
        assert!(!fresh.navigation().can_back(), "no inherited history");
        assert!(!fresh.navigation().can_forward());
        assert!(fresh.selection().is_empty());
        assert_eq!(fresh.query(), "");
        assert_eq!(fresh.view(), View::List, "the presentation carries over");
        assert_eq!(fresh.sort(), state.sort());

        // The source tab keeps what it had.
        assert!(state.navigation().can_back());
        assert_eq!(state.selection().len(), 1);
        assert_eq!(state.query(), "x");
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

    #[test]
    fn a_change_in_the_shown_folder_refreshes_it_and_other_folders_do_not() -> TestResult {
        let mut state = state();
        assert!(state.folder_changed(Path::new("/elsewhere")).is_none());
        let scan = state
            .folder_changed(Path::new("/fixture"))
            .ok_or("a refresh of the shown folder")?;
        assert!(scan.live);
        assert_eq!(scan.location, dir("/fixture"));
        Ok(())
    }

    /// A tab navigating into X also watches X: a change that lands before
    /// the scan commits would otherwise be read by neither the scan (which
    /// may already have listed X) nor a refresh.
    #[test]
    fn a_change_in_the_folder_a_tab_is_navigating_into_refreshes_it_after_the_commit() -> TestResult
    {
        let mut state = state();
        let navigating = state
            .begin(Transition::Visit(dir("/fixture/x")))
            .ok_or("scan")?;
        assert_eq!(state.pending_directory(), Some(Path::new("/fixture/x")));
        assert!(
            state.folder_changed(Path::new("/fixture/x")).is_none(),
            "the navigation in flight is not cancelled"
        );
        assert!(state.is_loading(), "still navigating");
        assert!(state.commit(navigating, listing(&[("old", EntryKind::File)])));

        let refresh = state
            .begin_deferred_op_refresh()
            .ok_or("a re-read of X after it committed")?;
        assert_eq!(refresh.location, dir("/fixture/x"));
        assert!(refresh.live, "the refresh keeps selection and filter");
        assert!(state.commit(
            refresh,
            listing(&[("old", EntryKind::File), ("new", EntryKind::File)])
        ));
        assert_eq!(state.listing().entries.len(), 2, "the change shows");
        Ok(())
    }

    #[test]
    fn a_change_elsewhere_while_navigating_refreshes_nothing() -> TestResult {
        let mut state = state();
        let navigating = state
            .begin(Transition::Visit(dir("/fixture/x")))
            .ok_or("scan")?;
        // The folder being left changes: it is about to be replaced anyway.
        assert!(state.folder_changed(Path::new("/fixture")).is_none());
        assert!(state.folder_changed(Path::new("/other")).is_none());
        assert!(state.commit(navigating, listing(&[])));
        assert!(state.begin_deferred_op_refresh().is_none());
        Ok(())
    }

    #[test]
    fn a_change_during_a_live_refresh_is_read_again_after_it_commits() -> TestResult {
        let mut state = state();
        let first = state
            .folder_changed(Path::new("/fixture"))
            .ok_or("a refresh")?;
        assert!(state.folder_changed(Path::new("/fixture")).is_none());
        assert!(state.commit(first, listing(&[])));
        let again = state
            .begin_deferred_op_refresh()
            .ok_or("the change that landed mid-scan")?;
        assert_eq!(again.location, dir("/fixture"));
        Ok(())
    }

    #[test]
    fn the_trash_is_never_refreshed_by_a_folder_change() -> TestResult {
        let mut state = state();
        let trash = state
            .begin(Transition::Visit(Location::Trash))
            .ok_or("scan")?;
        assert_eq!(state.pending_directory(), None, "the Trash is no directory");
        assert!(state.commit(trash, listing(&[])));
        assert!(state.folder_changed(Path::new("/fixture")).is_none());
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
    fn the_target_of_a_transition_is_known_without_navigating() -> TestResult {
        let mut state = state();
        assert_eq!(
            state.target_of(&Transition::Visit(dir("/elsewhere"))),
            Some(dir("/elsewhere"))
        );
        assert_eq!(state.target_of(&Transition::Back), None);
        let scan = visit_scan(&mut state, "/fixture/a")?;
        assert!(state.commit(scan, listing(&[])));
        assert_eq!(state.target_of(&Transition::Back), Some(dir("/fixture")));
        assert_eq!(state.target_of(&Transition::Up), Some(dir("/fixture")));
        assert_eq!(state.target_of(&Transition::Forward), None);
        assert_eq!(state.current(), &dir("/fixture/a"), "nothing navigated");
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

    // --- off-thread arrangement ---------------------------------------------

    fn shown_names(state: &ExplorerState) -> Vec<String> {
        state
            .visible_shared()
            .iter()
            .filter_map(|&index| state.listing().entries.get(index))
            .map(|entry| entry.name.to_string_lossy().into_owned())
            .collect()
    }

    fn arrange_fixture() -> Listing {
        listing(&[
            ("alpha.txt", EntryKind::File),
            ("beta.md", EntryKind::File),
            ("Notes", EntryKind::Directory),
            ("alphabet.rs", EntryKind::File),
        ])
    }

    /// A committed explorer over [`arrange_fixture`] whose arrangement is
    /// current, with the index built the way the scan thread does.
    fn arranging() -> Result<ExplorerState, Box<dyn std::error::Error>> {
        let mut state = state();
        let scan = visit_scan(&mut state, "/fixture/a")?;
        let fixture = arrange_fixture();
        let index = Arc::new(ArrangeIndex::build(&fixture.entries));
        assert!(state.commit_indexed(scan, fixture, index));
        let job = state.begin_arrange().ok_or("a job for the new listing")?;
        assert!(state.is_arranging());
        assert!(state.commit_arrange(job.run()));
        assert!(!state.is_arranging());
        Ok(state)
    }

    #[test]
    fn an_arrangement_job_replaces_the_order_only_when_it_lands() -> TestResult {
        let mut state = arranging()?;
        assert_eq!(
            shown_names(&state),
            ["Notes", "alpha.txt", "alphabet.rs", "beta.md"]
        );
        state.set_query("alpha".into());
        let job = state.begin_arrange().ok_or("a job for the new query")?;
        assert_eq!(
            shown_names(&state).len(),
            4,
            "the previous order stays while the job runs"
        );
        assert!(state.begin_arrange().is_none(), "one job per input");
        assert!(state.commit_arrange(job.run()));
        assert_eq!(shown_names(&state), ["alpha.txt", "alphabet.rs"]);
        assert_eq!(
            state.visible_indices(),
            *state.visible_shared(),
            "both accessors agree"
        );
        assert!(state.begin_arrange().is_none(), "the order is current");
        Ok(())
    }

    #[test]
    fn a_stale_arrangement_job_is_dropped_and_never_shown() -> TestResult {
        let mut state = arranging()?;
        state.set_query("a".into());
        let slow = state.begin_arrange().ok_or("a job")?;
        state.set_query("alphab".into());
        let fast = state.begin_arrange().ok_or("a newer job")?;
        assert!(state.commit_arrange(fast.run()));
        assert!(!state.commit_arrange(slow.run()), "the older job is stale");
        assert_eq!(shown_names(&state), ["alphabet.rs"]);
        Ok(())
    }

    #[test]
    fn a_job_for_inputs_that_changed_meanwhile_is_dropped_and_restarted() -> TestResult {
        let mut state = arranging()?;
        state.set_query("beta".into());
        let job = state.begin_arrange().ok_or("a job")?;
        state.arrange(ArrangeCommand::SortBy(SortKey::Size));
        assert!(!state.commit_arrange(job.run()), "the sort changed");
        assert!(!state.is_arranging());
        let job = state
            .begin_arrange()
            .ok_or("a job for the current inputs")?;
        assert!(state.commit_arrange(job.run()));
        assert_eq!(shown_names(&state), ["beta.md"]);
        Ok(())
    }

    #[test]
    fn a_job_of_the_previous_listing_is_dropped_by_the_next_commit() -> TestResult {
        let mut state = arranging()?;
        state.set_query("a".into());
        let old = state.begin_arrange().ok_or("a job")?;
        let scan = visit_scan(&mut state, "/fixture/b")?;
        assert!(state.commit(scan, listing(&[("other.txt", EntryKind::File)])));
        let new = state.begin_arrange().ok_or("a job for the new listing")?;
        assert!(!state.commit_arrange(old.run()), "another listing");
        assert!(
            state.visible_shared().is_empty(),
            "a fresh listing never shows the old one's indices"
        );
        assert!(state.commit_arrange(new.run()));
        assert_eq!(shown_names(&state), ["other.txt"]);
        Ok(())
    }

    #[test]
    fn a_fresh_listing_without_a_job_is_arranged_in_place() -> TestResult {
        // Callers that never begin a job (tests, small folders) still get
        // the answer from the same accessor.
        let mut state = state();
        let scan = visit_scan(&mut state, "/fixture/a")?;
        assert!(state.commit(scan, arrange_fixture()));
        state.set_query("BETA".into());
        assert_eq!(shown_names(&state), ["beta.md"]);
        state.arrange(ArrangeCommand::ToggleFoldersFirst);
        state.clear_query();
        assert_eq!(
            shown_names(&state),
            ["alpha.txt", "alphabet.rs", "beta.md", "Notes"]
        );
        Ok(())
    }

    #[test]
    fn an_index_of_another_listing_is_ignored() -> TestResult {
        let mut state = state();
        let scan = visit_scan(&mut state, "/fixture/a")?;
        let wrong = Arc::new(ArrangeIndex::build(
            &listing(&[("x", EntryKind::File)]).entries,
        ));
        assert!(state.commit_indexed(scan, arrange_fixture(), wrong));
        assert_eq!(shown_names(&state).len(), 4);
        Ok(())
    }

    #[test]
    fn a_cancelled_job_lets_the_same_inputs_start_again() -> TestResult {
        let mut state = arranging()?;
        state.set_query("beta".into());
        let _dropped = state.begin_arrange().ok_or("a job")?;
        assert!(state.begin_arrange().is_none());
        state.cancel_arrange();
        assert!(!state.is_arranging());
        assert!(state.begin_arrange().is_some());
        Ok(())
    }

    #[test]
    fn a_cloned_state_does_not_inherit_the_job_in_flight() -> TestResult {
        let mut state = arranging()?;
        state.set_query("beta".into());
        let _running = state.begin_arrange().ok_or("a job")?;
        let copy = state.clone();
        assert!(state.is_arranging());
        assert!(!copy.is_arranging());
        assert_eq!(shown_names(&copy), ["beta.md"], "the copy computes its own");
        Ok(())
    }

    #[test]
    fn small_folders_are_arranged_in_place_and_large_ones_get_a_job() -> TestResult {
        let mut small = state();
        let scan = visit_scan(&mut small, "/fixture/a")?;
        assert!(small.commit(scan, arrange_fixture()));
        assert!(small.refresh_arrangement().is_none(), "no job to wait for");
        assert!(!small.is_arranging());
        assert_eq!(shown_names(&small).len(), 4);

        let mut large = state();
        let scan = visit_scan(&mut large, "/fixture/a")?;
        let names: Vec<String> = (0..=INLINE_ARRANGE_ENTRIES)
            .map(|serial| format!("file{serial}.txt"))
            .collect();
        let entries: Vec<(&str, EntryKind)> = names
            .iter()
            .map(|name| (name.as_str(), EntryKind::File))
            .collect();
        assert!(large.commit(scan, listing(&entries)));
        let job = large
            .refresh_arrangement()
            .ok_or("a job for a large folder")?;
        assert!(large.is_arranging());
        assert!(
            large.visible_shared().is_empty(),
            "nothing yet, nothing computed here"
        );
        assert!(large.commit_arrange(job.run()));
        assert_eq!(large.visible_shared().len(), names.len());
        assert!(large.refresh_arrangement().is_none(), "already current");
        Ok(())
    }

    #[test]
    fn settling_a_stale_job_starts_the_job_for_the_latest_inputs() -> TestResult {
        let mut state = arranging()?;
        state.set_query("a".into());
        let first = state.begin_arrange().ok_or("a job")?;
        state.set_query("alphab".into());
        let next = state
            .settle_arrange(first.run())
            .ok_or("the stale result starts the next job")?;
        assert_eq!(
            shown_names(&state).len(),
            4,
            "the stale result is not shown"
        );
        assert!(state.is_arranging());
        assert!(state.settle_arrange(next.run()).is_none(), "now current");
        assert_eq!(shown_names(&state), ["alphabet.rs"]);
        assert!(!state.is_arranging());
        Ok(())
    }

    #[test]
    fn typing_during_a_running_job_ends_with_the_latest_query() -> TestResult {
        let mut state = arranging()?;
        let mut running = None;
        for query in ["b", "be", "bet", "beta"] {
            state.set_query(query.into());
            // The view asks for a job after each keystroke; only a changed
            // input yields one, and the newest replaces the running one.
            if let Some(job) = state.begin_arrange() {
                running = Some(job);
            }
        }
        let mut job = running.ok_or("a job")?;
        while let Some(next) = state.settle_arrange(job.run()) {
            job = next;
        }
        assert_eq!(shown_names(&state), ["beta.md"]);
        assert!(!state.is_arranging());
        Ok(())
    }

    #[test]
    fn selection_and_keyboard_follow_the_last_shown_order_while_a_job_runs() -> TestResult {
        let mut state = arranging()?;
        state.select_visible(1);
        assert_eq!(state.cursor_position(), Some(1));
        state.set_query("beta".into());
        let job = state.begin_arrange().ok_or("a job")?;
        assert_eq!(
            shown_names(&state),
            ["Notes", "alpha.txt", "alphabet.rs", "beta.md"],
            "the previous order is still the displayed one"
        );
        state.extend_selection_by(1);
        assert_eq!(state.cursor_position(), Some(2), "moves in the shown order");
        assert!(state.settle_arrange(job.run()).is_none());
        assert_eq!(shown_names(&state), ["beta.md"]);
        Ok(())
    }

    #[test]
    fn a_pending_job_is_not_a_no_result_state() -> TestResult {
        let mut state = state();
        let scan = visit_scan(&mut state, "/fixture/a")?;
        let names: Vec<String> = (0..=INLINE_ARRANGE_ENTRIES)
            .map(|serial| format!("file{serial}.txt"))
            .collect();
        let entries: Vec<(&str, EntryKind)> = names
            .iter()
            .map(|name| (name.as_str(), EntryKind::File))
            .collect();
        assert!(state.commit(scan, listing(&entries)));
        state.arrange(ArrangeCommand::ToggleHidden);
        let job = state.refresh_arrangement().ok_or("a job")?;
        assert!(state.is_filtering());
        assert!(!state.is_no_result(), "still sorting, not empty-handed");
        assert!(state.settle_arrange(job.run()).is_none());
        Ok(())
    }

    #[test]
    fn selected_entries_follow_listing_order_and_visible_ones_the_displayed_order() -> TestResult {
        let mut state = sized_state()?;
        state.click_sort(SortKey::Size);
        state.click_sort(SortKey::Size);
        assert!(state.selected_entries().is_empty(), "nothing is selected");
        assert!(state.visible_selected().is_empty());
        state.select_only(Path::new("/fixture/b"));
        state.toggle(Path::new("/fixture/c"));
        let names = |entries: Vec<&Entry>| -> Vec<String> {
            entries
                .into_iter()
                .map(|entry| entry.name.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(names(state.selected_entries()), ["b", "c"], "listing order");
        assert_eq!(
            names(state.visible_selected()),
            ["c", "b"],
            "displayed order"
        );
        state.set_query("b".into());
        assert_eq!(names(state.selected_entries()), ["b", "c"], "hidden stays");
        assert_eq!(names(state.visible_selected()), ["b"], "only what is shown");
        Ok(())
    }
}
