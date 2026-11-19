//! The native explorer view: the whole window's layout, the async navigation
//! guard and the keyboard vocabulary.
//!
//! Layout follows the reference design (design/file-explorer/01, translated
//! from design/system.md): the desk with a native title bar, 8 px gutters, a
//! 290 px sidebar island and a document island, and a 38 px status bar. The
//! in-window header strip is empty; the window title shows the current
//! folder plus Fileflux.
//!
//! Directory scans run off the UI thread (see [`load_location`]); every scan
//! carries a generation from [`ExplorerState::begin`], and only a completion
//! matching the newest generation settles the state, so stale or failed
//! scans never corrupt the committed location, history, selection or query.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use bolt::composites::{Island, StatusChip};
use bolt::controls::{Button, InputEvent, InputState};
use bolt::modals::{ModalKey, ModalKeyHandler, ModalSeams, on_modal_key};
use bolt::toast::{ToastGlobal, ToastHost};
use bolt::tokens::{IslandMetrics, Spacing};
use fileflux::core::places::{Places, discover, startup_trash_root};
use fileflux::core::settings::{
    self, Preferences, SESSION_VERSION, SETTINGS_VERSION, Session, Settings,
};
use fileflux::core::terminal::{self, Terminal, TerminalEnv};
use fileflux::core::watch::{
    Debounce, FolderEvent, WatchHandle, WatchHub, nearest_existing_ancestor, stop_notice,
};
use fileflux::core::{Listing, Location};
use gpui::{
    App, AppContext as _, Context, Div, Entity, FocusHandle, Focusable, InteractiveElement,
    KeyBinding, MouseButton, MouseMoveEvent, MouseUpEvent, ParentElement, Render, ScrollStrategy,
    Stateful, Styled, Subscription, Task, UniformListScrollHandle, Window, actions, deferred, div,
    prelude::*, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::table::{TableEvent, TableState};
use gpui_component::tooltip::Tooltip;

use crate::explorer::actions::{MenuState, PendingSelect, Renaming};
use crate::explorer::arrange::{ArrangeCommand, ArrangeIndex};
use crate::explorer::clipboard::PasteHandle;
use crate::explorer::devices::DeviceHub;
use crate::explorer::favorites::{BookmarkSync, favorites_of};
use crate::explorer::inspector::{self, PermissionProbe};
use crate::explorer::ops::paste::{ClipMode, ClipboardState, PasteInto};
use crate::explorer::ops::{DialogQueue, MenuPage};
use crate::explorer::ops::{OpenAction, Severity, UndoStack, open_action};
use crate::explorer::persist::{
    Persistence, Startup, arrangement_from_prefs, arrangement_to_prefs, restore_fallback_notice,
    session_tab, view_from_pref, view_to_pref,
};
use crate::explorer::progress::Registry;
use crate::explorer::sidebar::{Panel, PanelDrag};
use crate::explorer::state::{ExplorerState, Favorites, PathArgument, Scan, Transition, View};
use crate::explorer::table::{EntryTableDelegate, ListWidths};
use crate::explorer::tabs::{Affected, Placement, TabList};
use crate::explorer::toasts;
use crate::explorer::{entries, overlay, progress, sidebar, tabstrip, toolbar, util};

/// Key context of the whole explorer window.
pub(crate) const EXPLORER_CONTEXT: &str = "FilefluxExplorer";
/// Key context of the entries area, which carries the arrow-key navigation.
pub(crate) const ENTRIES_CONTEXT: &str = "FilefluxEntries";

actions!(
    fileflux_explorer,
    [
        /// Switches the breadcrumbs to the editable path field.
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
        /// Shows or hides the details panel.
        ToggleInspector,
        /// Shows or hides the hidden (dot) files of the tab (Ctrl+H).
        ToggleHidden,
        /// Creates a folder in the shown folder and renames it in place.
        NewFolder,
        /// Renames the selected entry in place (F2).
        RenameSelected,
        /// Selects every visible entry (Ctrl+A).
        SelectAll,
        /// Moves the selected entries to the Trash (Del).
        TrashSelected,
        /// Asks to delete the selected entries for good (Shift+Del).
        DeleteSelected,
        /// Undoes the last rename, trash, creation, copy or move (Ctrl+Z).
        UndoLast,
        /// Copies the path of the selected entries to the clipboard.
        CopyPathSelected,
        /// Copies the selected entries (Ctrl+C), to paste them elsewhere.
        CopyEntries,
        /// Cuts the selected entries (Ctrl+X): pasting moves them.
        CutEntries,
        /// Pastes the clipboard's entries into the shown folder (Ctrl+V).
        PasteEntries,
        /// Opens the context menu on the selection (Menu key, Shift+F10).
        OpenContextMenu,
        /// Highlights the previous context-menu item.
        MenuUp,
        /// Highlights the next context-menu item.
        MenuDown,
        /// Runs the highlighted context-menu item.
        MenuConfirm,
        /// Closes the context menu.
        MenuCancel,
        /// Cancels the open confirmation dialog.
        DialogCancel,
        /// Moves up in the "Open with…" list.
        DialogUp,
        /// Moves down in the "Open with…" list.
        DialogDown,
        /// Opens a new tab on the current folder (Ctrl+T).
        NewTab,
        /// Closes the current tab; the last tab stays (Ctrl+W).
        CloseTab,
        /// Activates the next tab, wrapping around (Ctrl+Tab, Ctrl+PageDown).
        NextTab,
        /// Activates the previous tab, wrapping around (Ctrl+Shift+Tab,
        /// Ctrl+PageUp).
        PreviousTab,
        /// Activates the first tab (Alt+1).
        JumpToTab1,
        /// Activates the second tab (Alt+2).
        JumpToTab2,
        /// Activates the third tab (Alt+3).
        JumpToTab3,
        /// Activates the fourth tab (Alt+4).
        JumpToTab4,
        /// Activates the fifth tab (Alt+5).
        JumpToTab5,
        /// Activates the sixth tab (Alt+6).
        JumpToTab6,
        /// Activates the seventh tab (Alt+7).
        JumpToTab7,
        /// Activates the eighth tab (Alt+8).
        JumpToTab8,
        /// Activates the ninth tab (Alt+9).
        JumpToTab9,
    ]
);

/// Key context of the open context menu.
pub(crate) const MENU_CONTEXT: &str = "FilefluxMenu";

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
        /// Extends the selection to the previous visible entry.
        ExtendUp,
        /// Extends the selection to the next visible entry.
        ExtendDown,
        /// Extends the selection one tile to the left in the grid.
        ExtendLeft,
        /// Extends the selection one tile to the right in the grid.
        ExtendRight,
        /// Extends the selection to the first visible entry.
        ExtendToFirst,
        /// Extends the selection to the last visible entry.
        ExtendToLast,
        /// Opens the selected directory, or the selected file with the
        /// default application.
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
        KeyBinding::new("ctrl-i", ToggleInspector, shell),
        KeyBinding::new("ctrl-h", ToggleHidden, shell),
        KeyBinding::new("up", SelectUp, entries),
        KeyBinding::new("down", SelectDown, entries),
        KeyBinding::new("left", SelectLeft, entries),
        KeyBinding::new("right", SelectRight, entries),
        KeyBinding::new("home", SelectFirstEntry, entries),
        KeyBinding::new("end", SelectLastEntry, entries),
        KeyBinding::new("shift-up", ExtendUp, entries),
        KeyBinding::new("shift-down", ExtendDown, entries),
        KeyBinding::new("shift-left", ExtendLeft, entries),
        KeyBinding::new("shift-right", ExtendRight, entries),
        KeyBinding::new("shift-home", ExtendToFirst, entries),
        KeyBinding::new("shift-end", ExtendToLast, entries),
        KeyBinding::new("enter", OpenSelected, entries),
    ]);

    // The file operations answer anywhere in the window except while a text
    // field, the context menu or a dialog has the keyboard.
    let operations = Some("FilefluxExplorer && !Input && !Modal && !FilefluxMenu");
    cx.bind_keys([
        KeyBinding::new("ctrl-a", SelectAll, operations),
        KeyBinding::new("f2", RenameSelected, operations),
        KeyBinding::new("delete", TrashSelected, operations),
        KeyBinding::new("shift-delete", DeleteSelected, operations),
        KeyBinding::new("ctrl-shift-n", NewFolder, operations),
        KeyBinding::new("ctrl-z", UndoLast, operations),
        KeyBinding::new("ctrl-shift-c", CopyPathSelected, operations),
        KeyBinding::new("ctrl-c", CopyEntries, operations),
        KeyBinding::new("ctrl-x", CutEntries, operations),
        KeyBinding::new("ctrl-v", PasteEntries, operations),
        KeyBinding::new("menu", OpenContextMenu, operations),
        KeyBinding::new("shift-f10", OpenContextMenu, operations),
        KeyBinding::new("up", MenuUp, Some(MENU_CONTEXT)),
        KeyBinding::new("down", MenuDown, Some(MENU_CONTEXT)),
        KeyBinding::new("enter", MenuConfirm, Some(MENU_CONTEXT)),
        KeyBinding::new("escape", MenuCancel, Some(MENU_CONTEXT)),
        KeyBinding::new("escape", DialogCancel, Some("Modal")),
        KeyBinding::new("up", DialogUp, Some("Modal")),
        KeyBinding::new("down", DialogDown, Some("Modal")),
    ]);

    // Tabs answer anywhere in the window except behind a dialog or the
    // open menu, text fields included: none of these chords types text.
    let tabs = Some("FilefluxExplorer && !Modal && !FilefluxMenu");
    cx.bind_keys([
        KeyBinding::new("ctrl-t", NewTab, tabs),
        KeyBinding::new("ctrl-w", CloseTab, tabs),
        KeyBinding::new("ctrl-tab", NextTab, tabs),
        KeyBinding::new("ctrl-pagedown", NextTab, tabs),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, tabs),
        KeyBinding::new("ctrl-pageup", PreviousTab, tabs),
        KeyBinding::new("alt-1", JumpToTab1, tabs),
        KeyBinding::new("alt-2", JumpToTab2, tabs),
        KeyBinding::new("alt-3", JumpToTab3, tabs),
        KeyBinding::new("alt-4", JumpToTab4, tabs),
        KeyBinding::new("alt-5", JumpToTab5, tabs),
        KeyBinding::new("alt-6", JumpToTab6, tabs),
        KeyBinding::new("alt-7", JumpToTab7, tabs),
        KeyBinding::new("alt-8", JumpToTab8, tabs),
        KeyBinding::new("alt-9", JumpToTab9, tabs),
    ]);
    install_modal_seams(cx);
}

/// What Bolt's [`Modal`](bolt::modals::Modal) needs from the product: Escape
/// cancels. Enter is deliberately left unbound — a confirmation dialog acts
/// only through the button that has the focus, which is Cancel.
fn install_modal_seams(cx: &mut App) {
    fn attach(backdrop: Stateful<Div>, handler: ModalKeyHandler) -> Stateful<Div> {
        on_modal_key::<DialogCancel>(backdrop, ModalKey::Cancel, &handler)
    }
    cx.set_global(ModalSeams {
        attach_keys: attach,
        defer_to_owner: |_, _, _| {},
        escape_label: |_| Some("Esc".into()),
        close_icon: crate::explorer::icon(gpui_kit_assets::IconName::X),
        close_label: || "Close".into(),
    });
}

/// Reads one location off the UI thread: a plain directory scan, or the
/// home Trash listing. Recorded original Trash paths are never opened.
/// A listing read on the background executor with everything the UI thread
/// would otherwise build per commit: the arrangement index and the shared
/// entry snapshot. Both cost a walk of the whole folder, so both are made
/// next to the read.
struct Loaded {
    listing: Listing,
    index: Arc<ArrangeIndex>,
    snapshot: Arc<Vec<fileflux::core::Entry>>,
    folder_label: String,
}

/// [`load_location`] plus the derived data of [`Loaded`].
fn load_indexed(location: &Location, trash_root: Option<&Path>) -> io::Result<Loaded> {
    let listing = load_location(location, trash_root)?;
    let index = Arc::new(ArrangeIndex::build_hiding(
        &listing.entries,
        &listing.hidden_names,
    ));
    let snapshot = Arc::new(listing.entries.clone());
    let folder_label = util::folder_counts_label(&listing.entries);
    Ok(Loaded {
        listing,
        index,
        snapshot,
        folder_label,
    })
}

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

/// How long an arrangement job may run before the status bar says so.
const ARRANGE_HINT_DELAY: Duration = Duration::from_millis(150);

/// How often the UI collects what the watcher thread reported. The thread
/// itself sleeps in the kernel; this only drains its channel.
const WATCH_STEP: Duration = Duration::from_millis(100);

/// The folder subbar's height.
const SUBBAR_HEIGHT: f32 = 44.0;
/// The subbar's horizontal padding.
const SUBBAR_PADDING: f32 = 20.0;

/// Below this viewport width the sidebar parks behind the sidebar toggle,
/// so path, search and view stay reachable.
pub(crate) const NARROW_WIDTH: f32 = 760.0;

/// Identifies one tab for as long as it lives. Ids are never reused, so a
/// task that outlives its tab finds nothing instead of another tab.
pub(crate) type TabId = u64;

/// Everything one tab keeps for itself: where it is and how it got there,
/// what is selected, how it is presented, the filter and path fields, the
/// scroll handles, and the background work for its folder. The window-wide
/// things — places, favorites, clipboard, undo, dialogs, operations — live
/// on [`Explorer`].
pub(crate) struct Tab {
    /// This tab's identity, for tasks that must find it again.
    pub(crate) id: TabId,
    /// The committed explorer state, guarded by generations.
    pub(crate) state: ExplorerState,
    /// A clone of the committed listing for the virtualized rows, replaced
    /// on every commit; the render closures read this instead of scanning
    /// the directory again.
    pub(crate) snapshot: Arc<Vec<fileflux::core::Entry>>,
    /// "4 folders · 7 files" of the committed listing, counted on the scan
    /// thread so no frame walks the folder for it.
    pub(crate) folder_label: String,
    /// The editable path field.
    pub(crate) path_input: Entity<InputState>,
    /// The text the path field was last seeded with; an unedited submit
    /// retains the committed location even for non-UTF8 names.
    pub(crate) path_seed: String,
    /// Whether the path field holds an edit that has not been submitted.
    pub(crate) path_edited: bool,
    /// Whether the editable path field replaces the breadcrumbs.
    pub(crate) path_editing: bool,
    /// The path field's edit revision: bumped by every user edit, never by
    /// programmatic reseeds (they emit no Change event).
    path_edit_revision: u64,
    /// The edited-path submit in flight, if any: its scan generation plus
    /// the edit revision at submit time.
    submitted_edit: Option<SubmittedEdit>,
    /// The current-folder filename search field.
    pub(crate) search_input: Entity<InputState>,
    /// The details table's state entity: one per tab, so each tab keeps its
    /// own scroll offset; the delegate answers the tab's snapshot.
    pub(crate) table: Entity<TableState<EntryTableDelegate>>,
    /// The vertical scroll handle of the tile grid.
    pub(crate) grid_scroll: UniformListScrollHandle,
    /// The retained scan task, replaced by the next scan.
    scan_task: Option<Task<()>>,
    /// The mode of the single inspected entry, probed off the UI thread.
    pub(crate) permissions: Option<PermissionProbe>,
    /// The path whose permission probe is waiting out its debounce or
    /// running: at most one at a time (see [`inspector::probe_decision`]).
    pub(crate) permission_in_flight: Option<PathBuf>,
    /// The retained permission probe task.
    pub(crate) permission_task: Option<Task<()>>,
    /// The retained folder-counting task, replaced by the next commit.
    counts_task: Option<Task<()>>,
    /// The retained arrangement job loop (see [`Explorer::arrange_tab`]).
    arrange_task: Option<Task<()>>,
    /// When the current stretch of arrangement jobs began.
    arranging_since: Option<Instant>,
    /// The timer that repaints once a job has run [`ARRANGE_HINT_DELAY`].
    arrange_hint_task: Option<Task<()>>,
    /// The holds on the folders this tab live-refreshes from.
    watch: TabWatch,
    /// What to select (and whether to rename) once the next listing shows it.
    pub(crate) pending: Option<PendingSelect>,
    /// The path field's Enter/Change/Blur subscription.
    _path_events: Subscription,
    /// The search field's Change subscription.
    _search_events: Subscription,
    /// The details table's width-drag events.
    _table_events: Subscription,
}

impl Tab {
    /// An arrangement job has been running long enough to say so.
    pub(crate) fn arrange_is_slow(&self) -> bool {
        self.state.is_arranging()
            && self
                .arranging_since
                .is_some_and(|since| since.elapsed() >= ARRANGE_HINT_DELAY)
    }

    /// A tab with its own fields and tables, committed to `state` but not
    /// scanned yet.
    fn new(
        id: TabId,
        state: ExplorerState,
        window: &mut Window,
        cx: &mut Context<Explorer>,
    ) -> Self {
        let path_input = cx.new(|cx| InputState::new(window, cx));
        let search_input = cx.new(|cx| {
            let mut input = InputState::new(window, cx);
            input.set_placeholder("Search this folder…", window, cx);
            input
        });

        // The details table's state: one entity per tab, with the table's
        // internal selection off — the explorer's raw-path selection
        // (single, Ctrl-toggle, keyboard) drives everything through the
        // delegate's row clicks, so the table can never clobber it.
        let weak_this = cx.entity().downgrade();
        let table = cx.new(|cx| {
            TableState::new(
                EntryTableDelegate::new(weak_this, Arc::new(Vec::new()), Arc::new(Vec::new())),
                window,
                cx,
            )
            .row_selectable(false)
            .col_selectable(false)
            .cell_selectable(false)
            .sortable(false)
            .col_movable(false)
        });

        // The path field: Enter submits, typing clears the path error, and
        // blur restores the committed path (the same as Escape).
        let path_events = cx.subscribe_in(
            &path_input,
            window,
            move |this, _input, event: &InputEvent, window, cx| {
                this.path_event(id, event, window, cx);
            },
        );

        // The search field filters its tab's folder on every keystroke.
        let search_events = cx.subscribe_in(
            &search_input,
            window,
            move |this, _input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    let Some(tab) = this.tabs.by_id_mut(id) else {
                        return;
                    };
                    let text = tab.search_input.read(cx).value().to_string();
                    tab.state.set_query(text);
                    this.arrange_tab(id, cx);
                    cx.notify();
                }
            },
        );

        // The details table's column drags: only widths that actually moved
        // against the delegate's applied widths are deliberate, so an
        // undragged column keeps re-filling when the pane changes.
        let table_events = cx.subscribe(&table, move |this, table, event: &TableEvent, cx| {
            if let TableEvent::ColumnWidthsChanged(widths) = event {
                let applied = *table.read(cx).delegate().widths();
                for (column, width) in widths.iter().enumerate() {
                    let Some(applied) = applied.get(column) else {
                        continue;
                    };
                    if (width.as_f32() - applied).abs() > crate::explorer::table::DRAG_EPSILON {
                        this.list_widths.record(column, width.as_f32());
                    }
                }
                cx.notify();
            }
        });

        Self {
            id,
            state,
            snapshot: Arc::new(Vec::new()),
            folder_label: String::new(),
            path_input,
            path_seed: String::new(),
            path_edited: false,
            path_editing: false,
            path_edit_revision: 0,
            submitted_edit: None,
            search_input,
            table,
            grid_scroll: UniformListScrollHandle::new(),
            scan_task: None,
            permissions: None,
            permission_in_flight: None,
            permission_task: None,
            counts_task: None,
            arrange_task: None,
            arranging_since: None,
            arrange_hint_task: None,
            watch: TabWatch::default(),
            pending: None,
            _path_events: path_events,
            _search_events: search_events,
            _table_events: table_events,
        }
    }
}

/// The watch holds of one tab, active or not: its committed folder, and the
/// folder a navigation in flight is reading, from the moment the scan
/// starts. Dropping a hold releases its share of the inotify watch.
#[derive(Debug, Default)]
struct TabWatch {
    committed: Option<WatchHandle>,
    target: Option<WatchHandle>,
}

impl TabWatch {
    /// Moves `slot` to watch `wanted`. The new hold is taken before the old
    /// one is released, so a folder both name never loses its watch. A hold
    /// whose watch died (the folder was deleted or replaced) is not kept even
    /// for the same path: that folder is watched anew.
    fn retarget(slot: &mut Option<WatchHandle>, wanted: Option<PathBuf>, hub: &WatchHub) {
        let kept = slot
            .as_ref()
            .is_some_and(|handle| Some(handle.path()) == wanted.as_deref() && handle.is_live());
        if kept || (slot.is_none() && wanted.is_none()) {
            return;
        }
        *slot = wanted.and_then(|path| match hub.watch(&path) {
            Ok(handle) => Some(handle),
            Err(error) => {
                log::warn!("cannot watch {}: {error}", path.display());
                None
            }
        });
    }
}

impl TabList<Tab> {
    /// The tab with `id`, if it is still open.
    pub(crate) fn by_id_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.iter_mut().find(|tab| tab.id == id)
    }
}

/// The context an operation started in, kept until its result arrives.
pub(crate) struct OpContext {
    /// The tab the operation was started from: the one a "select the new
    /// item" result belongs to.
    pub(crate) origin: TabId,
    /// The folders the operation changes.
    pub(crate) affected: Affected,
    /// Every path it reads or writes: what keeps a device from being
    /// unmounted while the operation runs (see `explorer::devices`).
    pub(crate) touches: Vec<PathBuf>,
}

/// The explorer view: the window's tabs plus everything the tabs share.
/// All per-folder state lives in [`Tab`]; this struct adds the window's
/// UI: focus handles, the sidebar, the operations and their dialogs.
pub(crate) struct Explorer {
    /// The open tabs, one of them active.
    pub(crate) tabs: TabList<Tab>,
    /// The id the next opened tab gets.
    next_tab_id: TabId,
    /// The home directory, when known: `~` in typed paths, new tabs.
    pub(crate) home: Option<PathBuf>,
    /// The favorites, shared by every tab: the GTK bookmarks file's local
    /// entries (see [`crate::explorer::favorites`]).
    pub(crate) favorites: Favorites,
    /// Favorites still in `settings.toml` because they could not move to the
    /// bookmarks file; empty once they did.
    pub(crate) legacy_favorites: Vec<PathBuf>,
    /// The bookmarks file and its watch.
    pub(crate) bookmarks: BookmarkSync,
    /// Sidebar discovery, arriving off the UI thread at startup.
    pub(crate) places: Option<Places>,
    /// Why the sidebar could not be discovered, when it could not.
    pub(crate) sidebar_error: Option<String>,
    /// The home Trash root once places are known.
    pub(crate) trash_root: Option<PathBuf>,
    /// The whole-window focus, so Escape has somewhere to give the focus
    /// back and the shell keys work after a click on bare chrome.
    pub(crate) shell_focus: FocusHandle,
    /// The entries area focus, which carries the arrow keys.
    pub(crate) entries_focus: FocusHandle,
    /// The column widths the user dragged on the details table, honored by
    /// [`crate::explorer::table::distribute_widths`] on every render.
    pub(crate) list_widths: ListWidths,
    /// The entries area's measured inner width, recorded by the draw-nothing
    /// measuring canvas every frame; `None` until the first layout reached
    /// it, so the first frame draws with the estimate below.
    pub(crate) entries_width: Option<f32>,
    /// Whether the sidebar is shown; narrow windows park it behind the
    /// sidebar toggle.
    pub(crate) sidebar_open: bool,
    /// The docked sidebar's width in CSS pixels: the saved preference (the
    /// reference design's 290 by default).
    pub(crate) sidebar_width: f32,
    /// The details panel's width in CSS pixels: the saved preference.
    pub(crate) details_width: f32,
    /// The panel handle being dragged, if any.
    pub(crate) panel_drag: Option<PanelDrag>,
    /// When and whether settings and session are written.
    persistence: Persistence,
    /// Restored tabs whose saved folder has not been read yet, by tab: if the
    /// first scan finds it gone, the tab falls back to an existing ancestor.
    restore_pending: HashMap<TabId, PathBuf>,
    /// How many restored tabs fell back so far, for the status notice.
    restore_fallbacks: usize,
    /// A settings problem for the status bar (an invalid file at startup, a
    /// failed save); the next navigation dismisses it.
    settings_notice: Option<String>,
    /// The grid column count of the last render, so keyboard row steps use
    /// the same geometry the list draws with.
    pub(crate) rendered_columns: usize,
    /// Whether the user wants the details panel (Ctrl+I, toolbar button).
    pub(crate) inspector_open: bool,
    /// Whether the panel drew in the last render: wanted and wide enough.
    pub(crate) inspector_shown: bool,
    /// The operations running off the UI thread, behind the progress cards.
    pub(crate) progress: Registry,
    /// Where each running operation started and what it changes.
    pub(crate) op_contexts: HashMap<u64, OpContext>,
    /// The session's undo history; survives navigation and tab switches.
    pub(crate) undo: UndoStack,
    /// The inline rename in progress, if any: it belongs to the active tab
    /// and is cancelled when the tab changes.
    pub(crate) renaming: Option<Renaming>,
    /// The open context menu, if any.
    pub(crate) menu: Option<MenuState>,
    /// The focus of the open context menu.
    pub(crate) menu_focus: FocusHandle,
    /// The open confirmation dialog and those waiting behind it: modal over
    /// the whole window, whichever tab is active.
    pub(crate) dialogs: DialogQueue,
    /// The focus of the dialog's backdrop (the focus trap).
    pub(crate) dialog_focus: FocusHandle,
    /// The dialog's default button: Cancel.
    pub(crate) dialog_cancel_focus: FocusHandle,
    /// The danger button; focused only by a deliberate move.
    pub(crate) dialog_confirm_focus: FocusHandle,
    /// A conflict question's Skip button: its default focus.
    pub(crate) dialog_skip_focus: FocusHandle,
    /// A conflict question's Keep both button.
    pub(crate) dialog_keep_focus: FocusHandle,
    /// What Copy / Cut took, waiting for Paste.
    pub(crate) clipboard: ClipboardState,
    /// The text Copy / Cut wrote to the system clipboard: when something
    /// else is there later, the internal clipboard is stale.
    pub(crate) clipboard_text: Option<String>,
    /// The pastes running now, by operation id, with their conflict channel.
    pub(crate) pastes: HashMap<u64, PasteHandle>,
    /// Bolt's toast stack: operation summaries, with Undo.
    pub(crate) toast_host: Entity<ToastHost>,
    /// The open toasts, oldest first, so the stack can be held to
    /// [`toasts::MAX_VISIBLE`].
    pub(crate) toast_ids: RefCell<Vec<(u64, Severity)>>,
    /// The toast stack's measured height, for placing it (see
    /// [`Explorer::render_toasts`]).
    toast_stack_height: Rc<Cell<f32>>,
    /// The terminal "Open in terminal" runs, once found (off the UI thread)
    /// and when the machine has one.
    pub(crate) terminal: Option<Terminal>,
    /// The one watcher thread every tab's folders are watched through;
    /// `None` when inotify is unavailable (tabs then refresh on activation).
    pub(super) hub: Option<WatchHub>,
    /// What the watcher thread reported, drained by [`Self::pump_watch_events`].
    watch_events: Option<Receiver<(PathBuf, FolderEvent)>>,
    /// Per-folder debounce of the change events, shared by every tab on it.
    pub(super) folder_debounce: HashMap<PathBuf, Debounce>,
    /// The retained loop that drains the watcher channel.
    _watch_pump: Option<Task<()>>,
    /// The machine's devices and the actions on them: the Devices section.
    pub(crate) devices: DeviceHub,
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

    /// The active tab.
    pub(crate) fn tab(&self) -> &Tab {
        self.tabs.active()
    }

    /// The active tab, mutably.
    pub(crate) fn tab_mut(&mut self) -> &mut Tab {
        self.tabs.active_mut()
    }

    /// The id of the active tab.
    pub(crate) fn active_id(&self) -> TabId {
        self.tabs.active().id
    }

    /// Builds the explorer: the first tab scans `start` (the command-line
    /// path), else the home (or the honest root fallback when `$HOME` is
    /// missing), and the places discovery both start off the UI thread
    /// right away.
    ///
    /// `startup` is what the settings and session files held: the
    /// preferences shape the shell and new tabs, the favorites replace the
    /// empty list, and a restored session replaces the single first tab.
    pub(crate) fn new(
        start: Option<PathBuf>,
        startup: Startup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let Startup {
            preferences,
            favorites,
            legacy_favorites,
            bookmarks_path,
            restored,
            persistence,
            notices,
        } = startup;
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let view = view_from_pref(preferences.view);
        let arrangement = arrangement_from_prefs(&preferences.arrangement);
        let mut tab_states: Vec<ExplorerState> = restored
            .as_ref()
            .map(|restored| {
                restored
                    .tabs
                    .iter()
                    .map(|tab| {
                        ExplorerState::new(tab.location.clone(), home.clone()).with_presentation(
                            view_from_pref(tab.view),
                            arrangement_from_prefs(&tab.arrangement),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        if tab_states.is_empty() {
            let initial = start
                .or_else(|| home.clone())
                .map(Location::Directory)
                .unwrap_or_else(|| Location::Directory(PathBuf::from("/")));
            tab_states.push(
                ExplorerState::new(initial, home.clone()).with_presentation(view, arrangement),
            );
        }
        let active_tab = restored.as_ref().map_or(0, |restored| restored.active);
        let mut rest = tab_states.into_iter();
        let first_state = rest.next().unwrap_or_else(|| {
            ExplorerState::new(Location::Directory(PathBuf::from("/")), home.clone())
        });
        let initial = first_state.current().clone();
        let mut restore_pending = HashMap::new();
        if restored.is_some()
            && let Location::Directory(path) = &initial
        {
            restore_pending.insert(0, path.clone());
        }
        let first = Tab::new(0, first_state, window, cx);
        let remaining: Vec<ExplorerState> = rest.collect();

        let (hub, watch_events) = match WatchHub::new() {
            Ok((hub, events)) => (Some(hub), Some(events)),
            Err(error) => {
                log::warn!("live refresh is unavailable: {error}");
                (None, None)
            }
        };

        let toast_host = cx.new(|_cx| ToastHost::new());
        cx.set_global(ToastGlobal {
            host: toast_host.clone(),
        });
        let mut this = Self {
            tabs: TabList::new(first),
            next_tab_id: 1,
            home,
            favorites: favorites_of(favorites, &legacy_favorites),
            legacy_favorites,
            bookmarks: BookmarkSync::new(bookmarks_path),
            places: None,
            sidebar_error: None,
            // Known from the environment at once, so Del has a Trash even
            // before the places discovery answers.
            trash_root: startup_trash_root(
                std::env::var_os("HOME").as_deref(),
                std::env::var_os("XDG_DATA_HOME").as_deref(),
            ),
            shell_focus: cx.focus_handle(),
            entries_focus: cx.focus_handle(),
            list_widths: ListWidths::default(),
            entries_width: None,
            sidebar_open: !preferences.sidebar_collapsed,
            sidebar_width: preferences.sidebar_width as f32,
            details_width: preferences.details_width as f32,
            panel_drag: None,
            persistence,
            restore_pending,
            restore_fallbacks: 0,
            settings_notice: None,
            rendered_columns: 1,
            inspector_open: preferences.details_open,
            inspector_shown: false,
            progress: Registry::default(),
            op_contexts: HashMap::new(),
            undo: UndoStack::default(),
            renaming: None,
            menu: None,
            menu_focus: cx.focus_handle(),
            dialogs: DialogQueue::default(),
            dialog_focus: cx.focus_handle(),
            dialog_cancel_focus: cx.focus_handle(),
            dialog_confirm_focus: cx.focus_handle(),
            dialog_skip_focus: cx.focus_handle(),
            dialog_keep_focus: cx.focus_handle(),
            clipboard: ClipboardState::default(),
            clipboard_text: None,
            pastes: HashMap::new(),
            toast_host,
            toast_ids: RefCell::new(Vec::new()),
            toast_stack_height: Rc::new(Cell::new(0.0)),
            terminal: None,
            hub,
            watch_events,
            folder_debounce: HashMap::new(),
            _watch_pump: None,
            devices: DeviceHub::from_env(),
        };
        this._watch_pump = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(WATCH_STEP).await;
                if this
                    .update_in(cx, |this, window, cx| {
                        this.pump_watch_events(window, cx);
                        this.pump_devices(window, cx);
                        this.persist_tick(window, cx);
                    })
                    .is_err()
                {
                    return;
                }
            }
        }));

        // The terminal lookup reads `.desktop` files: off the UI thread. The
        // menus offer "Open in terminal" once it has answered.
        let terminal_task = cx
            .background_executor()
            .spawn(async move { terminal::resolve(&TerminalEnv::from_process()) });
        cx.spawn_in(window, async move |this, cx| {
            let found = terminal_task.await;
            if let Err(error) = this.update_in(cx, |this, _window, cx| {
                this.terminal = found;
                cx.notify();
            }) {
                log::debug!("the explorer closed before the terminal was found: {error}");
            }
        })
        .detach();

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

        this.attach_bookmark_watch();
        this.sync_window_title(window);
        this.navigate(Transition::Visit(initial), window, cx);
        // The rest of a restored session opens behind the first tab, in
        // order, and the saved active tab takes over.
        if !remaining.is_empty() {
            for state in remaining {
                let location = state.current().clone();
                if let Location::Directory(path) = &location {
                    this.restore_pending.insert(this.next_tab_id, path.clone());
                }
                this.open_tab_with(state, location, Placement::Background, window, cx);
            }
            this.activate_tab(active_tab, window, cx);
        }
        this.settings_notice = (!notices.is_empty()).then(|| notices.join(" \u{b7} "));
        this
    }

    /// The state that is saved between runs, as of now: the settings (the
    /// favorites, the shell, and the active tab's presentation as the
    /// default for new tabs) and the session (every tab).
    fn persist_snapshot(&self, window: &Window) -> (Settings, Session) {
        let active = self.tab();
        let size = window.bounds().size;
        let settings = Settings {
            version: SETTINGS_VERSION,
            favorites: self.legacy_favorites.clone(),
            preferences: Preferences {
                view: view_to_pref(active.state.view()),
                arrangement: arrangement_to_prefs(active.state.arrangement()),
                sidebar_width: self.sidebar_width.round() as u32,
                details_width: self.details_width.round() as u32,
                sidebar_collapsed: !self.sidebar_open,
                details_open: self.inspector_open,
                window_width: f32::from(size.width).round() as u32,
                window_height: f32::from(size.height).round() as u32,
            }
            .sanitized(),
        };
        let session = Session {
            version: SESSION_VERSION,
            active: self.tabs.active_index(),
            tabs: self
                .tabs
                .iter()
                .map(|tab| {
                    session_tab(
                        tab.state.current(),
                        tab.state.view(),
                        tab.state.arrangement(),
                    )
                })
                .collect(),
        };
        (settings, session)
    }

    /// Runs on every pump tick: saves the persisted state once it has stood
    /// still for the debounce. The write runs on the background executor; a
    /// failure shows one status notice and never blocks.
    fn persist_tick(&mut self, window: &Window, cx: &mut Context<Self>) {
        let (settings, session) = self.persist_snapshot(window);
        let Some(job) = self.persistence.observe(settings, session, Instant::now()) else {
            return;
        };
        let write = cx.background_executor().spawn(async move { job.run() });
        cx.spawn(async move |this, cx| {
            let report = write.await;
            if let Err(error) = this.update(cx, |this, cx| {
                if let Some(notice) = this.persistence.finished(&report, Instant::now()) {
                    this.settings_notice = Some(notice);
                    cx.notify();
                }
            }) {
                log::debug!("the explorer closed before its save settled: {error}");
            }
        })
        .detach();
    }

    /// Saves what is still pending, on the spot: the window is closing and
    /// the process ends right after, so there is no later to wait for.
    pub(crate) fn flush_persistence(&mut self, window: &Window) {
        let (settings, session) = self.persist_snapshot(window);
        if let Some(job) = self.persistence.flush(settings, session) {
            for error in job.run().errors() {
                log::warn!("settings were not saved on close: {error}");
            }
        }
    }

    /// Handles the path field of tab `id`: Enter, typing and blur.
    fn path_event(
        &mut self,
        id: TabId,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active = id == self.active_id();
        match event {
            InputEvent::PressEnter { .. } => {
                if active {
                    self.submit_path(window, cx);
                }
            }
            InputEvent::Change => {
                let Some(tab) = self.tabs.by_id_mut(id) else {
                    return;
                };
                let text = tab.path_input.read(cx).value().to_string();
                tab.path_edit_revision += 1;
                tab.path_edited = text != tab.path_seed;
                tab.state.clear_path_error();
                cx.notify();
            }
            InputEvent::Blur => {
                // A tab left in the background keeps its path edit: the
                // field only loses the focus because another tab took over.
                if !active {
                    return;
                }
                if self.tab().path_edited {
                    self.seed_path_input(id, window, cx, false);
                }
                // Leaving the field returns to the breadcrumbs.
                self.tab_mut().path_editing = false;
                cx.notify();
            }
            InputEvent::Focus => {}
        }
    }

    /// The current-location label, as the window title and the content head
    /// show it: `Home` for the home directory, `Home Trash` for the home
    /// Trash, else the folder name (or `/` for the filesystem root).
    pub(crate) fn current_label(&self) -> String {
        match self.tab().state.current() {
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

    /// Navigates the active tab: turns the transition into a scan and starts
    /// it off the UI thread. A transition that changes nothing produces no
    /// scan. Returns the started scan, so a path submit can remember it.
    pub(crate) fn navigate(
        &mut self,
        transition: Transition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Scan> {
        // A device that is being ejected is not entered, whichever way the
        // folder is reached (a visit, a history step, Up): a history entry
        // may be a symlink into it, which is resolved off the UI thread, so
        // its scan starts a moment later and is not returned.
        if self.devices.is_draining()
            && let Some(Location::Directory(path)) = self.tab().state.target_of(&transition)
        {
            self.when_not_ejecting(vec![path], window, cx, move |this, window, cx| {
                this.navigate_now(transition, window, cx);
            });
            return None;
        }
        self.navigate_now(transition, window, cx)
    }

    fn navigate_now(
        &mut self,
        transition: Transition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Scan> {
        self.settings_notice = None;
        let scan = self.tab_mut().state.begin(transition);
        self.start_scan(scan.clone(), window, cx);
        scan
    }

    /// Starts the active tab's scan (see [`Self::start_scan_in`]).
    pub(crate) fn start_scan(
        &mut self,
        scan: Option<Scan>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.active_id();
        self.start_scan_in(id, scan, window, cx);
    }

    /// Starts tab `id`'s scan (risk item 4): the blocking read runs on the
    /// background executor, the completion re-enters the view through
    /// [`Self::finish_scan`], and the task is retained by the tab so a newer
    /// scan replaces it.
    pub(crate) fn start_scan_in(
        &mut self,
        id: TabId,
        scan: Option<Scan>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(scan) = scan else {
            return;
        };
        let location = scan.location.clone();
        let trash_root = self.trash_root.clone();
        let load = cx
            .background_executor()
            .spawn(async move { load_indexed(&location, trash_root.as_deref()) });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = load.await;
            if let Err(error) = this.update_in(cx, |this, window, cx| {
                this.finish_scan(id, scan, result, window, cx);
            }) {
                log::debug!("the explorer closed before its scan settled: {error}");
            }
        });
        if let Some(tab) = self.tabs.by_id_mut(id) {
            tab.scan_task = Some(task);
        }
        // From now on a change in the folder being read is not missed.
        self.sync_watches(id);
        cx.notify();
    }

    /// Settles a finished scan of tab `id`. A stale result (a newer scan
    /// has started, including a newer failed one) and a result for a tab
    /// that closed meanwhile change nothing. A commit re-seeds the path
    /// field and, for the active tab, the window title; a failure only
    /// reports.
    fn finish_scan(
        &mut self,
        id: TabId,
        scan: Scan,
        result: io::Result<Loaded>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settle_scan(id, scan, result, window, cx);
        // The committed folder may have changed and the navigation is over.
        self.sync_watches(id);
    }

    fn settle_scan(
        &mut self,
        id: TabId,
        scan: Scan,
        result: io::Result<Loaded>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active = id == self.active_id();
        // A restored tab's first scan decides whether its saved folder is
        // still there; any other scan of the tab has nothing to decide.
        let unverified = if self
            .restore_pending
            .get(&id)
            .is_some_and(|saved| scan.location == Location::Directory(saved.clone()))
        {
            self.restore_pending.remove(&id)
        } else {
            None
        };
        let Some(tab) = self.tabs.by_id_mut(id) else {
            return;
        };
        if let (Err(error), Some(saved)) = (&result, unverified)
            && error.kind() == io::ErrorKind::NotFound
            && tab
                .state
                .commit_failure(scan.clone(), "the folder no longer exists".to_owned())
        {
            self.restore_fallback(id, saved, window, cx);
            cx.notify();
            return;
        }
        let Some(tab) = self.tabs.by_id_mut(id) else {
            return;
        };
        match result {
            Ok(Loaded {
                listing,
                index,
                snapshot,
                folder_label,
            }) => {
                if !tab.state.commit_indexed(scan.clone(), listing, index) {
                    return;
                }
                tab.snapshot = snapshot;
                tab.folder_label = folder_label;
                if scan.live {
                    // A background refresh only swaps the listing: focus,
                    // path editing, scroll position and the filter stay.
                    log::debug!(
                        "tab {id} ({}) now lists {} entries of {:?}",
                        if active { "active" } else { "background" },
                        self.tabs.by_id_mut(id).map_or(0, |tab| tab.snapshot.len()),
                        scan.location
                    );
                    self.start_child_counts(id, cx);
                    if let Some(tab) = self.tabs.by_id_mut(id) {
                        tab.permissions = None;
                    }
                    self.after_listing_changed(id, window, cx);
                    // A change that landed while this refresh was reading.
                    let postponed = self
                        .tabs
                        .by_id_mut(id)
                        .and_then(|tab| tab.state.begin_deferred_op_refresh());
                    self.start_scan_in(id, postponed, window, cx);
                    cx.notify();
                    return;
                }
                // A genuine folder change returns the reader to the top of
                // the presentation that is showing; a reload keeps the
                // reader at the place they were reading.
                if !scan.refresh {
                    self.scroll_to_top(id, cx);
                }
                self.start_child_counts(id, cx);
                let Some(tab) = self.tabs.by_id_mut(id) else {
                    return;
                };
                // A reload may have changed the modes: probe again.
                tab.permissions = None;
                // Consume the submitted edit only when this completion is
                // its own scan with no newer edit after it; stale or failed
                // completions never reseed either field.
                let revision = tab.path_edit_revision;
                let settled = tab
                    .submitted_edit
                    .take()
                    .is_some_and(|edit| edit.settles(scan.generation, revision));
                self.seed_path_input(id, window, cx, settled);
                if settled {
                    if let Some(tab) = self.tabs.by_id_mut(id) {
                        tab.path_editing = false;
                    }
                    if active {
                        self.entries_focus.focus(window, cx);
                    }
                }
                self.sync_search_input(id, window, cx);
                self.after_listing_changed(id, window, cx);
                // An operation that finished while this navigation was
                // pending refreshes the folder now shown.
                let postponed = self
                    .tabs
                    .by_id_mut(id)
                    .and_then(|tab| tab.state.begin_deferred_op_refresh());
                self.start_scan_in(id, postponed, window, cx);
                cx.notify();
            }
            Err(error) if scan.live => {
                // The folder became unreadable or vanished mid-scan. A
                // watched folder's vanishing is reported by the watcher; a
                // folder that vanished before its watch existed (or while
                // the refresh was reading) falls back here the same way.
                let location = scan.location.clone();
                // A stale refresh (the tab has navigated since) must not
                // override that navigation with a fallback.
                let current = tab
                    .state
                    .commit_failure(scan, "the folder could not be refreshed".to_owned());
                if current
                    && error.kind() == io::ErrorKind::NotFound
                    && let Location::Directory(gone) = location
                {
                    self.watched_folder_gone(id, &gone, window, cx);
                }
                cx.notify();
            }
            Err(error) => {
                tab.submitted_edit = None;
                if !tab.state.commit_failure(scan, error.to_string()) {
                    return;
                }
                log::warn!("directory scan failed: {error}");
                // The folder still shown is what the operation changed.
                let postponed = tab.state.begin_deferred_op_refresh();
                self.start_scan_in(id, postponed, window, cx);
                cx.notify();
            }
        }
        if active {
            self.sync_window_title(window);
        }
    }

    /// Points tab `id`'s watch holds at the folder it shows and the folder
    /// a navigation in flight is reading (the Trash is not watched). Tabs on
    /// the same folder share one inotify watch inside the hub.
    fn sync_watches(&mut self, id: TabId) {
        let Some(hub) = self.hub.as_ref() else {
            return;
        };
        let Some(tab) = self.tabs.by_id_mut(id) else {
            return;
        };
        let committed = match tab.state.current() {
            Location::Directory(path) => Some(path.clone()),
            Location::Trash => None,
        };
        let target = tab
            .state
            .pending_directory()
            .map(Path::to_path_buf)
            .filter(|path| committed.as_ref() != Some(path));
        TabWatch::retarget(&mut tab.watch.committed, committed, hub);
        TabWatch::retarget(&mut tab.watch.target, target, hub);
    }

    /// Collects what the watcher thread reported since the last call and
    /// acts on it for every tab, active or not: a vanished folder sends the
    /// tabs showing it to the nearest existing ancestor; a change restarts
    /// the folder's debounce, and once it settles every tab on that folder
    /// (or navigating into it) re-reads it, keeping selection, filter and
    /// scroll.
    fn pump_watch_events(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = Instant::now();
        let mut gone = Vec::new();
        let mut stopped = None;
        if let Some(events) = &self.watch_events {
            loop {
                match events.try_recv() {
                    Ok((path, FolderEvent::Changed)) => {
                        self.folder_debounce.entry(path).or_default().note(now);
                    }
                    Ok((path, FolderEvent::Gone)) => gone.push(path),
                    Err(error) => {
                        stopped = Some(error);
                        break;
                    }
                }
            }
        }
        if let Some(notice) = stopped.and_then(|error| stop_notice(error, false)) {
            self.watcher_stopped(notice, cx);
        }
        for path in gone {
            self.folder_debounce.remove(&path);
            self.bookmarks_folder_gone(&path, cx);
            let ids: Vec<TabId> = self
                .tabs
                .iter()
                .filter(|tab| tab.state.current() == &Location::Directory(path.clone()))
                .map(|tab| tab.id)
                .collect();
            for id in ids {
                self.watched_folder_gone(id, &path, window, cx);
            }
        }
        let due: Vec<PathBuf> = self
            .folder_debounce
            .iter()
            .filter(|(_, debounce)| debounce.ready(now))
            .map(|(path, _)| path.clone())
            .collect();
        for path in due {
            self.folder_debounce.remove(&path);
            self.bookmarks_folder_changed(&path, cx);
            let active = self.active_id();
            let ids: Vec<TabId> = self.tabs.iter().map(|tab| tab.id).collect();
            for id in ids {
                let scan = self
                    .tabs
                    .by_id_mut(id)
                    .and_then(|tab| tab.state.folder_changed(&path));
                if scan.is_some() {
                    log::debug!(
                        "live refresh of {} in tab {id} ({})",
                        path.display(),
                        if id == active { "active" } else { "background" }
                    );
                }
                self.start_scan_in(id, scan, window, cx);
            }
        }
    }

    /// The folders under a watch right now, as the watches name them.
    pub(crate) fn watched_paths(&self) -> Vec<PathBuf> {
        let held = self
            .tabs
            .iter()
            .flat_map(|tab| [&tab.watch.committed, &tab.watch.target])
            .flatten()
            .map(|handle| handle.path().to_path_buf());
        held.chain(self.folder_debounce.keys().cloned()).collect()
    }

    /// Lets go of the watches on `doomed` (see [`Self::watched_paths`]), so
    /// an inotify watch cannot keep a mount busy.
    pub(crate) fn release_watches(&mut self, doomed: &[PathBuf]) {
        for tab in self.tabs.iter_mut() {
            for slot in [&mut tab.watch.committed, &mut tab.watch.target] {
                if slot
                    .as_ref()
                    .is_some_and(|handle| doomed.iter().any(|path| path == handle.path()))
                {
                    *slot = None;
                }
            }
        }
        self.folder_debounce
            .retain(|path, _| !doomed.contains(path));
    }

    /// The watcher thread died: drop the hub and every hold, so tabs go back
    /// to re-reading on activation, and tell the user once.
    fn watcher_stopped(&mut self, notice: &str, cx: &mut Context<Self>) {
        log::warn!("the watcher thread stopped; live updates are off");
        self.hub = None;
        self.watch_events = None;
        self.folder_debounce.clear();
        for tab in self.tabs.iter_mut() {
            tab.watch = TabWatch::default();
        }
        self.tab_mut().state.set_notice(notice.to_owned());
        cx.notify();
    }

    /// The saved folder of restored tab `id` is gone: finds the nearest
    /// existing ancestor off the UI thread, then opens it in that tab with a
    /// status notice.
    fn restore_fallback(
        &mut self,
        id: TabId,
        saved: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let home = self.home.clone();
        let missing = saved.clone();
        let probe = cx.background_executor().spawn(async move {
            settings::restore_fallback(&missing, home.as_deref(), Path::is_dir)
        });
        cx.spawn_in(window, async move |this, cx| {
            let fallback = probe.await;
            if let Err(error) = this.update_in(cx, |this, window, cx| {
                let Some(tab) = this.tabs.by_id_mut(id) else {
                    return;
                };
                // The user may have navigated away while the probe ran.
                if tab.state.current() != &Location::Directory(saved.clone()) {
                    return;
                }
                this.restore_fallbacks += 1;
                this.settings_notice = Some(restore_fallback_notice(this.restore_fallbacks));
                let notice = format!(
                    "{} no longer exists; showing {}",
                    saved.display(),
                    fallback.display()
                );
                let scan = tab.state.begin_fallback(fallback, notice);
                this.start_scan_in(id, scan, window, cx);
                cx.notify();
            }) {
                log::debug!("the explorer closed before the restore fallback: {error}");
            }
        })
        .detach();
    }

    /// The folder tab `id` shows was deleted or moved away: fall back to the
    /// nearest existing ancestor with a status message, like a failed scan
    /// would.
    fn watched_folder_gone(
        &mut self,
        id: TabId,
        gone: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.by_id_mut(id) else {
            return;
        };
        if tab.state.current() != &Location::Directory(gone.to_path_buf()) {
            return;
        }
        let Some(ancestor) = nearest_existing_ancestor(gone) else {
            return;
        };
        let notice = format!(
            "{} no longer exists; showing {}",
            gone.display(),
            ancestor.display()
        );
        log::debug!(
            "{} is gone: tab {id} falls back to {}",
            gone.display(),
            ancestor.display()
        );
        let scan = tab.state.begin_fallback(ancestor, notice);
        self.start_scan_in(id, scan, window, cx);
    }

    /// Counts the entries of every folder of tab `id`'s committed listing on
    /// the background executor, then publishes them in one step. A newer
    /// commit replaces the task (dropping it), so counts never leak across
    /// listings.
    fn start_child_counts(&mut self, id: TabId, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.by_id_mut(id) else {
            return;
        };
        tab.state.set_child_counts(Arc::new(HashMap::new()));
        let entries = Arc::clone(&tab.snapshot);
        let has_folders = entries.iter().any(fileflux::core::Entry::is_directory);
        if !has_folders {
            tab.counts_task = None;
        }
        // The fresh listing's order, once, with its (empty) counts.
        self.arrange_tab(id, cx);
        if !has_folders {
            return;
        }
        let counting = cx
            .background_executor()
            .spawn(async move { fileflux::core::fs::child_counts(&entries) });
        let task = cx.spawn(async move |this, cx| {
            let counts = counting.await;
            if let Err(error) = this.update(cx, |this, cx| {
                if let Some(tab) = this.tabs.by_id_mut(id) {
                    tab.state.set_child_counts(Arc::new(counts));
                }
                this.arrange_tab(id, cx);
                cx.notify();
            }) {
                log::debug!("the explorer closed before its folder counts settled: {error}");
            }
        });
        if let Some(tab) = self.tabs.by_id_mut(id) {
            tab.counts_task = Some(task);
        }
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

    /// Makes tab `id`'s displayed order current after a commit or a change
    /// of its query, sort, filters or folder counts. A small folder is
    /// arranged right here; a large one gets a job on the background
    /// executor while the previous order stays on screen. The job loop
    /// ends with the order of the latest inputs: a stale result is dropped
    /// and the next job starts from what the inputs became meanwhile.
    pub(crate) fn arrange_tab(&mut self, id: TabId, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.by_id_mut(id) else {
            return;
        };
        let was_arranging = tab.state.is_arranging();
        let Some(first) = tab.state.refresh_arrangement() else {
            if !tab.state.is_arranging() {
                // Arranged here, or already current: no job is wanted.
                tab.arrange_task = None;
                tab.arranging_since = None;
                tab.arrange_hint_task = None;
            }
            cx.notify();
            return;
        };
        if !was_arranging {
            tab.arranging_since = Some(Instant::now());
            // The indicator only shows once the wait is noticeable.
            tab.arrange_hint_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(ARRANGE_HINT_DELAY).await;
                if let Err(error) = this.update(cx, |_, cx| cx.notify()) {
                    log::debug!("the explorer closed before its sorting hint: {error}");
                }
            }));
        }
        let task = cx.spawn(async move |this, cx| {
            let mut job = first;
            loop {
                let output = cx
                    .background_executor()
                    .spawn(async move { job.run() })
                    .await;
                let next = this.update(cx, |this, cx| {
                    cx.notify();
                    let tab = this.tabs.by_id_mut(id)?;
                    let next = tab.state.settle_arrange(output);
                    if next.is_none() {
                        tab.arranging_since = None;
                        tab.arrange_hint_task = None;
                    }
                    next
                });
                match next {
                    Ok(Some(next)) => job = next,
                    Ok(None) => break,
                    Err(error) => {
                        log::debug!("the explorer closed before its arrangement settled: {error}");
                        break;
                    }
                }
            }
        });
        tab.arrange_task = Some(task);
        cx.notify();
    }

    /// Seeds tab `id`'s path field with its committed location unless the
    /// user is mid-edit. Unedited submits therefore retain the actual
    /// current path. `force` reseeds even while focused, for a
    /// just-settled submission, so a second Enter reloads the displayed
    /// location; post-submission edits keep the guard.
    fn seed_path_input(
        &mut self,
        id: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
        force: bool,
    ) {
        let Some(tab) = self.tabs.by_id_mut(id) else {
            return;
        };
        let committed = committed_path_text(tab.state.current());
        let focused = tab.path_input.read(cx).focus_handle(cx).is_focused(window);
        if !force && focused && tab.path_edited {
            return;
        }
        tab.path_seed.clone_from(&committed);
        tab.path_edited = false;
        let seed = committed;
        tab.path_input.update(cx, |input, cx| {
            let shown = input.value().to_string();
            if shown != seed {
                input.set_value(seed, window, cx);
            }
        });
    }

    /// Synchronizes tab `id`'s search field with its committed query after
    /// a commit, so the field, the listing and the match count agree again.
    /// `set_value` emits no Change; no query-subscription feedback.
    fn sync_search_input(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.by_id_mut(id) else {
            return;
        };
        let committed = tab.state.query().to_owned();
        tab.search_input.update(cx, |input, cx| {
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
    /// Switches the breadcrumbs to the editable path field (Ctrl+L, or a
    /// click on the bar's empty space) and focuses it once it is drawn.
    pub(crate) fn focus_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tab_mut().path_edited = false;
        self.tab_mut().path_editing = true;
        cx.notify();
        cx.defer_in(window, |this, window, cx| {
            this.tab().path_input.update(cx, |input, cx| {
                input.focus(window, cx);
                input.select_all(window, cx);
            });
        });
    }

    /// Focuses the search field and selects its content.
    fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tab().search_input.update(cx, |input, cx| {
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
        self.tab_mut().path_edited = false;
        self.tab_mut().path_editing = false;
        self.tab_mut().path_seed = committed_path_text(self.tab().state.current());
        let seed = self.tab().path_seed.clone();
        self.tab().path_input.update(cx, |input, cx| {
            input.set_value(seed, window, cx);
        });
        self.tab_mut().state.cancel_path_edit();
        self.entries_focus.focus(window, cx);
        cx.notify();
    }

    /// A press outside the path field (a click on bare chrome never moves
    /// the focus, so no blur would tell): returns to the breadcrumbs,
    /// restoring the committed path, like Escape but without stealing a
    /// focus the press itself is about to give elsewhere.
    pub(crate) fn leave_path_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tab().path_editing {
            return;
        }
        let was_focused = self
            .tab()
            .path_input
            .read(cx)
            .focus_handle(cx)
            .is_focused(window);
        self.tab_mut().path_edited = false;
        self.tab_mut().path_editing = false;
        self.tab_mut().path_seed = committed_path_text(self.tab().state.current());
        let seed = self.tab().path_seed.clone();
        self.tab().path_input.update(cx, |input, cx| {
            input.set_value(seed, window, cx);
        });
        self.tab_mut().state.cancel_path_edit();
        if was_focused {
            self.entries_focus.focus(window, cx);
        }
        cx.notify();
    }

    /// Escape in the search field (captured by the field's frame):
    /// clears the query — the full listing returns, the no-result state
    /// goes away — and gives the keyboard back to the entries area.
    pub(crate) fn clear_search_on_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tab_mut().state.clear_query();
        let id = self.active_id();
        self.arrange_tab(id, cx);
        self.tab().search_input.update(cx, |input, cx| {
            input.set_value(String::new(), window, cx);
        });
        self.entries_focus.focus(window, cx);
        cx.notify();
    }

    /// Escape anywhere else in the shell: drops the selection and returns
    /// the focus to the shell, so a second Escape never reaches the
    /// window close path by accident.
    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Escape while dragging cancels the drag and nothing else.
        if cx.stop_active_drag(window) {
            return;
        }
        if !self.tab().state.selection().is_empty() {
            self.tab_mut().state.clear_selection();
        }
        self.shell_focus.focus(window, cx);
        cx.notify();
    }

    /// Submits the path field. An unedited field submits the committed
    /// location (a reload); an edited one resolves its text through
    /// [`ExplorerState::resolve_input`] — `~`, absolute and relative
    /// forms, no shell.
    pub(crate) fn submit_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.tab().path_input.read(cx).value().to_string();
        if !self.tab().path_edited || text == self.tab().path_seed {
            // Unedited: the submit retains the actual current path.
            let current = self.tab().state.current().clone();
            self.tab_mut().state.clear_path_error();
            self.tab_mut().path_editing = false;
            self.entries_focus.focus(window, cx);
            self.navigate(Transition::Visit(current), window, cx);
            return;
        }
        match self.tab().state.resolve_input(&text) {
            Ok(location) => {
                self.tab_mut().state.clear_path_error();
                self.tab_mut().state.clear_notice();
                let revision = self.tab().path_edit_revision;
                let scan = self.navigate(Transition::Visit(location), window, cx);
                if let Some(scan) = scan {
                    self.tab_mut().submitted_edit = Some(SubmittedEdit {
                        generation: scan.generation,
                        revision,
                    });
                }
            }
            Err(argument) => {
                self.tab_mut().state.clear_notice();
                self.tab_mut().state.set_path_error(argument);
                cx.notify();
            }
        }
    }

    /// Moves the keyboard selection by `delta` visible positions and
    /// scrolls the row it lands on into view.
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.tab().state.visible_shared().len();
        if count == 0 {
            return;
        }
        let last = count - 1;
        let current = self.tab().state.cursor_position();
        let target = match current {
            Some(position) => (position as isize + delta).clamp(0, last as isize) as usize,
            None => 0,
        };
        self.tab_mut().state.select_visible(target);
        self.scroll_selected_row(target, cx);
        cx.notify();
    }

    /// Shift+Arrow: extends the selection by `delta` visible positions.
    fn extend_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.tab_mut().state.extend_selection_by(delta);
        if let Some(position) = self.tab().state.cursor_position() {
            self.scroll_selected_row(position, cx);
        }
        cx.notify();
    }

    /// Shift+Home / Shift+End: extends the selection to `position`.
    fn extend_selection_to_extreme(&mut self, position: usize, cx: &mut Context<Self>) {
        self.tab_mut().state.extend_selection_to(position);
        if let Some(position) = self.tab().state.cursor_position() {
            self.scroll_selected_row(position, cx);
        }
        cx.notify();
    }

    fn select_extreme(&mut self, position: usize, cx: &mut Context<Self>) {
        let count = self.tab().state.visible_shared().len();
        if count == 0 {
            return;
        }
        self.tab_mut().state.select_visible(position);
        self.scroll_selected_row(position, cx);
        cx.notify();
    }

    pub(crate) fn scroll_selected_row(&mut self, position: usize, cx: &mut Context<Self>) {
        match self.tab().state.view() {
            View::List => {
                // Reveal only the nearest edge, preserving visible context
                // when keyboard selection crosses the viewport boundary.
                self.tab().table.update(cx, |table, cx| {
                    table
                        .vertical_scroll_handle
                        .scroll_to_item(position, ScrollStrategy::Nearest);
                    cx.notify();
                });
            }
            View::Grid => {
                self.tab().grid_scroll.scroll_to_item(
                    util::grid_row(position, self.rendered_columns.max(1)),
                    ScrollStrategy::Nearest,
                );
            }
        }
    }

    /// Returns tab `id`'s presentation that is showing to its top: the
    /// details table scrolls its own rows, the tile grid its own handle.
    fn scroll_to_top(&mut self, id: TabId, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.by_id_mut(id) else {
            return;
        };
        match tab.state.view() {
            View::List => {
                tab.table.update(cx, |table, cx| table.scroll_to_row(0, cx));
            }
            View::Grid => {
                tab.grid_scroll.scroll_to_item(0, ScrollStrategy::Top);
            }
        }
    }

    /// Switches the presentation. The other view's handle keeps an offset
    /// counted in the wrong row units, so the fresh view starts at its top;
    /// the selection lives in the state and survives untouched.
    pub(crate) fn set_view(&mut self, view: View, cx: &mut Context<Self>) {
        if self.tab().state.view() == view {
            return;
        }
        self.tab_mut().state.set_view(view);
        let id = self.active_id();
        self.scroll_to_top(id, cx);
        cx.notify();
    }

    /// Enter (or double click) on the selected entry: directories navigate,
    /// everything else opens with the default application. Only **visible**
    /// selected entries activate: a selection the current filter has
    /// hidden is never silently opened (see
    /// [`ExplorerState::activation_candidate`]).
    pub(crate) fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Enter belongs to the rename editor while it is open.
        if self.renaming.is_some() {
            return;
        }
        let Some(path) = self.tab().state.activation_candidate() else {
            return;
        };
        self.open_path(&path, window, cx);
    }

    /// Opens one entry: a directory, or a symlink to one, navigates (to the
    /// path as scanned, so the link stays in the path bar); a file, a link to
    /// a file or a broken link opens with the default application
    /// (`xdg-open`, see [`Explorer::open_file`]); see
    /// [`crate::explorer::ops::open_action`].
    pub(crate) fn open_path(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.tab().snapshot.iter().find(|entry| entry.path == path) else {
            return;
        };
        if open_action(entry) == OpenAction::Navigate {
            self.navigate(
                Transition::Visit(Location::Directory(entry.path.clone())),
                window,
                cx,
            );
            return;
        }
        let entry_path = entry.path.clone();
        self.open_file(&entry_path, cx);
    }

    /// Toggles the favorites membership of the current directory.
    pub(crate) fn toggle_favorite(&mut self, cx: &mut Context<Self>) {
        let current = self.tabs.active().state.current().clone();
        self.toggle_favorite_of(&current, cx);
    }

    /// The width the content island can use, as the **first-frame estimate**
    /// of the entries area's inner width, derived before the measuring
    /// canvas has reported: the 8 px outer gutters, the sidebar column, the
    /// inter-island gap and the island's 16 px content padding.
    fn content_width(&self, window: &Window) -> f32 {
        let total = f32::from(window.viewport_size().width);
        let panel = if self.inspector_wanted(window) {
            self.details_width
        } else {
            0.0
        };
        (total - self.shell_width(window) - panel - 2.0 * 16.0).max(0.0)
    }

    /// Everything in the window that is not the document island: the outer
    /// gutters and the docked sidebar with its gap.
    pub(crate) fn shell_width(&self, window: &Window) -> f32 {
        let narrow = f32::from(window.viewport_size().width) < NARROW_WIDTH;
        let sidebar = if self.sidebar_open && !narrow {
            self.sidebar_width + IslandMetrics::GAP.as_f32()
        } else {
            0.0
        };
        2.0 * IslandMetrics::GAP.as_f32() + sidebar
    }

    /// Whether the details panel draws at this window width.
    fn inspector_wanted(&self, window: &Window) -> bool {
        inspector::panel_visible(
            self.inspector_open,
            f32::from(window.viewport_size().width),
            self.shell_width(window),
            self.details_width,
        )
    }
}

// --- tabs ---------------------------------------------------------------------

impl Explorer {
    /// Opens `location` in a new tab to the right of the active one. A
    /// foreground tab takes over at once; a background tab loads quietly
    /// and the active tab keeps the keyboard. The new tab starts with the
    /// presentation (view and sort) of the tab it was opened from.
    pub(crate) fn open_tab(
        &mut self,
        location: Location,
        placement: Placement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A device that is being ejected is not opened in a new tab either.
        let Location::Directory(path) = &location else {
            return self.open_tab_checked(location, placement, window, cx);
        };
        let path = path.clone();
        self.when_not_ejecting(vec![path], window, cx, move |this, window, cx| {
            this.open_tab_checked(location, placement, window, cx);
        });
    }

    fn open_tab_checked(
        &mut self,
        location: Location,
        placement: Placement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = self.tab().state.for_new_tab(location.clone());
        self.open_tab_with(state, location, placement, window, cx);
    }

    /// [`Self::open_tab`] with the state the tab starts from: a new tab
    /// inherits the active tab's, a restored one brings its own.
    fn open_tab_with(
        &mut self,
        state: ExplorerState,
        location: Location,
        placement: Placement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let mut tab = Tab::new(id, state, window, cx);
        let scan = tab.state.begin(Transition::Visit(location));
        self.tabs.open(tab, placement);
        self.start_scan_in(id, scan, window, cx);
        if placement == Placement::Foreground {
            self.after_tab_switch(false, window, cx);
        }
        cx.notify();
    }

    /// Ctrl+T: a new tab on the folder the active tab shows.
    pub(crate) fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let here = self.tab().state.current().clone();
        self.open_tab(here, Placement::Foreground, window, cx);
    }

    /// Middle-click or the menu on an entry: a real directory opens in a
    /// background tab; files and symlinks do nothing.
    pub(crate) fn open_path_in_new_tab(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.tab().snapshot.iter().find(|entry| entry.path == path) else {
            return;
        };
        if entry.is_directory() && !entry.is_symlink {
            let location = Location::Directory(entry.path.clone());
            self.open_tab(location, Placement::Background, window, cx);
        }
    }

    /// The context menu's "Open in new tab" on the selected folder.
    pub(crate) fn open_selected_in_new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.tab().state.activation_candidate() {
            self.open_path_in_new_tab(&path, window, cx);
        }
    }

    /// Closes the tab at `index`. The last tab stays. Closing the active tab
    /// activates its right neighbour, else the left one.
    pub(crate) fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let previous = self.active_id();
        let Some(closed) = self.tabs.close(index) else {
            return;
        };
        // Dropping the tab ends its scan and probe tasks and releases its
        // watch holds (the folder's inotify watch goes with the last one).
        let closed_active = closed.id == previous;
        drop(closed);
        if closed_active {
            self.after_tab_switch(true, window, cx);
        }
        cx.notify();
    }

    /// Ctrl+W: closes the active tab.
    pub(crate) fn close_active_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let index = self.tabs.active_index();
        self.close_tab(index, window, cx);
    }

    /// Makes the tab at `index` the active one.
    pub(crate) fn activate_tab(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = self.active_id();
        if self.tabs.activate(index) && self.active_id() != previous {
            self.after_tab_switch(true, window, cx);
        }
    }

    /// Ctrl+Tab / Ctrl+Shift+Tab / Ctrl+PageDown / Ctrl+PageUp.
    pub(crate) fn cycle_tab(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let previous = self.active_id();
        self.tabs.cycle(step);
        if self.active_id() != previous {
            self.after_tab_switch(true, window, cx);
        }
    }

    /// Alt+1..9: the tab with that number.
    pub(crate) fn jump_to_tab(
        &mut self,
        number: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = self.active_id();
        if self.tabs.jump(number) && self.active_id() != previous {
            self.after_tab_switch(true, window, cx);
        }
    }

    /// What a change of the active tab does around the tabs themselves:
    ///
    /// - every tab keeps its own watch holds, so nothing changes hands here;
    ///   the tab being shown is re-read only when events cannot have kept it
    ///   current: the Trash (not watched) or a folder whose watch failed.
    ///   `refresh` is off for a tab that is being loaded for the first time
    ///   anyway;
    /// - the inline rename and the context menu belong to the tab they
    ///   started in and are dropped; dialogs are modal over the window and
    ///   stay;
    /// - the window title follows the tab and the keyboard goes to its
    ///   entries — or to its path field when it was mid path edit.
    fn after_tab_switch(&mut self, refresh: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.renaming = None;
        self.menu = None;
        let id = self.active_id();
        if refresh && self.tab().watch.committed.is_none() {
            let scan = self.tab_mut().state.begin_op_refresh();
            self.start_scan_in(id, scan, window, cx);
        }
        self.sync_window_title(window);
        if self.tab().path_editing {
            let input = self.tab().path_input.clone();
            cx.defer_in(window, move |_this, window, cx| {
                input.update(cx, |input, cx| input.focus(window, cx));
            });
        } else {
            self.entries_focus.focus(window, cx);
        }
        cx.notify();
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

/// The path text a committed location displays (lossy); typed edits and
/// submits go through the raw-state resolver instead.
fn committed_path_text(location: &Location) -> String {
    match location {
        Location::Directory(path) => path.to_string_lossy().into_owned(),
        Location::Trash => "Trash".to_owned(),
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

        let document = self.render_document_island(window, cx);
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
            Some(sidebar) if !narrow => {
                // The handle sits in the gap between the two islands, so it
                // never covers either one's content.
                let handle = sidebar::resize_handle(
                    "explorer-sidebar-resize",
                    Panel::Sidebar,
                    self.panel_drag
                        .is_some_and(|drag| drag.panel == Panel::Sidebar),
                    cx,
                )
                .left(px(IslandMetrics::GAP.as_f32()
                    + self.sidebar_width
                    + (IslandMetrics::GAP.as_f32() - sidebar::HANDLE_HIT) / 2.0));
                middle = middle.child(sidebar).child(document).child(handle);
            }
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

        let weak = cx.entity().downgrade();
        let cards = progress::render_cards(&self.progress, Instant::now(), &weak, cx);
        let overlays = overlay::render(self, window, cx);
        div()
            .key_context(EXPLORER_CONTEXT)
            .track_focus(&self.shell_focus)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.desk)
            .text_color(colors.foreground)
            .child(self.render_header())
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
                let current = this.tab().state.current().clone();
                this.navigate(Transition::Visit(current), window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleInspector, _window, cx| {
                this.inspector_open = !this.inspector_open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleHidden, _window, cx| {
                this.arrange(ArrangeCommand::ToggleHidden, cx);
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
                if this.tab().state.view() == View::List {
                    return;
                }
                this.move_selection(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectRight, _window, cx| {
                if this.tab().state.view() == View::List {
                    return;
                }
                this.move_selection(1, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectFirstEntry, _window, cx| {
                this.select_extreme(0, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectLastEntry, _window, cx| {
                let count = this.tab().state.visible_shared().len();
                this.select_extreme(count.saturating_sub(1), cx);
            }))
            .on_action(cx.listener(|this, _: &ExtendUp, _window, cx| {
                let columns = this.grid_step();
                this.extend_selection(-columns, cx);
            }))
            .on_action(cx.listener(|this, _: &ExtendDown, _window, cx| {
                let columns = this.grid_step();
                this.extend_selection(columns, cx);
            }))
            .on_action(cx.listener(|this, _: &ExtendLeft, _window, cx| {
                if this.tab().state.view() != View::List {
                    this.extend_selection(-1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ExtendRight, _window, cx| {
                if this.tab().state.view() != View::List {
                    this.extend_selection(1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ExtendToFirst, _window, cx| {
                this.extend_selection_to_extreme(0, cx);
            }))
            .on_action(cx.listener(|this, _: &ExtendToLast, _window, cx| {
                let count = this.tab().state.visible_shared().len();
                this.extend_selection_to_extreme(count.saturating_sub(1), cx);
            }))
            .on_action(cx.listener(|this, _: &SelectAll, _window, cx| {
                this.tab_mut().state.select_all_visible();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenSelected, window, cx| {
                this.open_selected(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewFolder, window, cx| {
                this.create_new(crate::explorer::ops::NewKind::Folder, window, cx);
            }))
            .on_action(cx.listener(|this, _: &RenameSelected, window, cx| {
                this.start_rename(window, cx);
            }))
            .on_action(cx.listener(|this, _: &TrashSelected, window, cx| {
                this.trash_selected(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DeleteSelected, window, cx| {
                this.delete_selected(window, cx);
            }))
            .on_action(cx.listener(|this, _: &UndoLast, window, cx| {
                this.undo_last(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CopyPathSelected, _window, cx| {
                this.copy_selected_paths(cx);
            }))
            .on_action(cx.listener(|this, _: &CopyEntries, _window, cx| {
                this.copy_selection(ClipMode::Copy, cx);
            }))
            .on_action(cx.listener(|this, _: &CutEntries, _window, cx| {
                this.copy_selection(ClipMode::Cut, cx);
            }))
            .on_action(cx.listener(|this, _: &PasteEntries, window, cx| {
                this.paste(PasteInto::CurrentFolder, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenContextMenu, window, cx| {
                let position = window.mouse_position();
                let on = this.tab().state.activation_candidate();
                this.open_context_menu(position, on.as_deref(), window, cx);
            }))
            .on_action(cx.listener(|this, _: &MenuUp, _window, cx| {
                this.move_menu(false, cx);
            }))
            .on_action(cx.listener(|this, _: &MenuDown, _window, cx| {
                this.move_menu(true, cx);
            }))
            .on_action(cx.listener(|this, _: &MenuConfirm, window, cx| {
                this.confirm_menu(window, cx);
            }))
            .on_action(cx.listener(|this, _: &MenuCancel, window, cx| {
                this.close_menu(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DialogCancel, window, cx| {
                this.cancel_dialog(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DialogUp, _window, cx| {
                this.move_open_with(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &DialogDown, _window, cx| {
                this.move_open_with(1, cx);
            }))
            .on_action(cx.listener(|this, _: &NewTab, window, cx| {
                this.new_tab(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| {
                this.close_active_tab(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| {
                this.cycle_tab(1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PreviousTab, window, cx| {
                this.cycle_tab(-1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &JumpToTab1, window, cx| {
                this.jump_to_tab(1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &JumpToTab2, window, cx| {
                this.jump_to_tab(2, window, cx);
            }))
            .on_action(cx.listener(|this, _: &JumpToTab3, window, cx| {
                this.jump_to_tab(3, window, cx);
            }))
            .on_action(cx.listener(|this, _: &JumpToTab4, window, cx| {
                this.jump_to_tab(4, window, cx);
            }))
            .on_action(cx.listener(|this, _: &JumpToTab5, window, cx| {
                this.jump_to_tab(5, window, cx);
            }))
            .on_action(cx.listener(|this, _: &JumpToTab6, window, cx| {
                this.jump_to_tab(6, window, cx);
            }))
            .on_action(cx.listener(|this, _: &JumpToTab7, window, cx| {
                this.jump_to_tab(7, window, cx);
            }))
            .on_action(cx.listener(|this, _: &JumpToTab8, window, cx| {
                this.jump_to_tab(8, window, cx);
            }))
            .on_action(cx.listener(|this, _: &JumpToTab9, window, cx| {
                this.jump_to_tab(9, window, cx);
            }))
            .child(middle)
            .child(self.render_statusbar(cx))
            .child(self.render_toasts(window, cx))
            .children(overlays)
            .children(cards)
            .children(self.panel_drag.is_some().then(|| {
                // While a handle is dragged this shield takes the pointer:
                // the col-resize cursor stays and nothing underneath reacts.
                deferred(
                    div()
                        .id("explorer-resize-shield")
                        .absolute()
                        .size_full()
                        .cursor_col_resize()
                        .occlude()
                        .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                            this.drag_panel(event.position.x, event.pressed_button, window, cx);
                        }))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _event: &MouseUpEvent, _window, cx| {
                                this.end_panel_drag(cx);
                            }),
                        ),
                )
                .with_priority(3)
            }))
    }
}

impl Explorer {
    /// The empty top gutter matches the side gutters, without branding.
    /// The OS window title retains the current folder and application identity.
    fn render_header(&self) -> impl IntoElement {
        div()
            .id("explorer-header")
            .flex_none()
            .h(IslandMetrics::GAP)
    }

    /// In the grid, up and down move one row of tiles; in the list they
    /// move one entry.
    fn grid_step(&self) -> isize {
        match self.tab().state.view() {
            View::List => 1,
            View::Grid => self.rendered_columns.max(1) as isize,
        }
    }

    /// The document island: toolbar, scope line, folder subbar, then the
    /// entries beside the details panel, all cut 14 px at the opposing
    /// corners by [`Island`].
    fn render_document_island(&mut self, window: &Window, cx: &mut Context<Self>) -> Island {
        self.inspector_shown = self.inspector_wanted(window);
        self.sync_permissions(cx);
        let panel = self.inspector_shown.then(|| inspector::render(self, cx));
        // The details handle covers the panel's left edge.
        let handle = self.inspector_shown.then(|| {
            sidebar::resize_handle(
                "explorer-details-resize",
                Panel::Details,
                self.panel_drag
                    .is_some_and(|drag| drag.panel == Panel::Details),
                cx,
            )
            .right(px(self.details_width - sidebar::HANDLE_HIT))
        });
        Island::new()
            .flex_1()
            .min_w_0()
            .h_full()
            .children(tabstrip::render(self, cx))
            .child(toolbar::render(self, cx))
            .child(self.render_subbar(cx))
            .children(self.render_scope_line(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .flex()
                    .relative()
                    .child(entries::render(self, cx))
                    .children(panel)
                    .children(handle),
            )
    }

    /// The scope line under the toolbar: the Trash scope, the live filter,
    /// or nothing.
    fn render_scope_line(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
        let colors = bolt::colors(cx);
        let text = if self.tab().state.current() == &Location::Trash {
            Some(
                "Home Trash \u{2014} Restore puts items back; Delete permanently and Empty Trash cannot be undone."
                    .to_owned(),
            )
        } else {
            let query = self.tab().state.query();
            (!query.is_empty()).then(|| {
                format!(
                    "Filtering \u{201c}{query}\u{201d} — {} of {} items. Current folder only, no subfolders.",
                    self.tab().state.visible_shared().len(),
                    self.tab().state.listing().entries.len()
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

    /// The folder subbar under the toolbar: the current folder's title and
    /// its mono "4 folders · 7 files" meta.
    fn render_subbar(&self, cx: &Context<Self>) -> Stateful<Div> {
        let colors = bolt::colors(cx);
        let meta = if self.tab().state.is_loading() {
            String::new()
        } else if self.tab().state.shows_fewer_than_listed() {
            util::filtered_counts_label(
                self.tab().state.visible_shared().len(),
                self.tab().state.listing().entries.len(),
            )
        } else {
            self.tab().folder_label.clone()
        };
        div()
            .id("explorer-subbar")
            .relative()
            .flex_none()
            .h(px(SUBBAR_HEIGHT))
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(SUBBAR_PADDING))
            .child(
                div()
                    .id("explorer-subbar-title")
                    .min_w_0()
                    .truncate()
                    .text_size(px(16.0))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(colors.strong)
                    .child(self.current_label()),
            )
            .child(
                div()
                    .id("explorer-subbar-meta")
                    .flex_none()
                    .font_family(bolt::fonts::editor_family(cx))
                    .text_size(px(12.0))
                    .text_color(colors.muted)
                    .child(meta),
            )
            .child(self.render_filter_chips(cx))
            .child(
                Button::new("explorer-sort", "Sort")
                    .secondary()
                    .icon(crate::explorer::icon(
                        gpui_kit_assets::IconName::ArrowUpDown,
                    ))
                    .tooltip("Sort by name, size, date or type")
                    .on_click(cx.listener(|this, event: &gpui::ClickEvent, window, cx| {
                        let position = overlay::under_button(event.position());
                        this.open_arrange_menu(MenuPage::Sort, position, window, cx);
                    })),
            )
            .child(
                Button::new("explorer-filter", "Filter")
                    .secondary()
                    .icon(crate::explorer::icon(gpui_kit_assets::IconName::ListFilter))
                    .tooltip("Filter by type, date, size or hidden files")
                    .on_click(cx.listener(|this, event: &gpui::ClickEvent, window, cx| {
                        let position = overlay::under_button(event.position());
                        this.open_arrange_menu(MenuPage::Filter, position, window, cx);
                    })),
            )
            .child(if self.tab().state.current() == &Location::Trash {
                Button::new("explorer-empty-trash", "Empty Trash")
                    .danger()
                    .icon(crate::explorer::icon(gpui_kit_assets::IconName::Trash))
                    .disabled(self.tab().state.listing().entries.is_empty())
                    .tooltip("Delete everything in the Trash for good")
                    .on_click(cx.listener(|this, _event, window, cx| {
                        this.request_empty_trash(window, cx);
                    }))
                    .into_any_element()
            } else {
                Button::new("explorer-new-folder", "New folder")
                    .primary()
                    .icon(crate::explorer::icon(gpui_kit_assets::IconName::FolderPlus))
                    .tooltip("New folder (Ctrl+Shift+N)")
                    .on_click(cx.listener(|this, _event, window, cx| {
                        this.create_new(crate::explorer::ops::NewKind::Folder, window, cx);
                    }))
                    .into_any_element()
            })
    }

    /// The toast stack: bottom-right, above the status bar, growing upward
    /// (see [`toasts`]). Bolt's host draws top-down from its parent's top
    /// edge, so the zero-height wrapper is placed from the stack's measured
    /// height; the measure settles within a frame of a toast appearing or
    /// closing.
    fn render_toasts(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let top = toasts::stack_top(
            f32::from(window.viewport_size().height),
            self.toast_stack_height.get(),
        );
        let measured = Rc::clone(&self.toast_stack_height);
        let explorer = cx.entity_id();
        deferred(
            div()
                .absolute()
                .top(px(top))
                .right(px(toasts::MARGIN - toasts::HOST_INSET))
                .flex_none()
                .h_0()
                .w(px(toasts::CARD_WIDTH + 2.0 * toasts::HOST_INSET))
                .on_children_prepainted(move |bounds, _window, cx| {
                    let height = bounds
                        .first()
                        .map_or(0.0, |bounds| f32::from(bounds.size.height));
                    if (measured.get() - height).abs() > 0.5 {
                        measured.set(height);
                        cx.notify(explorer);
                    }
                })
                .child(self.toast_host.clone()),
        )
    }

    /// The active filters as removable chips, then "Clear all"; this is
    /// also the subbar's flexible gap, so it fills the row when empty.
    fn render_filter_chips(&self, cx: &Context<Self>) -> Stateful<Div> {
        let colors = bolt::colors(cx);
        let tint = bolt::tokens::ChromeColors::tint(cx.theme());
        let chips = self.tab().state.arrangement().filters.chips();
        let clear = (chips.len() > 1).then(|| {
            div()
                .id("explorer-chips-clear")
                .flex_none()
                .cursor_pointer()
                .text_size(px(12.0))
                .text_color(colors.muted)
                .hover(|this| this.text_color(colors.foreground))
                .child("Clear all")
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.arrange(ArrangeCommand::ClearFilters, cx);
                }))
        });
        div()
            .id("explorer-chips")
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(6.0))
            .overflow_hidden()
            .children(chips.into_iter().enumerate().map(|(index, chip)| {
                StatusChip::new(("explorer-chip", index))
                    .clickable(true)
                    .tone(tint)
                    .wash(true)
                    .build(cx)
                    .text_size(px(12.0))
                    .tooltip(move |window, cx| {
                        Tooltip::new(format!("Remove the {} filter", chip.label()))
                            .build(window, cx)
                    })
                    .child(chip.label())
                    .child(div().text_color(colors.muted).child("\u{d7}"))
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.arrange(ArrangeCommand::RemoveChip(chip), cx);
                    }))
            }))
            .children(clear)
    }

    /// The volume usage line of the volume holding the current folder
    /// (longest mount-path prefix); nothing when it is unknown.
    fn free_space_label(&self) -> Option<String> {
        let places = self.places.as_ref()?;
        let path = match self.tab().state.current() {
            Location::Directory(path) => path.as_path(),
            Location::Trash => places.trash_root.as_path(),
        };
        let usage = places.volume_containing(path)?.usage?;
        Some(usage.label())
    }

    /// The 38 px status bar: the live status (loading, errors, notice) or
    /// "N items" / "N selected · size" on the left, warnings and the free
    /// space of the current volume on the right. Shortcuts live in the
    /// toolbar tooltips.
    fn render_statusbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let colors = bolt::colors(cx);
        let entries_count = self.tab().state.listing().entries.len();
        let shown = self
            .tab()
            .state
            .shows_fewer_than_listed()
            .then(|| self.tab().state.visible_shared().len());
        let selection = self.tab().state.selection();
        let mut count_text = util::status_label(
            entries_count,
            shown,
            selection.len(),
            self.tab().state.selected_size(),
        );
        // A single selected Trash entry keeps its recorded origin.
        if self.in_trash()
            && let Some(path) = selection.iter().next().filter(|_| selection.len() == 1)
            && let Some(origin) = self
                .tab()
                .state
                .listing()
                .entries
                .iter()
                .find(|entry| entry.path == *path)
                .and_then(|entry| entry.origin.as_ref())
        {
            count_text.push_str(&format!(" \u{b7} from {}", origin.to_string_lossy()));
        }

        // Long names and paths are cut with an ellipsis instead of running
        // under the right-hand side of the bar.
        let mut status = div()
            .flex()
            .items_center()
            .gap(Spacing::SM)
            .flex_1()
            .min_w_0()
            .overflow_hidden();
        if self.tab().state.is_loading() {
            status = status.child(
                div()
                    .id("explorer-status-loading")
                    .text_color(colors.foreground)
                    .child("Loading…"),
            );
        } else if let Some(error) = self.renaming.as_ref().and_then(|r| r.error.as_deref()) {
            status = status.child(
                div()
                    .id("explorer-status-rename-error")
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .border_1()
                    .border_color(colors.danger)
                    .px(Spacing::SM)
                    .text_color(colors.danger)
                    .child(error.to_owned()),
            );
        } else if let Some(error) = self.tab().state.load_error() {
            status = status.child(
                div()
                    .id("explorer-status-error")
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .border_1()
                    .border_color(colors.danger)
                    .px(Spacing::SM)
                    .text_color(colors.danger)
                    .child(format!("Could not open folder: {error}")),
            );
        } else if let Some(error) = self.tab().state.path_error() {
            status = status.child(
                div()
                    .id("explorer-status-error")
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .border_1()
                    .border_color(colors.danger)
                    .px(Spacing::SM)
                    .text_color(colors.danger)
                    .child(path_argument_message(error)),
            );
        } else if let Some(notice) = self.tab().state.notice() {
            status = status.child(
                div()
                    .id("explorer-status-notice")
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(colors.muted)
                    .child(notice.to_owned()),
            );
        } else if let Some(notice) = &self.settings_notice {
            status = status.child(
                div()
                    .id("explorer-status-settings")
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(colors.warning)
                    .child(notice.clone()),
            );
        }

        let warnings = self.tab().state.warnings().len();
        let selection_active = !selection.is_empty();
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
            .child(
                div()
                    .id("explorer-status-count")
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(if selection_active {
                        colors.foreground
                    } else {
                        colors.muted
                    })
                    .child(count_text),
            )
            .child(status)
            .children(self.progress.status_text(Instant::now()).map(|text| {
                div()
                    .id("explorer-status-progress")
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_color(colors.foreground)
                    .child(div().size(px(6.0)).bg(colors.info))
                    .child(text)
            }))
            .children(self.tab().arrange_is_slow().then(|| {
                div()
                    .id("explorer-status-sorting")
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_color(colors.foreground)
                    .child(div().size(px(6.0)).bg(colors.info))
                    .child("Sorting\u{2026}")
            }))
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
                    .children(self.free_space_label().map(|label| {
                        div()
                            .id("explorer-status-free")
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(div().size(px(6.0)).bg(colors.success))
                            .child(label)
                    })),
            )
    }
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

    /// Verifier F1: a folder deleted and recreated at the same path leaves a
    /// dead hold; a tab that commits the same path again (the fallback of a
    /// vanished folder lands on it) must end with a live watch.
    #[test]
    fn retarget_rewatches_the_same_path_after_its_watch_died()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::time::Duration;
        let base = std::env::temp_dir().join(format!("fileflux-retarget-{}", std::process::id()));
        let folder = base.join("p");
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&folder)?;
        let (hub, events) = WatchHub::new()?;
        let mut slot = None;
        TabWatch::retarget(&mut slot, Some(folder.clone()), &hub);
        assert!(slot.is_some(), "watching the folder");

        std::fs::remove_dir_all(&folder)?;
        std::fs::create_dir(&folder)?;
        assert_eq!(
            events.recv_timeout(Duration::from_secs(3)).ok(),
            Some((folder.clone(), FolderEvent::Gone))
        );
        TabWatch::retarget(&mut slot, Some(folder.clone()), &hub);
        assert_eq!(hub.watch_count(), 1, "the path is watched again");
        std::fs::write(folder.join("x"), b"x")?;
        assert_eq!(
            events.recv_timeout(Duration::from_secs(3)).ok(),
            Some((folder.clone(), FolderEvent::Changed))
        );
        std::fs::remove_dir_all(&base).ok();
        Ok(())
    }
}
