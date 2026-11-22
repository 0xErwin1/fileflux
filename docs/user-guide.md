# User guide

How to browse, organize and manage files with Fileflux. Every key is in [Keyboard shortcuts](keyboard-shortcuts.md).

## Quick path

1. Run `fileflux` (your home folder) or `fileflux ~/Pictures` (another folder).
2. Click a place in the sidebar, or press Ctrl+L and type a path.
3. Right-click anything: the context menu is where every action lives.

## Contents

- [Command line](#command-line)
- [Single instance and Show in folder](#single-instance-and-show-in-folder)
- [Window layout](#window-layout)
- [Navigation](#navigation)
- [Views](#views)
- [Sort and filter](#sort-and-filter)
- [Tabs](#tabs)
- [Dual pane](#dual-pane)
- [Selection](#selection)
- [Favorites and GTK bookmarks](#favorites-and-gtk-bookmarks)
- [Network](#network)
- [Open, Open with and terminal](#open-open-with-and-terminal)
- [File operations](#file-operations)
- [Trash](#trash)
- [Archives](#archives)
- [Links](#links)
- [Undo](#undo)
- [Clipboard](#clipboard)
- [Drag and drop](#drag-and-drop)
- [Progress and toasts](#progress-and-toasts)
- [Details panel](#details-panel)
- [Thumbnails](#thumbnails)
- [Image viewer](#image-viewer)
- [Search in subfolders](#search-in-subfolders)
- [Disk usage](#disk-usage)
- [Recent files](#recent-files)
- [Sidebar](#sidebar)
- [Current scope](#current-scope)

## Command line

```text
fileflux [OPTIONS] [PATH]
fileflux [OPTIONS] IMAGE...
```

| Input | Result |
|---|---|
| no `PATH` | Reopens the saved tabs, or your home directory (`/` when `$HOME` is unset) |
| a directory | Opens it. Relative paths resolve against the current directory; a leading `~` expands against `$HOME` even when quoted |
| a file | Opens its parent directory |
| an image file | Opens the [image viewer](#image-viewer) on it, browsing the other images of its folder |
| several image paths | Opens the viewer on exactly those images; directories and other files among them are ignored with a note on stderr |
| a missing path | Prints `fileflux: '<absolute path>' does not exist` and exits with status 2; no window opens |
| `-h`, `--help` / `-V`, `--version` | Prints the usage or `fileflux <version>` and exits with status 0 |
| an unknown option, or a second path when the first is not an image | Prints the error plus `Try 'fileflux --help' for more information.` and exits with status 2 |

Image formats: png, jpg/jpeg, gif, webp, bmp, tif/tiff, ico, svg, qoi, tga, hdr, exr.

Use `--` before a directory whose name starts with `-`. A path on the command line opens only that folder and leaves your saved tabs alone (see [Settings](settings.md#what-is-saved-and-when)).

## Single instance and Show in folder

Fileflux runs once per desktop session. Starting it again with a path hands the path to the window that is already open and exits with status 0: a folder opens in a new tab, and a file opens its folder in a new tab with the file selected. Starting it without a path just raises the running window. Image paths still open the [image viewer](#image-viewer) in their own window. Without a session bus (or when D-Bus fails) Fileflux simply opens as before.

The running window also answers the `org.freedesktop.FileManager1` interface, which is what "Show in folder" or "Open containing folder" in other applications (browsers, chat clients) calls: `ShowFolders` opens each folder in a new tab, `ShowItems` opens the parent folder of the items in a new tab and selects them, and `ShowItemProperties` does the same and shows the details panel. Only `file://` URIs are accepted; other schemes are ignored. Fileflux never takes this name from another file manager that already owns it (GNOME Files, Dolphin): the request is refused and the other manager keeps answering. No D-Bus service file is installed, so Fileflux does not start by itself on such a request; it answers only while it is running.

## Window layout

A sidebar of places on the left, the folder in the middle, and an optional [details panel](#details-panel) on the right.

| Part | What it does |
|---|---|
| Toolbar | Back, forward, up, the path bar, the search field with its options behind the filter button, the grid/list/compact switch, and one `⋯` menu with Split view, Favorite this folder, Refresh (Ctrl+R) and Properties (Ctrl+I). In a split view each pane's toolbar ends with a **×** (Close this pane). A pane too narrow for everything on one row (about 580 px, so every pane of a split on a window under roughly 1500 px) puts the search field and the grid/list/compact switch on a second row under the path; nothing is ever clipped. While the sidebar is hidden, the leftmost pane's toolbar starts with the hamburger that shows it again |
| Folder subbar | Current folder name, "4 folders · 7 files" count, **Sort**, **Filter**, **New folder**, active filter chips |
| Status bar | "N items" or "N selected · size" (with the file type when exactly one entry is selected), loading and error notices, a progress indicator for long operations, free space of the current volume |

Drag the right edge of the sidebar or the left edge of the details panel (a 6 px handle with a tint line on hover) to resize them; double-click a handle to restore the default width (290 px sidebar, 300 px details). The sidebar is clamped to 200–600 px and always leaves the folder pane 360 px; the details panel to 240–600 px and never wider than the window leaves room for the entries. Narrow windows park the sidebar as an overlay, without a resize handle, keeping its width; the hamburger at the top of the sidebar hides it at any width (while it is hidden, the same hamburger starts the leftmost pane's toolbar and shows it; Ctrl+B does the same from the keyboard), and the path, search and view controls always stay reachable. The widths are remembered (`sidebar_width`, `details_width` in [settings](settings.md#files-and-locations)).

The window title is the current folder plus Fileflux ("Home — Fileflux") and follows the active tab. Fileflux always starts in Bolt's dark palette; there is no theme toggle.

## Navigation

| Control | Behavior |
|---|---|
| Sidebar | Home, Desktop, Documents, Downloads, Music, Pictures and Videos (from your freedesktop user-dirs config), your favorites, the Home Trash and the Devices section. Names only; full paths in tooltips. Volumes come from the mount table, one entry per device without virtual or duplicate bind mounts (the root filesystem reads "System"), each with a usage meter and "X free of Y" from `statvfs`. See [Devices](devices.md) |
| Breadcrumbs | A Home icon and "Home" under `$HOME`, else `/`, with chevron separators. Each segment navigates and the current folder reads strong. Long paths collapse their leading segments behind a "…" (it goes to the closest hidden folder) instead of overflowing; the Home icon and the chevrons are never cut, only the current folder's name can end in an ellipsis |
| Path field | Click the bar's empty space or press Ctrl+L. Enter navigates to an absolute, relative or `~/…` path (no shell evaluation). Escape or leaving the field restores the committed path. An invalid or missing path reports an error and changes nothing |
| History | Alt+←, Alt+→ and Alt+↑ (back, forward, up), or the mouse's back and forward side buttons over a pane (in a split, the pane under the pointer). A failed load changes nothing |
| Favorites | Your GTK bookmarks (`~/.config/gtk-3.0/bookmarks`, shared with Nautilus and Thunar). The `⋯` menu, a folder's context menu and a sidebar row's menu add or remove one; an edit made by another program shows live. The Trash cannot be a favorite. See [Favorites and GTK bookmarks](#favorites-and-gtk-bookmarks) |

Non-UTF-8 file names keep their exact bytes for navigation; lossy text appears only in what is displayed.

### Live refresh

Every tab's folder, including background tabs, is watched. Changes made elsewhere (creates, deletes, moves, edits) appear within about a second, keeping the selection of entries that still exist, the filter and the scroll position. If a tab's folder is deleted, that tab falls back to its nearest existing parent with a notice. The Trash is not watched; it is re-read when its tab is shown.

## Views

A segmented toggle in the toolbar switches between three presentations. The toggle keeps the folder, search and selection.

| View | What you see |
|---|---|
| Grid | Fixed-size tiles with two-tone folders, file thumbnails and a size line (folders show "N items"). Rows are equal; the last partial row is left-aligned |
| List | Columns Name, Size and Modified, plus the optional Type, Permissions, Owner and Group (see below). Click a header to sort (folders stay first). Columns fill the pane; drag to resize. Folder item counts are filled in the background after the listing appears |
| Compact | A small icon and a single-line name (long names end in an ellipsis) in fixed-width cells that flow left to right and wrap into rows, with vertical scrolling. No size, date or thumbnail; images show their file icon. Selection, rubber band, drag and drop, context menus, type-ahead and the details panel work as in the grid |

**List columns.** Name is always shown; Size and Modified are on by default. Right-click a column header, or open the empty-space menu's **Columns** submenu, to tick or untick Size, Modified, **Type** (the same label as the details panel: "Image · JPG", "Folder"), **Permissions** (`rw-r--r--`, as the details panel shows it), **Owner** (user name, the number when the id has no name) and **Group** (likewise). The menu stays open while you tick several. The choice is saved and applies to every tab and the next launch; a dragged width is remembered per column for the session. Clicking the Type, Permissions, Owner or Group header sorts by it (Permissions by the octal mode, Owner and Group by name without case; ties fall back to the name, folders stay first); the same keys are in the **Sort by** menu. Owner, Group and Permissions are read during the scan; entries that do not come from a folder scan (search results, Trash, Recent) show "—" in those three columns.

### Icon size

Each view has its own icon size, in the GNOME Files fashion. The grid has four levels, **small**, **medium**, **large** and **larger**: the tile, its icon and its thumbnail grow together, and the number of columns follows the width of the pane at every level. The list has three, **small**, **medium** and **large**, which change the row height and the icon in the Name column. The compact view has three, **small**, **medium** and **large**, which change the cell width and height and the icon (16, 20 and 24 px); the number of cells per row follows the width of the pane. Medium is the size Fileflux drew before the zoom existed (for the compact view, its default).

| How | What it does |
|---|---|
| Ctrl+= or Ctrl++ (Ctrl+Shift+= where `+` needs Shift) | One level larger |
| Ctrl+- | One level smaller |
| Ctrl+0 | Back to medium |
| Ctrl+wheel over the entries | Wheel up zooms in, wheel down zooms out; one level per notch (a touchpad's small movements add up to a notch) |
| Empty-space menu, **Zoom** | **Zoom in**, **Zoom out**, **Reset zoom**; in the list it comes after **Columns**, in the grid and the compact view after **Filter** |

The keys and the wheel act on the view that is shown, and stop at the smallest and the largest level. The three views' levels are saved separately (`grid_zoom`, `list_zoom` and `compact_zoom` in `settings.toml`) and apply to every tab and the next launch. In a split view both panes share the levels.

Sizes use one SI formatter everywhere. Modified dates are shown in the system's local time zone (`TZ`, else `/etc/localtime`).

File icons come from the freedesktop icon theme the desktop is configured to use, so a Blender file, a shell script, a PDF or a spreadsheet shows the icon of the application that handles it. The lookup follows `gtk-icon-theme-name` in `~/.config/gtk-3.0/settings.ini`, that theme's `Inherits=` chain, and then `Adwaita`, `breeze`, `Papirus` and `hicolor`, in `$XDG_DATA_HOME/icons` and every `$XDG_DATA_DIRS/*/icons`; each type is resolved once per session. Themed icons keep their colours because Fileflux rasterizes them at the size it draws, and a type the theme does not name keeps the built-in glyph.

## Sort and filter

Sort and filter live in the tab and apply to the grid, the list and the compact view alike. Selection, arrow keys, ranges, Ctrl+A, Enter, the image viewer's list, paste targets and the details panel all follow the filtered and sorted result. A new tab inherits them, unless the folder it opens has its own [remembered view](#per-folder-view-memory).

Use the subbar's **Sort** and **Filter** buttons, the same two pages in the empty-space context menu, or the list header.

| Control | Behavior |
|---|---|
| Sort | **Name, Size, Modified, Type, Permissions, Owner, Group**, ascending or descending; the default is **Modified, descending** (newest first). *Type* orders by category, then extension, then name. **Folders first** (default on) groups folders before files. **Natural number order** (default on) puts `IMG_2` before `IMG_10`. A missing size or time sorts lowest |
| Type | Multi-select chips: Folders, Images, Videos, Audio, Documents, Archives, Code, Other, decided by extension. None selected means all |
| Hidden files | Ctrl+H, or *Show hidden files* in the Filter menu. **Hidden by default**: names starting with a dot and the names listed one per line in the folder's `.hidden` file (Nautilus and Thunar's convention). No chip in the default state; showing them adds a **Hidden shown ×** chip. While some are hidden the counts read "5 of 8 items" |
| Modified | Today, Last 7 days, Last 30 days, This year, Older (before the last 30 days). One choice or none. Files with an unknown time never match |
| Size | < 1 MB, 1–100 MB, 100 MB–1 GB, > 1 GB. One choice or none. Files with an unknown size never match |
| Folders and ranges | A size range never applies to folders. A date range applies to folders only when **Folders is the only type chip selected**; otherwise folders pass it |
| Search | Ctrl+F filters names in the current folder: case-insensitive, nonrecursive, with a live "n of m" count. No matches is distinct from an empty folder. For subfolders use [Search in subfolders](#search-in-subfolders) (Ctrl+Shift+F) |
| Combining | Every filter and the search combine with AND |

Each active filter is a removable chip next to the folder title, with **Clear all** when there are several. While anything filters, the subbar, status bar and details panel read "N of M items". **New folder** and **New file** clear the search and filters so the new item is visible.

**Large folders** (more than a few thousand entries) are arranged on a background thread, so typing and clicking never wait. The previous order stays on screen meanwhile, and a **Sorting…** indicator appears in the status bar only when a result takes longer than about 150 ms. A folder of a million entries stays responsive.

### Per-folder view memory

Fileflux remembers how you like each folder: change the view (grid, list, compact), the sort, **Folders first**, hidden files or a type, date or size filter in a folder and it comes back that way whenever you open the folder again, in any tab and on the next launch. A folder you never changed keeps what the tab shows, so your last choice still carries over to new folders. The Trash, Recent and search results are not remembered. To forget a folder's view, right-click the empty space and choose **Reset View for This Folder** (shown only for a folder that has one); it returns to the view the settings held at launch. Up to 1000 folders are kept, the least recently changed going first. See [Settings and files](settings.md#per-folder-view-memory).

## Tabs

The tab strip appears above the toolbar once two or more tabs are open. A tab shows the folder name (`Home`, `Trash`) with the full path as its tooltip.

| Action | How |
|---|---|
| New tab | Ctrl+T. It starts with the view, sort and filters of the tab it came from, and a fresh history |
| Close tab | Ctrl+W, the close button, or a middle click. The last tab is never closed. Closing the active tab activates its right neighbour (the left one when it was the rightmost) |
| Switch | Ctrl+Tab / Ctrl+Shift+Tab, Ctrl+PageDown / Ctrl+PageUp, Alt+1 … Alt+9, or click |
| Reorder | Drag a tab along the strip; a tint bar shows the gap it will land in. Or Ctrl+Shift+PageUp / Ctrl+Shift+PageDown to move the active tab one place left or right (it stops at the ends). The moved tab keeps its history, selection, view and everything else, and the order is saved with the session |
| Move to the other pane | In a [split view](#dual-pane), drag a tab onto the other pane's tab strip (a pane with one tab shows its strip over its toolbar while you drag, so nothing moves). The tab lands in the gap you drop it in and becomes that pane's active tab; the pane it left activates its right neighbour (else the left one) if it was the active one. A pane's last tab cannot be dragged out, like it cannot be closed. The tab keeps its watch on the folder and any scan in progress |
| Open a dragged folder in a tab | Drag **one folder** (from the entries or a sidebar row) up onto the tab strip: drop it on the empty end of the strip or on the narrow edge zone between two tabs (a tint bar shows the gap) and it opens in a new tab of that pane at that position, which becomes active. A pane with a single tab shows its strip over the toolbar while you drag (the entries do not move). The tabs keep their widths while you drag. Dropping on the middle of an existing tab does nothing, as before; several items or files never open a tab; no copy or move happens |
| Open in a new tab | Middle-click a folder (grid, list), a sidebar row or a breadcrumb segment, or **Open in new tab** in the context menu. It opens in the background, to the right of the active tab. Files do nothing |

Each tab keeps its own location, history, selection, view, sort, filter text, scroll position and path-edit state. Shared across the window: the sidebar and favorites, the clipboard (copy in one tab, paste in another), the undo history, progress cards, dialogs and toasts.

- Switching tabs cancels an inline rename and closes an open context menu.
- A file operation keeps running when you switch tabs. When it finishes, every tab showing a folder it changed is re-read.
- Tabs are restored at the next launch, in their saved order and pane (see [Settings](settings.md)).
- Fileflux's file explorer has a single window, so a tab cannot be dragged to another window yet (see the [roadmap](roadmap.md)).

## Dual pane

F3 or the **Split view** button in the toolbar shows two folders side by side; pressing it again goes back to one pane. The new pane opens on the folder you were in, with the same view, sort and filters.

| Action | How |
|---|---|
| Close a pane | The **×** at the end of a pane's toolbar (tooltip "Close this pane") closes that pane and leaves the other one as the single pane; the closed pane's tabs are parked like F3 parks them. It shows only while two panes are drawn |
| Split / unsplit | F3 or the toolbar button. Unsplitting keeps the pane that has the focus; the other pane's tabs are parked, still open and live, and come back with the next split |
| Switch pane | F6, or click anywhere in a pane |
| Copy to the other pane | F5: the selection is copied into the folder the other pane shows |
| Move to the other pane | Shift+F5: the selection is moved there |
| Drag and drop | Drag between panes like anywhere else: a move on the same device, a copy across devices, Ctrl copies, Shift moves, Ctrl+Shift links |

- **The divider between the panes is draggable.** Drag the 8 px column between them (the pointer becomes a column-resize cursor and a tint line shows) to give one pane more room; each pane keeps at least 460 px (enough for its title beside Sort, Filter and New folder), and a window too narrow for that splits evenly. Double-click the divider to go back to 50/50. The ratio is remembered (`split_permille` in [settings](settings.md#files-and-locations)).
- **Drag a folder to open it in the split.** While you drag exactly one folder (from the entries or a sidebar row), a narrow rail appears at the very right edge of the files area (in the padding beside the tiles, so every folder stays a drop target); it widens and shows the label **Open in split** while the drag is over it. Dropping the folder there opens it in the other pane: a view that is not split splits with it (a window narrower than 1100 px refuses with the same toast as F3), and a split view opens it in a new tab of the pane the drag did not start in. Nothing is copied or moved. Dropping anywhere else keeps the move/copy behaviour above; several items and files from other applications never use the strip. Dropping the folder on the tab strip opens a tab instead (see [Tabs](#tabs)).
- **Each pane is a tab strip of its own** (Ctrl+T, Ctrl+W, Ctrl+Tab and Alt+1 … Alt+9 act on the focused pane), and every tab keeps its location, history, view, sort, filters and selection. The focused pane has a 3 px tinted edge on top.
- **The focused pane is the one you are working in.** The sidebar, breadcrumbs, path field, search, toolbar and every keyboard shortcut act on it. Clicking in the other pane focuses it first (an open dialog or context menu keeps the press for itself).
- **F5 and Shift+F5 use the paste engine**, so you get the conflict dialog (Skip, Keep both, Replace), the progress card with Cancel, and Undo. They leave the clipboard alone. Without a split they say so in a toast. They are refused, with a toast, when the other pane shows the Trash, Recent or search results (there is no folder to put files in), and out of the Trash (use Restore).
- **Shared across both panes:** the sidebar and favorites, the clipboard, the undo history, progress cards, dialogs and toasts. When an operation finishes, every tab of both panes showing a folder it changed is re-read; live refresh and device ejects cover the tabs of both panes.
- **Panes always fit the window.** Each pane's toolbar gives way before anything is clipped: the breadcrumbs collapse their leading folders into a "…" and, in a pane under about 580 px, the search field and the view switch move to a second row. That holds at every pane width a 1100 px window with the default sidebar gives (about 390 px) and up; the folder title in the pane's header truncates. The panes share exactly the room between the sidebar and the window edge.
- **Window width.** Two panes need a window at least **1100 px** wide. In a narrower window F3 refuses with a toast; a window that shrinks while split shows only the focused pane (F6 switches which one) until it widens again. F5 and Shift+F5 still work then.
- **The details panel is not drawn while split** (there is no room beside two panes); Properties and Ctrl+I show it again once you unsplit.
- The split, both panes' tabs and which pane had the focus are restored at the next launch (see [Settings](settings.md)).

## Selection

| Gesture | Result |
|---|---|
| Click | Select one entry |
| Ctrl+click | Toggle one entry |
| Shift+click | Select a range, in the displayed order |
| ↑ ↓ ← →, Home, End | Move through the visible entries and scroll them into view |
| Shift + those keys | Extend the selection from the anchor |
| Ctrl+A | Select every entry visible under the filter |
| Ctrl+Shift+I | Invert the selection over the visible entries: what was selected is deselected and the reverse. Entries the filter or the hidden-file setting hides keep their state. The cursor moves to the first selected entry |
| Ctrl+S | Select items matching… (see below) |
| Type a name | Type-ahead find: the selection jumps to the first visible entry whose name starts with the letters you type (case-insensitive, following the sort, filter and hidden-file rules). Pause for about a second, press Escape, change folder or move the selection with the mouse or arrow keys to start over; repeating one letter cycles through the entries that start with it. Ctrl+F still filters |
| Escape | Clear the selection |
| Click on empty space | Clear the selection (Ctrl or Shift held: leave it) |
| Drag on empty space | Draw a selection rectangle; the entries it touches are selected live. Every point inside a tile is the entry, so the rectangle starts from the gaps between tiles, the margins and the space right of the last column and below the last row |
| Ctrl+drag on empty space | Toggle the entries the rectangle touches, against the selection from before the drag |
| Shift+drag on empty space | Add the entries the rectangle touches to the selection from before the drag |

**Rubber-band selection** works in the grid, the list and the compact view, in both panes. Press on empty space (the gaps between tiles, the blank part of a tile around its icon and name, the space right of the last tile or below the last row, the pane's margins; never on a tile's icon or name, a list row, a column header or the scrollbar), then drag. Without a modifier the rectangle replaces the selection; with Ctrl it toggles, and with Shift it adds, always relative to the selection you had when you pressed, so shrinking the rectangle gives entries back. Holding the pointer near, or beyond, the top or bottom edge scrolls the pane, faster the further out it is. Escape during the drag cancels it and restores the selection from before the press. Dragging from an entry still drags files, not a rectangle.

**Select items matching…** (Ctrl+S) asks for a name pattern. Enter selects every visible entry whose name matches, replacing the selection; Escape cancels. `*` stands for any run of characters (also none), `?` for exactly one character, `[abc]` or `[a-c]` for one of the listed characters and `[!abc]` for any other. Every other character, such as `.`, `+` or `(`, stands for itself. The whole name must match and case is ignored, so `*.jpg` finds `Photo.JPG`. With no match, a toast says `No items match <pattern>` and the selection stays. The dialog remembers the last pattern until you quit.

Right-click on empty space (in a folder, the Trash, Recent or search results) and open **Select** for Select all, Invert selection and Select items matching… without the keyboard.

A selected entry that the filter hides stays selected, but operations act only on the entries that are visible.

## Favorites and GTK bookmarks

The sidebar's Favorites are the local lines of `$XDG_CONFIG_HOME/gtk-3.0/bookmarks` (default `~/.config/gtk-3.0/bookmarks`), the file Nautilus, Thunar, Nemo and the GTK file chooser share.

- A line is `file:///percent-encoded/path Optional Label`. The sidebar shows the label when there is one, else the folder name.
- `sftp://`, `smb://`, `ftp://` and `dav://` lines are the [network bookmarks](#network). Other lines, comments and blank lines are kept untouched and not shown. Adding appends one line, removing drops only that folder's lines; order, labels and line endings stay.
- Changes are written atomically (a temporary file renamed over the original, through a symlink, keeping the file's permissions) after re-reading the file, so an edit made by another program meanwhile is not lost. An external edit shows in the sidebar live.
- Older versions kept favorites in `settings.toml`: the first run moves them into the bookmarks and saves the settings without them. If the bookmarks cannot be written they stay in `settings.toml` and are retried at the next launch.

## Network

The sidebar's **Network** section connects to servers the way Nautilus and Thunar do, through `gio mount` (the `gio` tool of glib, which needs gvfs). Without `gio` on `PATH` its entries are muted and say so.

- **Connect to Server…** (the Network section, or the empty-space menu of a folder) asks for an address: `sftp://user@host/path`, `smb://host/share`, `ftp://`, `ftps://`, `dav://` or `davs://`. The host is required; the user, port (`host:2222`, `[::1]:2222` for IPv6) and path are optional. The servers you connected to in this session are offered as quick picks (they are not saved).
- Do not type the password in the address: an address with `user:password@` is refused. If the server needs a user, a domain or a password, `gio` asks and Fileflux shows each question in a dialog. The password field is masked. The password goes straight to `gio` and is never written to disk, to the logs or to a notice, and it is not kept once the attempt ends. An unknown SSH host key (or another question with choices) is shown with one button per choice.
- **Cancel** in a question, or the **Connecting…** row of the sidebar, stops the attempt and kills `gio`. A failure shows `gio`'s message in a notice. When the mount is ready its folder (`$XDG_RUNTIME_DIR/gvfs/…`) opens. Only one connection is attempted at a time.
- The network lines of the GTK bookmarks file are listed under Network, with their label or else the host and path. A click opens the mount, connecting first when it is not mounted. The row's menu has **Connect** and **Remove bookmark** (it deletes only that bookmark's lines). Unmount a connected server from the **Devices** section like any other gvfs mount. Adding a server as a bookmark from Fileflux is not offered yet: add the line to the bookmarks file or use another file manager.

## Open, Open with and terminal

- **Open** (double click or Enter): folders navigate. Images open the [image viewer](#image-viewer). A symlink to a folder is entered like the folder. Files, links to files and broken links open with the default application (`xdg-open <path>`, detached, no shell); a failure to start it appears in the status bar. Trash entries are not opened: restore them first. The same holds for a new tab (Open in new tab, middle click): a folder in the Trash opens nothing and shows a notice.
- **Open with…** (context menu, one or several selected files; not folders, not in the Trash; see [Several files](#several-files-at-once)): lists the default application first, then every other application whose `.desktop` file declares the file's MIME type. Up/Down choose, Enter or a click opens. The MIME type comes from `xdg-mime query filetype` (the extension is the fallback). Applications are found in `$XDG_DATA_HOME/applications` (else `~/.local/share/applications`) and each `$XDG_DATA_DIRS/applications` (else `/usr/local/share:/usr/share`); a same-named file in an earlier directory hides the later one. Every `text/*` type also matches applications that list `text/plain`. `NoDisplay=true`, `Hidden=true`, `Terminal=true` entries, ones whose `Exec=` takes no file (`%f %F %u %U`), and ones that put a field code inside a quoted argument (forbidden by the Desktop Entry spec, and with `sh -c "… %f"` it would let a file name run as a shell command) are not listed. `Exec=` is split by the Desktop Entry quoting rules: `%f`/`%F` become the path, `%u`/`%U` its percent-encoded `file://` URI, `%%` a percent sign, and the other codes are dropped. The program is started without a shell, detached, in the file's folder. If nothing handles the type, a toast says so. The last row, **Other application…**, lists every visible application that takes a file (not only those declaring the type), by name, with a filter box (case-insensitive match on the name); the same `Exec=` rules apply, so entries that run in a terminal, are hidden, or put a field code inside quotes are not listed. **Remember for <type>** (off by default) makes the chosen application the default for that type: it writes `<type>=<desktop-id>;` to the `[Default Applications]` group of `$XDG_CONFIG_HOME/mimeapps.list` (else `~/.config/mimeapps.list`), creating the file or group when missing and keeping every other line, comment and line ending, atomically and through a symlinked file. That application is then listed first for the type. Unchecked, the choice opens the file once and writes nothing. If the file cannot be written, a warning toast says so and the application still opens the file.
- **Open in terminal** (context menu on one selected folder, on a sidebar row and on empty space): starts your terminal in that folder (the shown folder on empty space), with no shell. The terminal is the first that exists of: `$TERMINAL`, `xdg-terminal-exec`, the desktop ids in `xdg-terminals.list`, a desktop entry with `Categories=TerminalEmulator` (known terminals first), then ghostty, kitty, alacritty, foot, wezterm, gnome-terminal, kgx, konsole, xfce4-terminal, xterm. Terminals that ignore the working directory of the process get their own argument: `--working-directory=DIR` (ghostty, foot, gnome-terminal, kgx, xfce4-terminal), `--working-directory DIR` (alacritty), `--directory DIR` (kitty), `--workdir DIR` (konsole), `start --cwd DIR` (wezterm); xterm uses its working directory, and a terminal found through a desktop entry uses the entry's `X-TerminalArgDir` when it has one. `<desktop>-xdg-terminals.list` variants for `$XDG_CURRENT_DESKTOP` are tried first, and the lists are read from `$XDG_CONFIG_HOME` then `$XDG_CONFIG_DIRS`. The lookup reads `.desktop` files, so it runs off the UI thread at startup; the item is omitted until a terminal is found, and when none is.
- **Open in viewer** (context menu, one selected image), and **Open in new tab**, **Add to favorites** / **Remove from favorites** (selected folder, not in the Trash).

### Several files at once

Enter (or **Open**) and **Open with…** work on a selection of several files, following the Desktop Entry rules for the application's `Exec=` line:

- An `Exec=` with `%F` or `%U` gets **every file in one process**, each as its own argument (`%U` as percent-encoded `file://` URIs). One with only `%f` or `%u` is started **once per file**, each in its own file's folder. File names are passed as separate arguments, never through a shell, and keep their exact bytes (spaces, `%`, non-UTF-8 names).
- **Open with…** on several files lists the applications that handle **every** selected type (the intersection of their `MimeType=` lists; `text/*` types still match applications that list `text/plain`), then **Other application…** as usual. **Remember for <type>** appears only when all files share one type; with mixed types nothing is written to `mimeapps.list`. The menu row is offered when no selected entry is a folder.
- **Enter** on several files groups them by each type's default application (the one in `mimeapps.list`, else `xdg-mime query default`) and starts each group as above. Files with no usable default go to `xdg-open`, one process per file. The first selected image opens the built-in viewer, as for one image; the other images of the selection are not started separately. Folders mixed with files are left alone (nothing navigates away); a selection of only folders keeps the rule for one entry.
- A click that would start **more than 20 processes** first asks, for example "Open 37 windows of Viewer?" (without the application name when several applications are involved); Cancel starts nothing. The default button is Cancel.
- If some applications cannot start (a program that is missing, or an `Exec=` line that cannot be run), one notice lists them: "Could not start A, B". The others still start.

The context menu on an entry offers, as they apply: Open, Open in new tab, Add to / Remove from favorites, Open in terminal, Open in viewer, Open with…, Cut, Copy, Paste (**Paste into folder** when exactly one folder is selected; dimmed while the clipboard is empty), **Paste as link** (on one selected folder), [Extract here, Extract to… and Compress…](#archives), Rename, [Make Link](#links), Copy path, Properties, Move to Trash and Delete permanently…. On empty space: New folder, New file, [**New from template ›**](#new-from-template), Paste, [Paste as link](#links), **Sort by ›** and **Filter ›** (cascading submenus holding the same options as the subbar menus: hovering the row, or pressing → or Enter on it, opens the submenu beside the menu, which stays open; near the right edge of the window it opens on the left), Open in terminal and Copy folder path. Choices in these submenus (a sort key, a filter) apply at once and keep the menu open, with their check marks updating; the subbar **Sort** and **Filter** buttons open the same options directly.

## Custom actions

You can add your own entries to the context menu. They are read from `actions.toml` (see [Settings and files](settings.md#custom-actions-actionstoml)); Fileflux ships none.

```toml
# Open a terminal in the folder you right-clicked (empty space).
[[action]]
name = "Open a terminal here"
command = ["alacritty", "--working-directory", "%d"]
appears_on = "background"

# Compress the selection (one or more files or folders) with a tool of your choice.
[[action]]
name = "Compress with 7z"
command = ["7z", "a", "archive.7z", "%F"]
appears_on = "both"
selection = "any"

# Copy the path of one image to the clipboard.
[[action]]
name = "Copy path (wl-copy)"
command = ["wl-copy", "%f"]
appears_on = "files"
selection = "one"
mime = "image/*"
```

- **Fields.** `name` (the menu label) and `command` are required. `command` is a list: the program, then its arguments. It never goes through a shell; if you want one, write it out: `["sh", "-c", "echo \"$1\"", "sh", "%f"]`. `appears_on` is `files`, `folders`, `both` (the default) or `background` (the empty space of a folder). `selection` is `one`, `many` or `any` (the default) and counts the selected entries; it does not apply to `background`. `mime` (for example `image/*` or `image/png`; `inode/directory` for folders) and `glob` (for example `*.md`, case-insensitive, the same patterns as Select items matching…) narrow it further: every selected entry must match. The type is the one Fileflux shows for the file (from the name); an entry with no known type never matches `mime`.
- **Placeholders.** `%f` is the first selected path, `%F` all of them (each as its own argument, so it must be an argument on its own: `"%F"`, not `"--files=%F"`), `%d` the folder that holds the first path, `%n` its name and `%%` a literal `%`. Inside a larger argument, such as `"--file=%f"`, only `%f`, `%d` and `%n` work. For a `background` action, `%f` and `%d` are the shown folder. Names that are not valid UTF-8 are passed unchanged. The program itself cannot contain placeholders.
- **Menu.** The actions that match the selection appear after **Open with…**; with up to three they are plain rows, with more they are grouped in a **Custom actions ›** submenu. On the empty space they close the menu in a group of their own. Actions are not offered in the Trash, and `background` actions are not offered in Recent or search results. Entries listed there use their real paths.
- **Running.** The program starts detached, with no input, its output discarded, in the shown folder (in the folder of the first result in Recent and search results). If it cannot start, a notice says `Could not run <name>: <reason>`.
- **Reloading.** The file is checked every time a context menu opens, and read again only when it changed. If it has an error (bad TOML, an empty `command`, an unknown placeholder or value, a program that is not an executable on `PATH` or at the absolute path you gave), no custom actions are offered, one notice names the problem and its line, and the same message is written to the log as a warning. A missing or empty file simply offers nothing.

## File operations

Everything that changes the disk runs off the UI thread, reports its result in a toast or an inline error, and never overwrites anything silently. Right-click an entry or the empty space (or use the toolbar "more" button, the Menu key or Shift+F10). The menu is headed by its target ("report.pdf", "3 items", "Home (this folder)") and the target keeps a tint outline while it is open.

| Action | How |
|---|---|
| New folder / New file | The subbar's **New folder** chip, Ctrl+Shift+N or the menu. Creates "Untitled folder" or "Untitled file" (numbered when taken) and starts the inline rename |
| New from template | **New from template ›** on empty space, after New file (see below) |
| Rename | F2. The stem is preselected (folders select the whole name). Enter commits, Escape cancels, leaving the field commits. The name is kept exactly as typed. A refused name (empty, blank, contains `/`, already exists) stays in the field with the reason shown; leaving the field with a refused name cancels the rename and changes nothing. A name containing a control character cannot be edited inline: F2 says so and leaves the file untouched |
| Copy path | Ctrl+Shift+C. **Copy folder path** on empty space |
| Move to Trash | Del. See [Trash](#trash) |
| Delete permanently | Shift+Del. Opens a confirmation naming the item or the count; **Cancel** is the default and Enter alone never deletes. Folders are removed recursively; symlinks are never followed |
| Properties | Toggles the [details panel](#details-panel) (Ctrl+I) |

### New from template

The submenu lists the files of your templates folder: `XDG_TEMPLATES_DIR` in `$XDG_CONFIG_HOME/user-dirs.dirs` (default `~/.config`), else `~/Templates`. A value equal to your home folder switches templates off, as in xdg-user-dirs. The list is read each time the menu opens; the row is absent when the folder is missing or has nothing to offer, and in the Trash, Recent and search results.

- Rows are named like the file without its extension (with it when two files of a level would read the same), folders first, in the explorer's natural order. Subfolders are nested submenus, up to three levels; a level shows at most 100 entries and a listing at most 500.
- Hidden files, broken links, links to folders, and anything that is not a regular file or a folder are skipped, and so are folders with nothing to offer. A link to a regular file is offered under the link's name and its contents are copied.
- Choosing a template copies its contents into the shown folder under the template's file name ("Report.odt"; "Report (2).odt" when taken, never overwriting anything), selects the copy and starts the inline rename. Filters and the search are cleared first, as for New file.

**Long names.** A name is at most 255 bytes. Wherever Fileflux has to invent a numbered name ("name (2).ext") it shortens the part before the extension so the result still fits.

**Dialogs never change under you.** A dialog that finishes in the background waits behind the open one and appears when it closes. Each danger button acts on exactly the paths its own dialog listed, and ignores the pointer for about 400 ms after the dialog appears so the tail of a double-click cannot confirm it.

## Trash

**Del never deletes: it moves to the Trash** and offers **Undo** in the toast. Permanent deletion only comes from Shift+Del, the Trash view, or an unavailable Trash, and always behind a confirmation.

- Items go to the home Trash on the home device (known from `$XDG_DATA_HOME` / `$HOME` at startup, so Del works before the sidebar has loaded), else to `$topdir/.Trash-$uid` of their mount. The shared `$topdir/.Trash` is not used.
- If an item has no usable Trash, a dialog asks whether to delete it permanently. Nothing is deleted without that answer; declining reports "N items not deleted (no Trash available)".
- Phones and other gvfs mounts have no Trash: deleting there asks and deletes permanently (see [Devices](devices.md#phones-and-gvfs-mounts)).
- **Trash view** (sidebar row **Home Trash**): **Restore** puts items back; on a name conflict both are kept and the restored one gets "name (2)". **Delete permanently…** and **Empty Trash** each ask first. The view refreshes after every operation.
- A name whose `.trashinfo` file would not fit (over 245 bytes) is stored under a shortened name; the recorded original path stays exact, so Restore returns the original name.

## Archives

Compressing and extracting use `bsdtar` (libarchive) as an external program, off the UI thread, with a progress card and **Cancel** (which stops the program and removes what it made). The entries appear in the context menu of a selection in a real folder (not in the Trash, Recent or search results), after the Paste group. Without `bsdtar` on `$PATH` they are shown dimmed ("needs bsdtar") and a notice says "Install libarchive (bsdtar) to compress and extract".

- **Compress…** (any selection): asks for the archive name (the entry's name, or "Archive" for several) and a format: **.zip** (default), **.tar.gz** or **.tar.xz**. The archive is written in the shown folder under a hidden temporary name and renamed into place only when `bsdtar` succeeded, so a failure or a cancel leaves no partial archive. An existing file is never replaced: the name is numbered ("Archive (2).zip"). The new archive is selected.
- **Extract here** (one archive file): extracts into a new folder next to it, named after the archive without its suffix ("backup.tar.gz" → "backup"; numbered when taken). **Extract to…** asks for a destination folder (prefilled with the other pane's folder while the view is split, else the shown folder; `~` and relative paths work) and extracts into a new folder inside it. Archives are recognised by extension: `.zip`, `.tar`, `.tar.gz`/`.tgz`, `.tar.bz2`/`.tbz2`, `.tar.xz`/`.txz`, `.tar.zst`/`.tzst`, `.7z` and `.rar` (read what the installed libarchive can read; Fileflux only writes zip, tar.gz and tar.xz).
- **Safety.** The member list is read first (`bsdtar -tf`): an archive with an absolute member name or a `..` component is refused whole with an error, and nothing is extracted. Extraction runs in a hidden folder inside the destination with `--no-same-owner --no-same-permissions` (never `-P`) and is renamed into place only at the end; a failure or cancel removes it. Afterwards any symlink whose resolved target leaves the new folder is removed and the toast says how many. Folders the archive made unreadable for you are given owner access so they can be browsed and deleted.
- **Undo** moves the created archive or folder to the Trash (if it is still the item that was created), never deleting it.

## Links

Symbolic links are created, never copied. They run as ordinary operations (progress card when several, one summary toast, per-item errors under **Details**) and are offered only in a real folder: not in the Trash, Recent or search results, and never onto a device that is being ejected. A link's target is the absolute path of the entry as Fileflux shows it (not resolved through links of its own), so a link keeps working when you move it to another folder. Nothing is ever replaced: a taken name gets a number.

- **Make Link** (context menu, any selection in a folder): creates "Link to <name>" next to each selected entry ("Link to <name> (2)" when taken) and selects the new links.
- **Paste as link** (context menu on empty space, or on one selected folder; dimmed while the clipboard is empty): creates a link to each entry on the clipboard in that folder, named after the entry (numbered when taken, like a copy). The clipboard is untouched, so a Cut is still waiting for a real Paste.
- **Drop with Ctrl+Shift** held on a folder (entries dragged inside Fileflux, and files dragged in from other applications) creates the links there instead of copying or moving. Dropping onto the folder the entries already live in does nothing; use **Make Link** there.
- **Undo** removes exactly the links that were created, each only if it is still the symlink that was created (a link replaced by another file, or already gone, is skipped and reported). A link is unlinked itself: the entry it points to, even a folder, is never touched.

## Undo

Ctrl+Z, or the toast's Undo button (undoing with Ctrl+Z closes that toast so no stale button stays), undoes the last rename, Move to Trash, New folder/file/template, copy, move, compress, extract or link. The history is per session and survives navigation.

| Operation | What undo does |
|---|---|
| Rename | Renames back |
| Move to Trash | Restores item by item, can be cancelled, never stops at the first problem, selects what it restored, and reports "restored / skipped / failed". Items that can still come back stay undoable |
| New folder / file | A new, still-empty item goes to the Trash, never straight to deletion |
| New from template | The created file goes to the Trash if it is still the file that was created (edits you made to it are kept in the Trash); a different file under the same name is left alone |
| Copy | Moves the copies it created to the Trash (never deletes them) |
| Move | Puts items back where they came from, only if each is still the item that was moved, never replacing anything that took the original place |
| Compress / Extract | The created archive or folder goes to the Trash, only if it is still the item that was created |
| Make Link / Paste as link / link drop | Unlinks the created links, each only if it is still the symlink that was created; never follows a link |

Each history entry remembers which file it acted on (device and inode). If the item at that place is no longer the same one (replaced, changed, emptied from the Trash), the toast says "Can't undo: … changed since …" and nothing is touched. Items replaced during a paste are not restorable by Undo, and **permanent deletion cannot be undone**.

## Clipboard

| Action | Behavior |
|---|---|
| Copy / Cut | Ctrl+C / Ctrl+X remember the selected entries and put them on the system clipboard as files to copy or cut: other file managers (GNOME Files, Nemo, Dolphin, Thunar…) paste them as a copy or a move through `x-special/gnome-copied-files` and `text/uri-list` (plus `application/x-kde-cutselection` for a cut), and a terminal or text field gets the paths, one per line. X11 and Wayland. Fileflux's own behaviour is unchanged: a cut moves on Paste, with the conflict dialog, progress card and Undo. Needs a build against a DBFlux GPUI with file clipboard support |
| Cut look | Cut entries are drawn dimmed until pasted or until the clipboard changes |
| Paste | Ctrl+V or **Paste** pastes into the shown folder. With exactly one folder selected, the menu offers **Paste into folder** instead. Nothing is offered in the Trash view |
| Files from other applications | Ctrl+V pastes files another application copied or cut: it reads `x-special/gnome-copied-files`, then `text/uri-list` (with the KDE cut flag), then text. A cut moves, a copy copies. As text, every line must be a `file://` URI (percent-encoded) or an absolute path, optionally after a first line `copy` or `cut`; without it the paste is a copy. Missing files are skipped, and any other text is not a file list. The system clipboard wins when it changed since Fileflux last wrote it (the same files cut by another application count as a change); otherwise the internal clipboard is used. The conflict dialog, progress card and Undo are the same as for any paste |
| After paste | A cut-and-paste empties the clipboard (a cancelled one keeps what did not move). A copy stays |

Pasting is a byte-based operation with a progress card: "Copying 12 items to Pictures", bytes done, current file, speed and an estimate after two seconds, **Pause** and **Cancel**. Folders are copied recursively, symlinks as links (never followed), and permissions and modification times are kept. A copy is built under a hidden temporary name and renamed into place, so a cancelled or failed copy never leaves a half file under the real name. A move renames when it can; across devices it copies, verifies and only then removes the source.

### Conflicts

When an item already exists in the destination, a modal asks "“report.pdf” already exists in Documents" with the size and date of both.

| Choice | Result |
|---|---|
| **Skip** (default) | Leaves the existing item |
| **Keep both** | The new one is named "report (2).pdf" |
| **Replace** (danger) | The old item is moved aside and deleted only once the new one is in place, and put back if that fails. If a leftover cannot be removed, the summary lists it instead of reporting a clean success |
| **Cancel** | Stops the whole job; what was done stays done |

"Apply to all remaining conflicts" answers the rest of the job. Pasting a copy into its own folder keeps both without asking. Moving into the folder the items are already in does nothing and says so. A folder pasted into itself or its own subfolder is an error and nothing is touched.

## Drag and drop

Drag selected entries from the grid, the list or the compact view (dragging an unselected entry selects it alone first). A sidebar Places, Favorites or mounted Devices row can be dragged too, as the one folder it stands for: it drops on folders, on the split rail and on the tab strip exactly like a dragged folder tile, and a click that does not move still opens it (the drag starts after a couple of pixels there, not 8). A drag starts only once the pointer has moved 8 px from where you pressed, the desktop's usual threshold: before that there is no preview, no target lights up, nothing can be dropped and the selection does not change, and releasing there is just a click. A slip of the hand while clicking never moves a file. The preview shows the icon and name, or "N items". A drop is a paste with explicit sources, so it uses the same conflict dialog, progress card and undo as Ctrl+V.

- **Targets:** a folder tile or row, a sidebar Places, Favorites or Devices row, a breadcrumb segment, and the empty space of the shown folder. A valid target gets a selection wash and a tint border; invalid targets (the dragged item itself, anything inside it, a file, the Trash view) show nothing and refuse the drop.
- **Action:** by default a drop **moves on the same device and copies across devices**. Hold **Ctrl** to copy or **Shift** to move (Ctrl wins), or **Ctrl+Shift** to create [links](#links) instead.
- **Home Trash row:** moves to the Trash through the same undoable job as Del.
- **From other applications:** files dropped on a folder are always copied (or linked with Ctrl+Shift), never moved, and cannot be dropped on the Trash row.
- **To other applications:** dragging past the window's edge hands the dragged entries to the desktop as a file list (`text/uri-list`), so a browser, chat or another file manager can take them; the receiving application decides whether it copies, moves or uploads. Wayland only for now: on X11 the drag stays inside Fileflux.
- **Escape** during a drag cancels it; releasing outside any target does nothing.

## Progress and toasts

An operation that runs longer than about 400 ms shows a card at the bottom right (title, bar, "N of M items", current item, **Cancel**; several stack) and a status-bar indicator, and ends in a summary toast. Cancelling stops before the next item; what was done stays done. Copy and move cards add bytes, speed and ETA, and a **Pause** button (it becomes **Resume**). Pausing stops the copy at its next 1 MiB chunk or item and the card reads "Paused"; the partly copied file stays under its hidden temporary name, and a move never removes a source while paused. Speed and ETA do not count the time spent paused. **Cancel** works while paused and cleans up like any cancel. Trash, delete, permission, link, undo, compress and extract cards have no Pause button.

Toasts stack in the **bottom-right corner** of the window, above the status bar, and grow upward, so they never cover the subbar buttons or the middle of the grid. Info and success toasts close after 3 s, a toast with **Undo** after 6 s, errors and warnings after 8 s (or at once with the close button). The timer pauses while the pointer is on a toast. At most 3 show at once: a new one closes the oldest (an error last).

When an operation fails, the toast says which item and why, in plain words, never the raw system text: "Could not move “a.txt” to Documents: you don't have permission to do that", "Could not rename “a.txt” to “b.txt”: …", "Could not create the folder “Untitled folder” in Documents: there is no space left on the device". The reason covers what the system reports most often: no permission, no space or quota left, a read-only location, a name that already exists, a folder that is not empty, an item that is in use, a device that was removed or failed, and a network location that was lost. When several items fail, the toast groups them by reason ("Could not move 3 items: you don't have permission to do that (2), there is no space left on the device (1)") and **Details** keeps one line per item with its own reason. Create, rename, copy, move, links, Compress/Extract, templates, the Trash (move, restore, delete, empty), permissions and Undo all follow this wording.

## Details panel

A panel on the right (300 px by default, resizable) that describes the selection and edits its permissions and group, toggled by the toolbar button or Ctrl+I. It is open by default and hides itself when the window is too narrow to leave the entries 520 px.

| Selection | Shows |
|---|---|
| Nothing | Folder glyph, name, item total ("N of M" while a filter is active), size on disk of the folder (see below) and full location |
| One entry | A preview (image files draw the image, videos and documents their thumbnail; others the folder or file icon), name, kind ("Image · JPG"), size, modified time, permissions (`rw-r--r--`, or `unavailable`), item count for folders, size on disk of a folder (see below), and path (Trash entries show their original location) |
| Several | The count and the total size of the selected files, and the permissions editor for all of them together |

Images, videos, PDFs, office documents and vector images are previewed; the rest show their file icon.

**Permissions** appear under the facts for one or several selected entries, in a real folder only (the Trash, Recent and search results stay read-only). The editor follows GNOME Files:

- Per class (**Owner**, **Group**, **Others**) pick **None**, **Read only** or **Read and write**. A folder's access maps to `r-x` for Read only and `rwx` for Read and write, so a readable folder stays listable and None means `---`. A folder class that is not exactly `---`, `r-x` or `rwx` (for example `r--`, or `--x` in a `711` folder) reads "(custom)" with nothing highlighted, so picking **Read only** or any other choice applies it. For files, **Allow executing** turns the execute bit on for every class that can read, or off for all.
- The setuid, setgid and sticky bits are never changed.
- **Group** lists the groups you belong to (your primary group first); the entries are given that group. If the list of your groups cannot be read, the group is shown read-only with "(groups unavailable)". The **Owner** menu changes the owner of one selected file or folder (people first, then root and system accounts). Changing an owner needs root, so Fileflux asks polkit (`pkexec`), which shows its own authentication dialog: Fileflux never sees or stores your password. Only the selected item changes: folders are not changed recursively and a symbolic link is never followed. When `fileflux-owner-helper` is installed next to the Fileflux executable, that small helper runs as root: it opens the path from `/` without following any symbolic link at any level, checks that the open file is still the one the panel showed (device, inode, birth time and owner) and changes the owner on that same open file, so a folder swapped for a link while the dialog waited cannot redirect the change. If the file was replaced or its owner changed, nothing is changed and the toast says so. Without the helper Fileflux falls back to `pkexec chown --no-dereference --from=<shown owner>`: it checks the file before and after, and if the item was replaced while the dialog waited it says so and names the path to check, but it cannot stop a parent folder swapped for a link during the wait. Package builds that install the helper should also install `dev.fileflux.owner-helper.policy` (polkit action `dev.fileflux.change-owner`, authentication as an administrator); on NixOS add the package to `environment.systemPackages` so polkit finds the policy, otherwise `pkexec` still works with its generic prompt. Cancelling the dialog or a failure leaves the owner as it was, with a message saying why. **An owner change cannot be undone** (not with Ctrl+Z either): to give the item back, change the owner again. One owner change runs at a time; the menu is disabled while one waits for authorization, and when `pkexec` (polkit) is not installed, with a tooltip saying why.
- With several entries, a value they do not share reads "(mixed)" and nothing is highlighted. **Apply** changes only the classes you picked a different value for (and the group if you picked another), so the other bits of each entry stay exactly as they were. **Reset** drops your choices.
- **Apply to enclosed files…** (when a folder is selected) asks for a second press, then gives the folder rule (`r-x`/`rwx`) to the folder and every subfolder and the file rule to every file inside, using the access the editor shows (plus the group, if you picked one). It is disabled while it would change nothing (every class mixed or custom, and no choice or group picked). It never follows symbolic links and does not enter another filesystem. It runs in the background with a progress card and Cancel.
- Symbolic links are skipped and the notice says how many: their own mode cannot be changed on Linux, and changing the target through a link is never done.
- The result is one notice: "Changed permissions of 12 items", with the failures (a file you do not own, a folder you cannot read) summarized by reason ("1 failed: you don't have permission to do that") and listed under **Details**. **Undo** (the toast's button or Ctrl+Z) restores the exact previous modes and groups of everything that changed, skipping any entry that was replaced since. A change of more than 100,000 entries cannot be undone, and the notice says so.

**Size on disk** appears for one selected folder, or for the current folder when nothing is selected. It is measured in the background after a short pause, so arrowing through folders does not walk each one: a growing "12.4 GB so far…" turns into the final value, with "N files · M folders" beside it. Selecting something else cancels the walk. The number follows the rules of `du`: allocated blocks rather than the apparent size (a sparse file counts what it occupies), each hard-linked file once, symbolic links never followed, and only the folder's own filesystem (a mount below it is skipped). Subfolders that cannot be read are skipped and the value reads "(incomplete)". Folders on gvfs mounts (phones, network shares) are not walked on their own: the panel shows **Calculate size** instead. The list and grid Size column keeps showing "N items" for folders, as GNOME Files does.

## Thumbnails

Thumbnails follow the freedesktop Thumbnail Managing Standard. Fileflux reuses thumbnails other applications already made in `$XDG_CACHE_HOME/thumbnails` (default `~/.cache/thumbnails`) when their `Thumb::URI` and `Thumb::MTime` match the file. When there is none it generates a 256 px `large` PNG there, upright per the photo's EXIF orientation, off the UI thread and at most two at a time. Images that fail to decode are recorded under `fail/fileflux-<version>/` and not retried until they change. The icon size asks for the thumbnail size that fits: the `normal` (128 px) directory is enough while a tile's picture is at most 128 px wide or tall (the grid's small, medium and large levels), and the **larger** level needs `large` (256 px) or above. Zooming up to it looks the pictures up again in `large` and generates the missing ones; zooming back keeps the larger thumbnails it already has, and a picture whose cached thumbnail is big enough is not looked up again just because the zoom changed. Fileflux never generates thumbnails for files inside the cache, on gvfs/FUSE/network mounts, or for images over 50 megapixels or 100 MB; those show the file-type icon.

Formats Fileflux does not decode — videos, PDFs, office documents, vector images — are thumbnailed by the freedesktop thumbnailers installed on the system, the same `.thumbnailer` commands pcmanfm, Nautilus and Thunar run: `~/.local/share/thumbnailers` first, then every `$XDG_DATA_DIRS/*/thumbnailers` (usually `/usr/share/thumbnailers`). Install one for the types you care about, for example `ffmpegthumbnailer` (`video/mp4`, `video/x-matroska`, …) or a PDF one (`application/pdf`). The Nix packages (`fileflux-source` and the nightly) and the dev shell already bring `ffmpegthumbnailer` for videos: its `share/thumbnailers` is appended to `XDG_DATA_DIRS`, after the system's own directories (`/usr/local/share:/usr/share` when the variable is unset), so a thumbnailer the system provides for the same type still wins. Each command runs without a shell, is killed after 10 seconds, and its output is decoded and rewritten into the cache with the standard's tags so the other file managers reuse it. With no thumbnailer for a type, the tile shows the file's icon.

## Image viewer

Opening an image shows it in a window of its own, with no sidebar: `fileflux photo.png`, a double click on an image in the explorer, or **Open in viewer**. The viewer walks the images of the folder in the explorer's order (current sort and filter).

| Task | How |
|---|---|
| Navigate | ←/→, PageUp/PageDown, Home/End, mouse back/forward, or the wheel. No wrapping at the ends |
| Zoom | The image fits the window by default and is never enlarged beyond 100% on fit. Ctrl+wheel or `+`/`-` zoom around the pointer, `0` fits, `1` shows 100%, double-click toggles. Drag to pan |
| Rotate | `R` clockwise, Shift+`R` counter-clockwise. Animated images cannot be rotated |
| Info and window | `I` toggles the info panel, `F`/F11 fullscreen, Esc/`Q` closes |

Animated GIF and WebP play. Images beyond 16384 px on a side or 100 megapixels, damaged files, and unsupported formats (HEIC, AVIF, RAW) show a message instead.

### Making Fileflux your default image viewer

```sh
scripts/install-desktop.sh                   # install the desktop entry (needs `fileflux` on PATH)
scripts/install-desktop.sh --default-images  # also make it the default for image types
scripts/install-desktop.sh --default-folders # also make it the default file manager (inode/directory)
scripts/install-desktop.sh --uninstall       # undo exactly what was installed
```

The two `--default-*` options can be combined.

The script writes only to `$XDG_DATA_HOME/applications` and `$XDG_CONFIG_HOME/mimeapps.list`, and `--uninstall` restores the defaults it replaced.

## Search in subfolders

The search box normally filters the folder you are in. The toggle next to it (a folder with a magnifier, tooltip *Search in subfolders*) or **Ctrl+Shift+F** makes it search the folder **and every folder under it** by file name instead. The box then reads "Search subfolders…".

- **Typing** starts the search about 200 ms after you stop; each new query replaces the running one. **Enter** starts it at once and moves the keyboard to the results.
- **Matching** is a case-insensitive substring of the displayed name, the rule of the folder filter. Folders match too.
- **Results stream in**, shallowest folders first, into a results view that shows each match with the folder it lives in (under the tile, after the name in the list, and in the status bar and details panel for the selection). Sort and filters apply as in any folder; the natural-order sort too.
- **Status bar:** "12 found · searching…" while the walk runs, "12 found" when it ends, "10,000 found (stopped at 10,000)" at the cap, and "12 found · stopped" after Escape. Folders that could not be read (permissions) are counted and reported at the end; the search goes on without them.
- **What is searched:** hidden files and folders are skipped while they are hidden (Ctrl+H includes them and searches again), the same rule as browsing, including a folder's `.hidden` file. Symbolic links to folders are never followed (a link that matches by name is a result, but nothing is searched through it), so a link loop cannot trap the search, and folders on other filesystems (mounted drives, network shares) are not entered. The search never changes anything on disk.
- **Cap:** at most 10,000 matches. Narrow the query to find the rest.
- **Stopping:** Escape stops a running search and keeps what it found; clearing the box, Escape again, or navigating anywhere ends it and returns to the folder. Turning the toggle off returns to the folder.
- **Using a result:** Enter opens it (a folder result navigates into it), and **Open containing folder** in the context menu goes to its folder with the file selected. Rename, copy, cut, Move to Trash, Delete permanently and Undo work on the selected results, and the search runs again afterwards so the list stays true. Results are a read-only view: nothing can be created, pasted or dropped into it.
- **Tabs and history:** a search is one history entry however often you refine it; Back returns to the folder (and Forward to the search, which runs again). Its breadcrumb is the folder plus a "Search “query”" segment. **A search tab is not saved in the session**: the tab comes back on the folder it searched, with an empty box.

**Search options.** Three toggles next to the box change how the subfolder search reads the query; each re-runs the current query and the choice lasts for the session (it is not saved):

- **Match case** (`Aa`): off by default, so "report" finds "REPORT.txt" (Unicode letters fold too, "árbol" finds "ÁRBOL"). On, case must match.
- **Regular expression** (`.*`): the query is a [regex](https://docs.rs/regex/latest/regex/#syntax) matched against the file name. An invalid expression shows its error beside the toolbar and runs nothing; the box is not cleared. An empty query matches nothing.
- **Search file contents** (the document icon): a file is a result when its name matches **or** one of its lines does. Match case and Regular expression apply to the lines too; a pattern never spans lines. Results found by content look like any other result. Only regular files of at most 10 MiB are read; binary files (a NUL byte in the first 8 KiB) and symbolic links are skipped, invalid UTF-8 is read lossily, and a file stops being read at its first match. The 10,000-result cap, streaming, Escape and the filesystem boundary are the same as for names.

The toggles apply to the subfolder search only; filtering the current folder (Ctrl+F) is always a case-insensitive name substring.

## Disk usage

**Analyze Disk Usage** shows where the space of a folder goes. Right-click the empty space of a folder, or a single selected folder, and choose it. A **Disk usage** view opens in the same tab: a ranked list on the right and a sunburst chart on the left, with the folder's total and item count in the status bar.

- **The list** ranks the folder's children by their size on disk, largest first. Each row has an icon, the name, a bar for its share of the folder, the size, and the item count of a folder. A folder with a size that is only a lower bound reads "(incomplete)" after its name, because something below it could not be read. The chevron opens a folder in place, showing its children indented under it. A folder that holds more than 200 files lists its 200 largest and folds the rest into one "N other files" row; they still count in every total.
- **The chart** draws rings: the first ring is the folder's children, the next ring the children of each of those, three rings in all. The angle of a slice is its share of its parent, and slices too thin to see are folded into one grey "Other" slice. Hover a slice for its name, size, share and item count. **Click a folder slice** to drill into it, **click the centre** to go up one folder, click a file slice to select it. The chart and the list share one selection.
- **While it measures,** the rows fill in as each folder is counted and the status bar reads "Measuring… 12.4 GB in 80,000 items so far". The chart appears when the measuring ends. **Stop** ends the walk and keeps what was counted ("Stopped — … counted so far"); leaving the view cancels it too.
- **Drilling down:** double-click a folder row (or press Enter on it, or click its slice) to see its own ranking. The whole tree was measured in one walk, so a folder inside the analysis opens instantly; the trail above the list goes back to any folder of it, and **Up** goes to the parent folder's usage (at the analysis root, to the folder itself). Double-clicking a file opens it as usual. **Analyze again** measures from scratch.
- **Cleaning up:** right-click a row or a slice for **Open**, **Open containing folder**, **Copy path**, **Move to Trash** and **Delete permanently…**. They are the normal commands: Move to Trash needs no confirmation and can be undone from its toast or with Ctrl+Z; Delete permanently asks first and cannot be undone. Del and Shift+Del work on the selected row. After a removal the item leaves the totals of every folder above it (its measured size is subtracted) and the status bar reads "9.8 GB in 79,000 items **(estimate after cleanup)**". It is an estimate: with snapshots or shared blocks (btrfs reflinks) deleting a file may free less than its measured size, and an Undo or a paste is not added back: the line then says the numbers "may have changed since the analysis", and **Analyze again** measures them.
- **What is counted:** allocated blocks, not the apparent size (a sparse file counts what it occupies); each hard-linked file once across the whole analysis, so only one name carries its bytes while the others still count as items; symbolic links never followed (a link counts its own few bytes); only the folder's own filesystem, so a mount below it is not entered. Folders that cannot be read are skipped and flagged incomplete.
- **Read-only:** nothing can be created, pasted or dropped into the view, and it never changes anything by itself. The folder inside the Trash cannot be analyzed.
- **Tabs and history:** Back and Forward step through the folders you drilled into. Its breadcrumb is the folder plus a "Disk usage" segment. **A disk usage tab is not saved in the session**: the tab comes back on the folder it measured.

## Recent files

The sidebar's Places section has a **Recent** row. It lists up to 200 files that applications opened, newest first, from the freedesktop `recently-used.xbel` (`$XDG_DATA_HOME/recently-used.xbel`, else `~/.local/share/recently-used.xbel`). The view is read-only and updates live when that file changes. Missing files and non-local entries (`sftp://`, `smb://`) are skipped. The Modified column shows when each file was last used. Enter opens a file as usual, and the context menu's **Open containing folder** goes to its folder with the file selected. Nothing can be created, pasted or dropped into Recent, and Fileflux never writes `recently-used.xbel`.

## Sidebar

Right-click a row of Places, Favorites or Devices (a Network bookmark's menu is described in [Network](#network)): **Open**, **Open in new tab**, **Open in terminal**, **Add to favorites** / **Remove from favorites**, **Copy path** and **Properties** (opens the folder with the details panel shown). The **Home Trash** row offers Open, Open in new tab and **Empty Trash…**, which asks first. Devices have extra actions: see [Devices](devices.md). A **Locked** (encrypted LUKS) device is unlocked by a click on its row or by **Unlock and mount** in its menu: `gio` asks for the passphrase in a masked dialog (never saved or logged, no keyring), then the unlocked volume opens. Unmounting or ejecting it locks it again. A disc in an optical drive appears with a disc icon; Eject unmounts it and opens the tray (an audio CD is listed but cannot be mounted; see [Devices](devices.md)).

## Current scope

- Tabs cannot be reordered or dragged between windows.
