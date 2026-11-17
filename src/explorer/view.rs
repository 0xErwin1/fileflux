//! The native explorer view: the whole window's layout, the async navigation
//! guard and the keyboard vocabulary.
//!
//! Layout follows the reference design (design/file-explorer/01, translated
//! from design/system.md): the desk with a native title bar, 8 px gutters, a
//! 290 px sidebar island and a document island, and a 38 px status bar. The
//! window title shows the current folder plus Fileflux.
//!
//! Directory scans run off the UI thread (see [`load_location`]); every scan
//! carries a generation from [`ExplorerState::begin`], and only a completion
//! matching the newest generation settles the state, so stale or failed
//! scans never corrupt the committed location, history, selection or query.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bolt::composites::Island;
use bolt::controls::{InputEvent, InputState};
use bolt::tokens::{ChromeColors, IslandMetrics, Spacing};
use fileflux::core::places::{Places, discover};
use fileflux::core::{Listing, Location};
use gpui::{
    App, AppContext as _, Context, Div, Entity, FocusHandle, Focusable, InteractiveElement,
    KeyBinding, ParentElement, Render, ScrollHandle, ScrollStrategy, Stateful, Styled,
    Subscription, Task, UniformListScrollHandle, Window, actions, div, prelude::*, px,
};

use gpui_component::ActiveTheme as _;

use crate::explorer::state::{ExplorerState, PathArgument, Scan, Transition, View};
use crate::explorer::{entries, sidebar, toolbar, util};

/// Key context of the whole explorer window.
pub(crate) const EXPLORER_CONTEXT: &str = "FilefluxExplorer";
/// Key context of the entries area, which carries the arrow-key navigation.
pub(crate) const ENTRIES_CONTEXT: &str = "FilefluxEntries";

actions!(
    fileflux_explorer,
    [
        /// Focuses the path field and selects its content.
        FocusPath,
        /// Focuses the filename search field and selects its content.
        FocusSearch,
        /// Restores the path field or clears the search, and gives the
        /// focus back to the shell.
        CancelEdit,
        /// One step back in the history.
        NavigateBack,
        /// One step forward in the history.
        NavigateForward,
        /// The parent of the current directory.
        NavigateUp,
        /// Re-reads the current location from the filesystem.
        Reload,
    ]
);

actions!(
    fileflux_entries,
    [
        /// Selects the previous visible entry.
        SelectUp,
        /// Selects the next visible entry.
        SelectDown,
        /// Selects one tile to the left in the grid.
        SelectLeft,
        /// Selects one tile to the right in the grid.
        SelectRight,
        /// Selects the first visible entry.
        SelectFirstEntry,
        /// Selects the last visible entry.
        SelectLastEntry,
        /// Opens the selected directory; other entries answer honestly
        /// read-only.
        OpenSelected,
    ]
);

/// Binds the explorer's keyboard vocabulary.
pub(crate) fn bind_keys(cx: &mut App) {
    let shell = Some(EXPLORER_CONTEXT);
    let entries = Some(ENTRIES_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("ctrl-l", FocusPath, shell),
        KeyBinding::new("ctrl-f", FocusSearch, shell),
        KeyBinding::new("escape", CancelEdit, shell),
        KeyBinding::new("alt-left", NavigateBack, shell),
        KeyBinding::new("alt-right", NavigateForward, shell),
        KeyBinding::new("alt-up", NavigateUp, shell),
        KeyBinding::new("ctrl-r", Reload, shell),
        KeyBinding::new("up", SelectUp, entries),
        KeyBinding::new("down", SelectDown, entries),
        KeyBinding::new("left", SelectLeft, entries),
        KeyBinding::new("right", SelectRight, entries),
        KeyBinding::new("home", SelectFirstEntry, entries),
        KeyBinding::new("end", SelectLastEntry, entries),
        KeyBinding::new("enter", OpenSelected, entries),
    ]);
}

/// Reads one location off the UI thread: a plain directory scan, or the
/// home Trash listing. Recorded original Trash paths are never opened.
fn load_location(location: &Location, trash_root: Option<&Path>) -> io::Result<Listing> {
    match location {
        Location::Directory(path) => fileflux::core::fs::read_directory(path),
        Location::Trash => match trash_root {
            Some(root) => fileflux::core::trash::read_trash(root),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Home Trash is not discovered yet",
            )),
        },
    }
}

/// Sidebar width of the reference design, in CSS pixels.
pub(crate) const SIDEBAR_WIDTH: f32 = 290.0;
/// Below this viewport width the sidebar parks behind the sidebar toggle,
/// so path, search and view stay reachable.
pub(crate) const NARROW_WIDTH: f32 = 760.0;

/// The explorer view. All filesystem state lives in [`ExplorerState`]; this
/// struct only adds the session UI: inputs, focus handles, the scroll
/// anchor and the background tasks.
pub(crate) struct Explorer {
    /// The committed explorer state, guarded by generations.
    pub(crate) state: ExplorerState,
    /// A clone of the committed listing for the virtualized rows, replaced
    /// on every commit; the render closures read this instead of scanning
    /// the directory again.
    pub(crate) snapshot: Arc<Vec<fileflux::core::Entry>>,
    /// Sidebar discovery, arriving off the UI thread at startup.
    pub(crate) places: Option<Places>,
    /// Why the sidebar could not be discovered, when it could not.
    pub(crate) sidebar_error: Option<String>,
    /// The home Trash root once places are known.
    pub(crate) trash_root: Option<PathBuf>,
    /// The editable path field.
    pub(crate) path_input: Entity<InputState>,
    /// The text the path field was last seeded with; an unedited submit
    /// retains the committed location even for non-UTF8 names.
    pub(crate) path_seed: String,
    /// Whether the path field holds an edit that has not been submitted.
    pub(crate) path_edited: bool,
    /// The path field's edit revision: bumped by every user edit, never by
    /// programmatic reseeds (they emit no Change event).
    path_edit_revision: u64,
    /// The edited-path submit in flight, if any: its scan generation plus
    /// the edit revision at submit time.
    submitted_edit: Option<SubmittedEdit>,
    /// The current-folder filename search field.
    pub(crate) search_input: Entity<InputState>,
    /// The whole-window focus, so Escape has somewhere to give the focus
    /// back and the shell keys work after a click on bare chrome.
    pub(crate) shell_focus: FocusHandle,
    /// The entries area focus, which carries the arrow keys.
    pub(crate) entries_focus: FocusHandle,
    /// The vertical scroll handle of the details list. The two
    /// presentations count rows in different units — entries versus rows of
    /// tiles — so each keeps its own handle and its own list element id.
    pub(crate) list_scroll: UniformListScrollHandle,
    /// The vertical scroll handle of the tile grid.
    pub(crate) grid_scroll: UniformListScrollHandle,
    /// The horizontal scroll handle of the details-list viewport, which
    /// Bolt's `GridOverflow` drives for header and rows together.
    pub(crate) list_hscroll: ScrollHandle,
    /// The entries area's measured inner width, recorded by the draw-nothing
    /// measuring canvas every frame; `None` until the first layout reached
    /// it, so the first frame draws with the estimate below.
    pub(crate) entries_width: Option<f32>,
    /// The retained scan task, replaced by the next scan.
    scan_task: Option<Task<()>>,
    /// The path field's Enter/Change/Blur subscription.
    _path_events: Option<Subscription>,
    /// The search field's Change subscription.
    _search_events: Option<Subscription>,
    /// Whether the sidebar is shown; narrow windows park it behind the
    /// sidebar toggle.
    pub(crate) sidebar_open: bool,
    /// The grid column count of the last render, so keyboard row steps use
    /// the same geometry the list draws with.
    pub(crate) rendered_columns: usize,
}

impl Focusable for Explorer {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.shell_focus.clone()
    }
}

impl Explorer {
    /// The focus the window gives the explorer when it opens, so the entry
    /// keyboard works at once.
    pub(crate) fn initial_focus(&self) -> FocusHandle {
        self.entries_focus.clone()
    }

    /// Builds the explorer: the first scan of the home (or the honest root
    /// fallback when `$HOME` is missing) and the places discovery both
    /// start off the UI thread right away.
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let initial = home
            .clone()
            .map(Location::Directory)
            .unwrap_or_else(|| Location::Directory(PathBuf::from("/")));
        let state = ExplorerState::new(initial.clone(), home);

        let path_input = cx.new(|cx| InputState::new(window, cx));
        let search_input = cx.new(|cx| {
            let mut input = InputState::new(window, cx);
            input.set_placeholder("Search this folder…", window, cx);
            input
        });

        let mut this = Self {
            state,
            snapshot: Arc::new(Vec::new()),
            places: None,
            sidebar_error: None,
            trash_root: None,
            path_input,
            path_seed: String::new(),
            path_edited: false,
            path_edit_revision: 0,
            submitted_edit: None,
            search_input,
            shell_focus: cx.focus_handle(),
            entries_focus: cx.focus_handle(),
            list_scroll: UniformListScrollHandle::new(),
            grid_scroll: UniformListScrollHandle::new(),
            list_hscroll: ScrollHandle::new(),
            entries_width: None,
            scan_task: None,
            sidebar_open: true,
            rendered_columns: 1,
            _path_events: None,
            _search_events: None,
        };

        // The path field: Enter submits, typing clears the path error, and
        // blur restores the committed path (the same as Escape).
        let path_entity = this.path_input.clone();
        this._path_events = Some(cx.subscribe_in(
            &path_entity,
            window,
            |this, _input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.submit_path(window, cx),
                InputEvent::Change => {
                    let text = this.path_input.read(cx).value().to_string();
                    this.path_edit_revision += 1;
                    this.path_edited = text != this.path_seed;
                    this.state.clear_path_error();
                    cx.notify();
                }
                InputEvent::Blur => {
                    if this.path_edited {
                        this.seed_path_input(window, cx, false);
                        cx.notify();
                    }
                }
                InputEvent::Focus => {}
            },
        ));

        // The search field filters the current folder on every keystroke.
        let search_entity = this.search_input.clone();
        this._search_events = Some(cx.subscribe_in(
            &search_entity,
            window,
            |this, _input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    let text = this.search_input.read(cx).value().to_string();
                    this.state.set_query(text);
                    cx.notify();
                }
            },
        ));

        // The sidebar discovery reads the user-dirs configuration and the
        // mount table: blocking work that belongs off the UI thread.
        let discover_task = cx.background_executor().spawn(async move { discover() });
        cx.spawn_in(window, async move |this, cx| {
            let discovered = discover_task.await;
            if let Err(error) = this.update_in(cx, |this, _window, cx| {
                this.finish_discover(discovered, cx);
            }) {
                log::debug!("the explorer closed before the places arrived: {error}");
            }
        })
        .detach();

        this.sync_window_title(window);
        this.navigate(Transition::Visit(initial), window, cx);
        this
    }

    /// The current-location label, as the window title and the content head
    /// show it: `Home` for the home directory, `Home Trash` for the home
    /// Trash, else the folder name (or `/` for the filesystem root).
    pub(crate) fn current_label(&self) -> String {
        match self.state.current() {
            Location::Trash => "Home Trash".to_owned(),
            Location::Directory(path) => {
                if self
                    .places
                    .as_ref()
                    .is_some_and(|places| *path == places.home)
                {
                    "Home".to_owned()
                } else {
                    path.file_name().map_or_else(
                        || path.to_string_lossy().into_owned(),
                        |name| name.to_string_lossy().into_owned(),
                    )
                }
            }
        }
    }

    /// Navigates: turns the transition into a scan and starts it off the UI
    /// thread. A transition that changes nothing produces no scan. Returns
    /// the started scan, so a path submit can remember it.
    pub(crate) fn navigate(
        &mut self,
        transition: Transition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Scan> {
        let scan = self.state.begin(transition);
        self.start_scan(scan.clone(), window, cx);
        scan
    }

    /// Starts the scan (risk item 4): the blocking read runs on the
    /// background executor, the completion re-enters the view through
    /// [`Self::finish_scan`], and the task is retained so a newer scan
    /// replaces it.
    fn start_scan(&mut self, scan: Option<Scan>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(scan) = scan else {
            return;
        };
        let location = scan.location.clone();
        let trash_root = self.trash_root.clone();
        let load = cx
            .background_executor()
            .spawn(async move { load_location(&location, trash_root.as_deref()) });
        self.scan_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = load.await;
            if let Err(error) = this.update_in(cx, |this, window, cx| {
                this.finish_scan(scan, result, window, cx);
            }) {
                log::debug!("the explorer closed before its scan settled: {error}");
            }
        }));
        cx.notify();
    }

    /// Settles a finished scan. A stale result (a newer scan has started,
    /// including a newer failed one) changes nothing. A commit re-seeds the
    /// path field and the window title; a failure only reports.
    fn finish_scan(
        &mut self,
        scan: Scan,
        result: io::Result<Listing>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(listing) => {
                if !self.state.commit(scan.clone(), listing) {
                    return;
                }
                // A genuine folder change returns the reader to the top of
                // the presentation that is showing; a reload keeps the
                // reader at the place they were reading.
                if !scan.refresh {
                    self.active_scroll_handle()
                        .scroll_to_item(0, ScrollStrategy::Top);
                }
                self.refresh_snapshot();
                // Consume the submitted edit only when this completion is
                // its own scan with no newer edit after it; stale or failed
                // completions never reseed either field.
                let revision = self.path_edit_revision;
                let settled = self
                    .submitted_edit
                    .take()
                    .is_some_and(|edit| edit.settles(scan.generation, revision));
                self.seed_path_input(window, cx, settled);
                self.sync_search_input(window, cx);
                cx.notify();
            }
            Err(error) => {
                self.submitted_edit = None;
                if !self.state.commit_failure(scan, error.to_string()) {
                    return;
                }
                log::warn!("directory scan failed: {error}");
                cx.notify();
            }
        }
        self.sync_window_title(window);
    }

    /// Applies the sidebar discovery. A discovery failure is honest and
    /// recoverable: the sidebar explains it and the explorer stays on the
    /// root fallback instead of panicking.
    fn finish_discover(&mut self, discovered: io::Result<Places>, cx: &mut Context<Self>) {
        match discovered {
            Ok(places) => {
                self.trash_root = Some(places.trash_root.clone());
                self.sidebar_error = None;
                self.places = Some(places);
            }
            Err(error) => {
                self.sidebar_error = Some(error.to_string());
            }
        }
        cx.notify();
    }

    /// Rebuilds the entry snapshot the virtualized rows read.
    fn refresh_snapshot(&mut self) {
        self.snapshot = Arc::new(self.state.listing().entries.clone());
    }

    /// Seeds the path field with the committed location unless the user is
    /// mid-edit. Unedited submits therefore retain the actual current path.
    /// `force` reseeds even while focused, for a just-settled submission,
    /// so a second Enter reloads the displayed location; post-submission
    /// edits keep the guard.
    fn seed_path_input(&mut self, window: &mut Window, cx: &mut Context<Self>, force: bool) {
        let committed = self.committed_path_text();
        let focused = self.path_input.read(cx).focus_handle(cx).is_focused(window);
        if !force && focused && self.path_edited {
            return;
        }
        self.path_seed.clone_from(&committed);
        self.path_edited = false;
        let seed = committed;
        self.path_input.update(cx, |input, cx| {
            let shown = input.value().to_string();
            if shown != seed {
                input.set_value(seed, window, cx);
            }
        });
    }

    /// The path text the committed location displays (lossy); typed edits
    /// and submits go through the raw-state resolver instead.
    fn committed_path_text(&self) -> String {
        match self.state.current() {
            Location::Directory(path) => path.to_string_lossy().into_owned(),
            Location::Trash => "Trash".to_owned(),
        }
    }

    /// Synchronizes the search field with the committed query after a
    /// commit, so the field, the listing and the match count agree again.
    /// `set_value` emits no Change; no query-subscription feedback.
    fn sync_search_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let committed = self.state.query().to_owned();
        self.search_input.update(cx, |input, cx| {
            let shown = input.value().to_string();
            if let Some(value) = search_field_resync(&committed, &shown) {
                input.set_value(value.to_owned(), window, cx);
            }
        });
    }

    fn sync_window_title(&self, window: &mut Window) {
        window.set_window_title(&format!("{} — Fileflux", self.current_label()));
    }

    /// Focuses the path field and selects its content.
    fn focus_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.path_edited = false;
        self.path_input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        cx.notify();
    }

    /// Focuses the search field and selects its content.
    fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        cx.notify();
    }

    /// Escape in the path field (captured by the field's frame, see
    /// [`super::toolbar`]): restores the committed path and gives the
    /// keyboard back to the entries area, the useful place a canceled path
    /// edit lands. Dismiss the failed edit's status without changing the
    /// committed location, listing or navigation history.
    pub(crate) fn restore_path_on_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.path_edited = false;
        self.path_seed = self.committed_path_text();
        let seed = self.path_seed.clone();
        self.path_input.update(cx, |input, cx| {
            input.set_value(seed, window, cx);
        });
        self.state.cancel_path_edit();
        self.entries_focus.focus(window, cx);
        cx.notify();
    }

    /// Escape in the search field (captured by the field's frame):
    /// clears the query — the full listing returns, the no-result state
    /// goes away — and gives the keyboard back to the entries area.
    pub(crate) fn clear_search_on_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.state.clear_query();
        self.search_input.update(cx, |input, cx| {
            input.set_value(String::new(), window, cx);
        });
        self.entries_focus.focus(window, cx);
        cx.notify();
    }

    /// Escape anywhere else in the shell: drops the selection and returns
    /// the focus to the shell, so a second Escape never reaches the
    /// window close path by accident.
    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.state.selection().is_empty() {
            self.state.clear_selection();
        }
        self.shell_focus.focus(window, cx);
        cx.notify();
    }

    /// Submits the path field. An unedited field submits the committed
    /// location (a reload); an edited one resolves its text through
    /// [`ExplorerState::resolve_input`] — `~`, absolute and relative
    /// forms, no shell.
    pub(crate) fn submit_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.path_input.read(cx).value().to_string();
        if !self.path_edited || text == self.path_seed {
            // Unedited: the submit retains the actual current path.
            let current = self.state.current().clone();
            self.state.clear_path_error();
            self.navigate(Transition::Visit(current), window, cx);
            return;
        }
        match self.state.resolve_input(&text) {
            Ok(location) => {
                self.state.clear_path_error();
                self.state.clear_notice();
                let revision = self.path_edit_revision;
                let scan = self.navigate(Transition::Visit(location), window, cx);
                if let Some(scan) = scan {
                    self.submitted_edit = Some(SubmittedEdit {
                        generation: scan.generation,
                        revision,
                    });
                }
            }
            Err(argument) => {
                self.state.clear_notice();
                self.state.set_path_error(argument);
                cx.notify();
            }
        }
    }

    /// Moves the keyboard selection by `delta` visible positions and
    /// scrolls the row it lands on into view.
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.state.visible_indices().len();
        if count == 0 {
            return;
        }
        let last = count - 1;
        let current = self
            .state
            .selection()
            .iter()
            .next()
            .and_then(|path| self.state.visible_position_of(path));
        let target = match current {
            Some(position) => (position as isize + delta).clamp(0, last as isize) as usize,
            None => 0,
        };
        self.state.select_visible(target);
        self.scroll_selected_row(target);
        cx.notify();
    }

    fn select_extreme(&mut self, position: usize, cx: &mut Context<Self>) {
        let count = self.state.visible_indices().len();
        if count == 0 {
            return;
        }
        self.state.select_visible(position);
        self.scroll_selected_row(position);
        cx.notify();
    }

    fn scroll_selected_row(&self, position: usize) {
        let row = match self.state.view() {
            View::List => position,
            View::Grid => util::grid_row(position, self.rendered_columns.max(1)),
        };
        self.active_scroll_handle()
            .scroll_to_item(row, ScrollStrategy::Nearest);
    }

    /// The vertical scroll handle of the presentation that is showing: the
    /// two views count rows in different units, so they never share one.
    fn active_scroll_handle(&self) -> &UniformListScrollHandle {
        match self.state.view() {
            View::List => &self.list_scroll,
            View::Grid => &self.grid_scroll,
        }
    }

    /// Switches the presentation. The other view's handle keeps an offset
    /// counted in the wrong row units, so the fresh view starts at its top;
    /// the selection lives in the state and survives untouched.
    pub(crate) fn set_view(&mut self, view: View, cx: &mut Context<Self>) {
        if self.state.view() == view {
            return;
        }
        self.state.set_view(view);
        self.active_scroll_handle()
            .scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    /// Enter (or double click) on the selected entry: directories navigate,
    /// everything else answers honestly read-only. Only **visible**
    /// selected entries activate: a selection the current filter has
    /// hidden is never silently opened (see
    /// [`ExplorerState::activation_candidate`]).
    pub(crate) fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.state.activation_candidate() else {
            return;
        };
        self.open_path(&path, window, cx);
    }

    /// Opens one entry: a real directory navigates; a file, symlink or
    /// broken link sets the honest read-only notice and keeps everything.
    /// A symlinked directory is not followed either: the MVP navigates
    /// entry paths as scanned, and Trash rows never lead out of the Trash.
    pub(crate) fn open_path(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.snapshot.iter().find(|entry| entry.path == path) else {
            return;
        };
        if entry.is_directory() && !entry.is_symlink {
            self.navigate(
                Transition::Visit(Location::Directory(entry.path.clone())),
                window,
                cx,
            );
            return;
        }
        let name = entry.display_name().into_owned();
        self.state.set_open_notice(&name);
        cx.notify();
    }

    /// Toggles the favorites membership of the current directory.
    pub(crate) fn toggle_favorite(&mut self, cx: &mut Context<Self>) {
        self.state.toggle_favorite();
        cx.notify();
    }

    /// The width the content island can use, as the **first-frame estimate**
    /// of the entries area's inner width, derived before the measuring
    /// canvas has reported: the 8 px outer gutters, the sidebar column, the
    /// inter-island gap and the island's 16 px content padding.
    fn content_width(&self, window: &Window) -> f32 {
        let total = f32::from(window.viewport_size().width);
        let narrow = total < NARROW_WIDTH;
        let sidebar = if self.sidebar_open && !narrow {
            SIDEBAR_WIDTH + IslandMetrics::GAP.as_f32()
        } else {
            0.0
        };
        (total - 2.0 * IslandMetrics::GAP.as_f32() - sidebar - 2.0 * 16.0).max(0.0)
    }
}

/// The path-field edit a successful edited-path submit started: the scan
/// generation it began under, and the field's edit revision at submit time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SubmittedEdit {
    generation: u64,
    revision: u64,
}

impl SubmittedEdit {
    /// Only the submission's own scan, and only with no newer edit, may
    /// reseed the field — even while it stays focused. The revision, never
    /// the text, decides: edits away and back are still newer edits.
    fn settles(self, scan_generation: u64, current_revision: u64) -> bool {
        self.generation == scan_generation && self.revision == current_revision
    }
}

/// The value the search field must be set to after a commit, or `None` when
/// it already shows the committed query and no programmatic set is needed.
fn search_field_resync<'a>(committed_query: &'a str, field_value: &str) -> Option<&'a str> {
    (field_value != committed_query).then_some(committed_query)
}

impl Render for Explorer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = bolt::colors(cx);
        let narrow = f32::from(window.viewport_size().width) < NARROW_WIDTH;

        // Grid geometry comes from the entries area's measured inner width
        // once the first frame has reported it; the estimate only seeds the
        // very first frame. The arrow keys reuse the same geometry.
        self.rendered_columns = entries::grid_columns(
            self.entries_width
                .unwrap_or_else(|| self.content_width(window)),
        );

        let document = self.render_document_island(cx);
        let side = sidebar::render(self, narrow, cx);

        // The sidebar reads on the LEFT of the content island (artboard
        // 01); a narrow window leaves the island alone and overlays the
        // parked sidebar on top.
        let mut middle = div()
            .relative()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .px(IslandMetrics::GAP)
            .gap(IslandMetrics::GAP);
        match side {
            Some(sidebar) if !narrow => middle = middle.child(sidebar).child(document),
            Some(sidebar) => {
                middle = middle.child(document).child(
                    div()
                        .absolute()
                        .left(IslandMetrics::GAP)
                        .top(IslandMetrics::GAP)
                        .bottom(Spacing::LG + IslandMetrics::GAP)
                        .child(sidebar),
                )
            }
            None => middle = middle.child(document),
        }

        div()
            .key_context(EXPLORER_CONTEXT)
            .track_focus(&self.shell_focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.desk)
            .text_color(colors.foreground)
            .child(self.render_header(cx))
            .on_action(cx.listener(|this, _: &FocusPath, window, cx| {
                this.focus_path(window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                this.focus_search(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CancelEdit, window, cx| {
                this.cancel_edit(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NavigateBack, window, cx| {
                this.navigate(Transition::Back, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NavigateForward, window, cx| {
                this.navigate(Transition::Forward, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NavigateUp, window, cx| {
                this.navigate(Transition::Up, window, cx);
            }))
            .on_action(cx.listener(|this, _: &Reload, window, cx| {
                let current = this.state.current().clone();
                this.navigate(Transition::Visit(current), window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectUp, _window, cx| {
                let columns = this.grid_step();
                this.move_selection(-columns, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectDown, _window, cx| {
                let columns = this.grid_step();
                this.move_selection(columns, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectLeft, _window, cx| {
                if this.state.view() == View::List {
                    return;
                }
                this.move_selection(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectRight, _window, cx| {
                if this.state.view() == View::List {
                    return;
                }
                this.move_selection(1, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectFirstEntry, _window, cx| {
                this.select_extreme(0, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectLastEntry, _window, cx| {
                let count = this.state.visible_indices().len();
                this.select_extreme(count.saturating_sub(1), cx);
            }))
            .on_action(cx.listener(|this, _: &OpenSelected, window, cx| {
                this.open_selected(window, cx);
            }))
            .child(middle)
            .child(self.render_statusbar(cx))
    }
}

impl Explorer {
    /// The 44 px header row on the desk: the Fileflux brandmark, and the
    /// current folder as the window title shows it. No buttons — the family
    /// shell has one primary job.
    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement {
        let colors = bolt::colors(cx);
        let tint = ChromeColors::tint(cx.theme());
        div()
            .id("explorer-header")
            .flex_none()
            .h(px(44.0))
            .flex()
            .items_center()
            .px(Spacing::LG)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(Spacing::SM)
                    .text_color(tint)
                    .child(
                        div()
                            .h(px(22.0))
                            .w(px(22.0))
                            .bg(tint)
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(colors.desk)
                            .child("F"),
                    )
                    .child("fileflux"),
            )
            .child(
                div()
                    .id("explorer-header-title")
                    .absolute()
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .text_size(px(12.0))
                    .text_color(colors.muted)
                    .child(format!("{} — Fileflux", self.current_label())),
            )
    }

    /// In the grid, up and down move one row of tiles; in the list they
    /// move one entry.
    fn grid_step(&self) -> isize {
        match self.state.view() {
            View::List => 1,
            View::Grid => self.rendered_columns.max(1) as isize,
        }
    }

    /// The document island: toolbar, scope line, entries, all cut 14 px at
    /// the opposing corners by [`Island`].
    fn render_document_island(&mut self, cx: &mut Context<Self>) -> Island {
        Island::new()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(toolbar::render(self, cx))
            .children(self.render_scope_line(cx))
            .child(entries::render(self, cx))
    }

    /// The scope line under the toolbar: the Trash scope, the live filter,
    /// or nothing.
    fn render_scope_line(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
        let colors = bolt::colors(cx);
        let text = if self.state.current() == &Location::Trash {
            Some("Home Trash — read-only: no restore, delete or empty here.".to_owned())
        } else {
            let query = self.state.query();
            (!query.is_empty()).then(|| {
                format!(
                    "Filtering \u{201c}{query}\u{201d} — {} of {} items. Current folder only, no subfolders.",
                    self.state.visible_indices().len(),
                    self.state.listing().entries.len()
                )
            })
        }?;
        Some(
            div()
                .id("explorer-scope")
                .flex_none()
                .px(Spacing::XL)
                .pt(Spacing::SM)
                .text_size(px(12.0))
                .text_color(colors.muted)
                .child(text),
        )
    }

    /// The 38 px status bar: counts, the live status (loading, errors,
    /// notice, selection), warnings and the key hints.
    fn render_statusbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let colors = bolt::colors(cx);
        let entries_count = self.state.listing().entries.len();
        let visible = if self.state.query().is_empty() {
            None
        } else {
            Some(self.state.visible_indices().len())
        };

        let count_text = match (entries_count, visible) {
            (_, Some(shown)) => format!("{shown} of {entries_count} shown"),
            (0, None) => "0 items".to_owned(),
            (1, None) => "1 item".to_owned(),
            (count, None) => format!("{count} items"),
        };

        let mut status = div().flex().items_center().gap(Spacing::SM).min_w_0();
        if self.state.is_loading() {
            status = status.child(
                div()
                    .id("explorer-status-loading")
                    .text_color(colors.foreground)
                    .child("Loading…"),
            );
        } else if let Some(error) = self.state.load_error() {
            status = status.child(
                div()
                    .id("explorer-status-error")
                    .flex_none()
                    .border_1()
                    .border_color(colors.danger)
                    .px(Spacing::SM)
                    .text_color(colors.danger)
                    .child(format!("Could not open folder: {error}")),
            );
        } else if let Some(error) = self.state.path_error() {
            status = status.child(
                div()
                    .id("explorer-status-error")
                    .flex_none()
                    .border_1()
                    .border_color(colors.danger)
                    .px(Spacing::SM)
                    .text_color(colors.danger)
                    .child(path_argument_message(error)),
            );
        } else if let Some(notice) = self.state.notice() {
            status = status.child(
                div()
                    .id("explorer-status-notice")
                    .text_color(colors.muted)
                    .child(notice.to_owned()),
            );
        } else if let Some(path) = self.state.selection().iter().next() {
            let mut text = selection_text(self, path);
            let more = self.state.selection().len().saturating_sub(1);
            if more > 0 {
                text.push_str(&format!(" (+{more} more)"));
            }
            status = status.child(
                div()
                    .id("explorer-status-selected")
                    .min_w_0()
                    .truncate()
                    .text_color(colors.foreground)
                    .child(text),
            );
        }

        let warnings = self.state.warnings().len();
        div()
            .id("explorer-status")
            .flex_none()
            .h(px(38.0))
            .flex()
            .items_center()
            .gap(Spacing::LG)
            .px(Spacing::LG)
            .text_size(px(12.0))
            .text_color(colors.muted)
            .child(count_text)
            .child(status)
            .child(
                div()
                    .ml_auto()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(Spacing::MD)
                    .children((warnings > 0).then(|| {
                        div()
                            .text_color(colors.warning)
                            .child(format!("! {warnings} warning(s) reading this folder"))
                    }))
                    .child("Ctrl+L path · Ctrl+F find · Alt+←/→/↑ history · Ctrl+R reload"),
            )
    }
}

/// The status text of the single selected entry: its (lossy) path plus, for
/// trash entries, the recorded original location. Display-only; navigation
/// always uses the raw paths the state keeps.
fn selection_text(explorer: &Explorer, path: &Path) -> String {
    let mut text = format!("Selected: {}", path.to_string_lossy());
    if let Some(origin) = explorer
        .state
        .listing()
        .entries
        .iter()
        .find(|entry| entry.path == path)
        .and_then(|entry| entry.origin.as_ref())
    {
        text.push_str(&format!(" (from {})", origin.to_string_lossy()));
    }
    text
}

/// The user-facing message for a rejected path argument.
pub(crate) fn path_argument_message(argument: &PathArgument) -> String {
    match argument {
        PathArgument::Empty => "Enter a path to navigate.".to_owned(),
        PathArgument::Unresolvable(detail) => format!("Cannot navigate there: {detail}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The finding: a successful edited-path submission reseeds the path
    /// field — even while it stays focused — so a second Enter reloads the
    /// displayed folder instead of resolving the stale text against the new
    /// directory. Only the submission's own scan settles it, and only while
    /// no newer edit exists; the revision (never the text) decides, so
    /// edits away and back are still newer edits.
    #[test]
    fn a_submitted_path_edit_settles_only_on_its_own_current_scan() {
        let submitted = SubmittedEdit {
            generation: 7,
            revision: 3,
        };
        let same_scan = submitted.settles(7, 3);
        assert!(same_scan, "the submitted scan's completion reseeds");
        let newer_scan = submitted.settles(8, 3);
        assert!(!newer_scan, "a newer scan's completion never reseeds");
        let newer_edit = submitted.settles(7, 4);
        assert!(!newer_edit, "an edit after the submission is preserved");
        let edited_away_and_back = submitted.settles(7, 5);
        assert!(
            !edited_away_and_back,
            "identical text at a newer revision is a newer edit"
        );
    }

    /// The finding: a successful navigation clears the state's query, and
    /// the search field must follow the committed query, so the field and
    /// the listing agree. An already-synced field is left alone.
    #[test]
    fn the_search_field_resyncs_with_the_committed_query() {
        let cleared = search_field_resync("", "alpha");
        assert_eq!(cleared, Some(""), "field follows cleared query");
        let restored = search_field_resync("alpha", "");
        assert_eq!(restored, Some("alpha"), "lost query is restored");
        assert_eq!(search_field_resync("", ""), None, "already in sync: no set");
    }
}
