# User guide

How to browse, organize and manage files with Fileflux. Every key is in [Keyboard shortcuts](keyboard-shortcuts.md).

## Quick path

1. Run `fileflux` (your home folder) or `fileflux ~/Pictures` (another folder).
2. Click a place in the sidebar, or press Ctrl+L and type a path.
3. Right-click anything: the context menu is where every action lives.

## Contents

- [Command line](#command-line)
- [Window layout](#window-layout)
- [Navigation](#navigation)
- [Views](#views)
- [Sort and filter](#sort-and-filter)
- [Tabs](#tabs)
- [Selection](#selection)
- [Favorites and GTK bookmarks](#favorites-and-gtk-bookmarks)
- [Open, Open with and terminal](#open-open-with-and-terminal)
- [File operations](#file-operations)
- [Trash](#trash)
- [Undo](#undo)
- [Clipboard](#clipboard)
- [Drag and drop](#drag-and-drop)
- [Progress and toasts](#progress-and-toasts)
- [Details panel](#details-panel)
- [Thumbnails](#thumbnails)
- [Image viewer](#image-viewer)
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

## Window layout

A sidebar of places on the left, the folder in the middle, and an optional [details panel](#details-panel) on the right.

| Part | What it does |
|---|---|
| Toolbar | Sidebar toggle, back, forward, up, the path bar, grid/list toggle, details toggle, favorite star, reload (Ctrl+R), "more" menu (…) |
| Folder subbar | Current folder name, "4 folders · 7 files" count, **Sort**, **Filter**, **New folder**, active filter chips |
| Status bar | "N items" or "N selected · size", loading and error notices, a progress indicator for long operations, free space of the current volume |

Drag the right edge of the sidebar or the left edge of the details panel (a 6 px handle with a tint line on hover) to resize them; double-click a handle to restore the default width (290 px sidebar, 300 px details). The sidebar is clamped to 200–600 px and always leaves the folder pane 360 px; the details panel to 240–600 px and never wider than the window leaves room for the entries. Narrow windows park the sidebar as an overlay, without a resize handle, keeping its width; the sidebar toggle shows or hides it at any width, and the path, search and view controls always stay reachable. The widths are remembered (`sidebar_width`, `details_width` in [settings](settings.md#files-and-locations)).

The window title is the current folder plus Fileflux ("Home — Fileflux") and follows the active tab. Fileflux always starts in Bolt's dark palette; there is no theme toggle.

## Navigation

| Control | Behavior |
|---|---|
| Sidebar | Home, Desktop, Documents, Downloads, Music, Pictures and Videos (from your freedesktop user-dirs config), your favorites, the Home Trash and the Devices section. Names only; full paths in tooltips. Volumes come from the mount table, one entry per device without virtual or duplicate bind mounts (the root filesystem reads "System"), each with a usage meter and "X free of Y" from `statvfs`. See [Devices](devices.md) |
| Breadcrumbs | A Home icon and "Home" under `$HOME`, else `/`, with chevron separators. Each segment navigates and the current folder reads strong. Long paths collapse their leading segments instead of overflowing |
| Path field | Click the bar's empty space or press Ctrl+L. Enter navigates to an absolute, relative or `~/…` path (no shell evaluation). Escape or leaving the field restores the committed path. An invalid or missing path reports an error and changes nothing |
| History | Alt+←, Alt+→ and Alt+↑ (back, forward, up). A failed load changes nothing |
| Favorites | Your GTK bookmarks (`~/.config/gtk-3.0/bookmarks`, shared with Nautilus and Thunar). The toolbar star, a folder's context menu and a sidebar row's menu add or remove one; an edit made by another program shows live. The Trash cannot be a favorite. See [Favorites and GTK bookmarks](#favorites-and-gtk-bookmarks) |

Non-UTF-8 file names keep their exact bytes for navigation; lossy text appears only in what is displayed.

### Live refresh

Every tab's folder, including background tabs, is watched. Changes made elsewhere (creates, deletes, moves, edits) appear within about a second, keeping the selection of entries that still exist, the filter and the scroll position. If a tab's folder is deleted, that tab falls back to its nearest existing parent with a notice. The Trash is not watched; it is re-read when its tab is shown.

## Views

A segmented toggle in the toolbar switches between two presentations. The toggle keeps the folder, search and selection.

| View | What you see |
|---|---|
| Grid | Fixed-size tiles with two-tone folders, image thumbnails and a size line (folders show "N items"). Rows are equal; the last partial row is left-aligned |
| List | Columns Name, Size and Modified. Click a header to sort (folders stay first). Columns fill the pane; drag to resize. Folder item counts are filled in the background after the listing appears |

Sizes use one SI formatter everywhere. Modified dates are shown in UTC.

## Sort and filter

Sort and filter live in the tab and apply to the grid and the list alike. Selection, arrow keys, ranges, Ctrl+A, Enter, the image viewer's list, paste targets and the details panel all follow the filtered and sorted result. A new tab inherits them.

Use the subbar's **Sort** and **Filter** buttons, the same two pages in the empty-space context menu, or the list header.

| Control | Behavior |
|---|---|
| Sort | **Name, Size, Modified, Type**, ascending or descending; the default is **Modified, descending** (newest first). *Type* orders by category, then extension, then name. **Folders first** (default on) groups folders before files. **Natural number order** (default on) puts `IMG_2` before `IMG_10`. A missing size or time sorts lowest |
| Type | Multi-select chips: Folders, Images, Videos, Audio, Documents, Archives, Code, Other, decided by extension. None selected means all |
| Hidden files | Ctrl+H, or *Show hidden files* in the Filter menu. **Hidden by default**: names starting with a dot and the names listed one per line in the folder's `.hidden` file (Nautilus and Thunar's convention). No chip in the default state; showing them adds a **Hidden shown ×** chip. While some are hidden the counts read "5 of 8 items" |
| Modified | Today, Last 7 days, Last 30 days, This year, Older (before the last 30 days). One choice or none. Files with an unknown time never match |
| Size | < 1 MB, 1–100 MB, 100 MB–1 GB, > 1 GB. One choice or none. Files with an unknown size never match |
| Folders and ranges | A size range never applies to folders. A date range applies to folders only when **Folders is the only type chip selected**; otherwise folders pass it |
| Search | Ctrl+F filters names in the current folder: case-insensitive, nonrecursive, with a live "n of m" count. No matches is distinct from an empty folder |
| Combining | Every filter and the search combine with AND |

Each active filter is a removable chip next to the folder title, with **Clear all** when there are several. While anything filters, the subbar, status bar and details panel read "N of M items". **New folder** and **New file** clear the search and filters so the new item is visible.

**Large folders** (more than a few thousand entries) are arranged on a background thread, so typing and clicking never wait. The previous order stays on screen meanwhile, and a **Sorting…** indicator appears in the status bar only when a result takes longer than about 150 ms. A folder of a million entries stays responsive.

## Tabs

The tab strip appears above the toolbar once two or more tabs are open. A tab shows the folder name (`Home`, `Trash`) with the full path as its tooltip.

| Action | How |
|---|---|
| New tab | Ctrl+T. It starts with the view, sort and filters of the tab it came from, and a fresh history |
| Close tab | Ctrl+W, the close button, or a middle click. The last tab is never closed. Closing the active tab activates its right neighbour (the left one when it was the rightmost) |
| Switch | Ctrl+Tab / Ctrl+Shift+Tab, Ctrl+PageDown / Ctrl+PageUp, Alt+1 … Alt+9, or click |
| Open in a new tab | Middle-click a folder (grid, list), a sidebar row or a breadcrumb segment, or **Open in new tab** in the context menu. It opens in the background, to the right of the active tab. Files do nothing |

Each tab keeps its own location, history, selection, view, sort, filter text, scroll position and path-edit state. Shared across the window: the sidebar and favorites, the clipboard (copy in one tab, paste in another), the undo history, progress cards, dialogs and toasts.

- Switching tabs cancels an inline rename and closes an open context menu.
- A file operation keeps running when you switch tabs. When it finishes, every tab showing a folder it changed is re-read.
- Tabs are restored at the next launch (see [Settings](settings.md)).

## Selection

| Gesture | Result |
|---|---|
| Click | Select one entry |
| Ctrl+click | Toggle one entry |
| Shift+click | Select a range, in the displayed order |
| ↑ ↓ ← →, Home, End | Move through the visible entries and scroll them into view |
| Shift + those keys | Extend the selection from the anchor |
| Ctrl+A | Select every entry visible under the filter |
| Escape | Clear the selection |

A selected entry that the filter hides stays selected, but operations act only on the entries that are visible.

## Favorites and GTK bookmarks

The sidebar's Favorites are the local lines of `$XDG_CONFIG_HOME/gtk-3.0/bookmarks` (default `~/.config/gtk-3.0/bookmarks`), the file Nautilus, Thunar, Nemo and the GTK file chooser share.

- A line is `file:///percent-encoded/path Optional Label`. The sidebar shows the label when there is one, else the folder name.
- `sftp://`, `smb://` and other lines, comments and blank lines are kept untouched and not shown. Adding appends one line, removing drops only that folder's lines; order, labels and line endings stay.
- Changes are written atomically (a temporary file renamed over the original, through a symlink, keeping the file's permissions) after re-reading the file, so an edit made by another program meanwhile is not lost. An external edit shows in the sidebar live.
- Older versions kept favorites in `settings.toml`: the first run moves them into the bookmarks and saves the settings without them. If the bookmarks cannot be written they stay in `settings.toml` and are retried at the next launch.

## Open, Open with and terminal

- **Open** (double click or Enter): folders navigate. Images open the [image viewer](#image-viewer). A symlink to a folder is entered like the folder. Files, links to files and broken links open with the default application (`xdg-open <path>`, detached, no shell); a failure to start it appears in the status bar. Trash entries are not opened: restore them first.
- **Open with…** (context menu, one selected file; not folders, not in the Trash): lists the default application first, then every other application whose `.desktop` file declares the file's MIME type. Up/Down choose, Enter or a click opens. The MIME type comes from `xdg-mime query filetype` (the extension is the fallback). Applications are found in `$XDG_DATA_HOME/applications` (else `~/.local/share/applications`) and each `$XDG_DATA_DIRS/applications` (else `/usr/local/share:/usr/share`); a same-named file in an earlier directory hides the later one. Every `text/*` type also matches applications that list `text/plain`. `NoDisplay=true`, `Hidden=true`, `Terminal=true` entries, ones whose `Exec=` takes no file (`%f %F %u %U`), and ones that put a field code inside a quoted argument (forbidden by the Desktop Entry spec, and with `sh -c "… %f"` it would let a file name run as a shell command) are not listed. `Exec=` is split by the Desktop Entry quoting rules: `%f`/`%F` become the path, `%u`/`%U` its `file://` URI, `%%` a percent sign, and the other codes are dropped. The program is started without a shell, detached, in the file's folder. If nothing handles the type, a toast says so. There is no "Other application…" entry.
- **Open in terminal** (context menu on one selected folder, on a sidebar row and on empty space): starts your terminal in that folder (the shown folder on empty space), with no shell. The terminal is the first that exists of: `$TERMINAL`, `xdg-terminal-exec`, the desktop ids in `xdg-terminals.list`, a desktop entry with `Categories=TerminalEmulator` (known terminals first), then ghostty, kitty, alacritty, foot, wezterm, gnome-terminal, kgx, konsole, xfce4-terminal, xterm. Terminals that ignore the working directory of the process get their own argument: `--working-directory=DIR` (ghostty, foot, gnome-terminal, kgx, xfce4-terminal), `--working-directory DIR` (alacritty), `--directory DIR` (kitty), `--workdir DIR` (konsole), `start --cwd DIR` (wezterm); xterm uses its working directory, and a terminal found through a desktop entry uses the entry's `X-TerminalArgDir` when it has one. `<desktop>-xdg-terminals.list` variants for `$XDG_CURRENT_DESKTOP` are tried first, and the lists are read from `$XDG_CONFIG_HOME` then `$XDG_CONFIG_DIRS`. The lookup reads `.desktop` files, so it runs off the UI thread at startup; the item is omitted until a terminal is found, and when none is.
- **Open in viewer** (context menu, one selected image), and **Open in new tab**, **Add to favorites** / **Remove from favorites** (selected folder, not in the Trash).

The context menu on an entry offers, as they apply: Open, Open in new tab, Add to / Remove from favorites, Open in terminal, Open in viewer, Open with…, Cut, Copy, Paste (**Paste into folder** when exactly one folder is selected; dimmed while the clipboard is empty), Rename, Copy path, Properties, Move to Trash and Delete permanently…. On empty space: New folder, New file, Paste, **Sort by ›** and **Filter ›** (the same options as the subbar menus, each opening its page inside the menu with a Back row), Open in terminal and Copy folder path.

## File operations

Everything that changes the disk runs off the UI thread, reports its result in a toast or an inline error, and never overwrites anything silently. Right-click an entry or the empty space (or use the toolbar "more" button, the Menu key or Shift+F10). The menu is headed by its target ("report.pdf", "3 items", "Home (this folder)") and the target keeps a tint outline while it is open.

| Action | How |
|---|---|
| New folder / New file | The subbar's **New folder** chip, Ctrl+Shift+N or the menu. Creates "Untitled folder" or "Untitled file" (numbered when taken) and starts the inline rename |
| Rename | F2. The stem is preselected (folders select the whole name). Enter commits, Escape cancels, leaving the field commits. The name is kept exactly as typed. A refused name (empty, blank, contains `/`, already exists) stays in the field with the reason shown; leaving the field with a refused name cancels the rename and changes nothing. A name containing a control character cannot be edited inline: F2 says so and leaves the file untouched |
| Copy path | Ctrl+Shift+C. **Copy folder path** on empty space |
| Move to Trash | Del. See [Trash](#trash) |
| Delete permanently | Shift+Del. Opens a confirmation naming the item or the count; **Cancel** is the default and Enter alone never deletes. Folders are removed recursively; symlinks are never followed |
| Properties | Toggles the [details panel](#details-panel) (Ctrl+I) |

**Long names.** A name is at most 255 bytes. Wherever Fileflux has to invent a numbered name ("name (2).ext") it shortens the part before the extension so the result still fits.

**Dialogs never change under you.** A dialog that finishes in the background waits behind the open one and appears when it closes. Each danger button acts on exactly the paths its own dialog listed, and ignores the pointer for about 400 ms after the dialog appears so the tail of a double-click cannot confirm it.

## Trash

**Del never deletes: it moves to the Trash** and offers **Undo** in the toast. Permanent deletion only comes from Shift+Del, the Trash view, or an unavailable Trash, and always behind a confirmation.

- Items go to the home Trash on the home device (known from `$XDG_DATA_HOME` / `$HOME` at startup, so Del works before the sidebar has loaded), else to `$topdir/.Trash-$uid` of their mount. The shared `$topdir/.Trash` is not used.
- If an item has no usable Trash, a dialog asks whether to delete it permanently. Nothing is deleted without that answer; declining reports "N items not deleted (no Trash available)".
- Phones and other gvfs mounts have no Trash: deleting there asks and deletes permanently (see [Devices](devices.md#phones-and-gvfs-mounts)).
- **Trash view** (sidebar row **Home Trash**): **Restore** puts items back; on a name conflict both are kept and the restored one gets "name (2)". **Delete permanently…** and **Empty Trash** each ask first. The view refreshes after every operation.
- A name whose `.trashinfo` file would not fit (over 245 bytes) is stored under a shortened name; the recorded original path stays exact, so Restore returns the original name.

## Undo

Ctrl+Z, or the toast's Undo button (undoing with Ctrl+Z closes that toast so no stale button stays), undoes the last rename, Move to Trash, New folder/file, copy or move. The history is per session and survives navigation.

| Operation | What undo does |
|---|---|
| Rename | Renames back |
| Move to Trash | Restores item by item, can be cancelled, never stops at the first problem, selects what it restored, and reports "restored / skipped / failed". Items that can still come back stay undoable |
| New folder / file | A new, still-empty item goes to the Trash, never straight to deletion |
| Copy | Moves the copies it created to the Trash (never deletes them) |
| Move | Puts items back where they came from, only if each is still the item that was moved, never replacing anything that took the original place |

Each history entry remembers which file it acted on (device and inode). If the item at that place is no longer the same one (replaced, changed, emptied from the Trash), the toast says "Can't undo: … changed since …" and nothing is touched. Items replaced during a paste are not restorable by Undo, and **permanent deletion cannot be undone**.

## Clipboard

| Action | Behavior |
|---|---|
| Copy / Cut | Ctrl+C / Ctrl+X remember the selected entries and also write their paths, one per line, to the system clipboard as plain text, so pasting into a terminal gives paths. The clipboard is internal: other programs do not see files |
| Cut look | Cut entries are drawn dimmed until pasted or until the clipboard changes |
| Paste | Ctrl+V or **Paste** pastes into the shown folder. With exactly one folder selected, the menu offers **Paste into folder** instead. Nothing is offered in the Trash view |
| After paste | A cut-and-paste empties the clipboard (a cancelled one keeps what did not move). A copy stays |

Pasting is a byte-based operation with a progress card: "Copying 12 items to Pictures", bytes done, current file, speed and an estimate after two seconds, and **Cancel**. Folders are copied recursively, symlinks as links (never followed), and permissions and modification times are kept. A copy is built under a hidden temporary name and renamed into place, so a cancelled or failed copy never leaves a half file under the real name. A move renames when it can; across devices it copies, verifies and only then removes the source.

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

Drag selected entries from the grid or the list (dragging an unselected entry selects it alone first). The preview shows the icon and name, or "N items". A drop is a paste with explicit sources, so it uses the same conflict dialog, progress card and undo as Ctrl+V.

- **Targets:** a folder tile or row, a sidebar Places, Favorites or Devices row, a breadcrumb segment, and the empty space of the shown folder. A valid target gets a selection wash and a tint border; invalid targets (the dragged item itself, anything inside it, a file, the Trash view) show nothing and refuse the drop.
- **Action:** by default a drop **moves on the same device and copies across devices**. Hold **Ctrl** to copy or **Shift** to move (Ctrl wins).
- **Home Trash row:** moves to the Trash through the same undoable job as Del.
- **From other applications:** files dropped on a folder are always copied, never moved, and cannot be dropped on the Trash row.
- **Escape** during a drag cancels it; releasing outside any target does nothing.

## Progress and toasts

An operation that runs longer than about 400 ms shows a card at the bottom right (title, bar, "N of M items", current item, **Cancel**; several stack) and a status-bar indicator, and ends in a summary toast. Cancelling stops before the next item; what was done stays done. Copy and move cards add bytes, speed and ETA.

Toasts stack in the **bottom-right corner** of the window, above the status bar, and grow upward, so they never cover the subbar buttons or the middle of the grid. Info and success toasts close after 3 s, a toast with **Undo** after 6 s, errors and warnings after 8 s (or at once with the close button). The timer pauses while the pointer is on a toast. At most 3 show at once: a new one closes the oldest (an error last).

## Details panel

A read-only panel on the right (300 px by default, resizable), toggled by the toolbar button or Ctrl+I. It is open by default and hides itself when the window is too narrow to leave the entries 520 px.

| Selection | Shows |
|---|---|
| Nothing | Folder glyph, name, item total ("N of M" while a filter is active) and full location |
| One entry | A preview (image files draw the image; others the folder or file glyph), name, kind ("Image · JPG"), size, modified time, permissions (`rw-r--r--`, or `unavailable`), item count for folders, and path (Trash entries show their original location) |
| Several | The count and the total size of the selected files |

Only image files are previewed.

## Thumbnails

Image tiles in the grid use the freedesktop thumbnail cache (`$XDG_CACHE_HOME/thumbnails/{normal,large,x-large}`) when it holds a current thumbnail: the file name is the MD5 of the image's canonical `file://` URI, and the PNG's `Thumb::URI` must match and `Thumb::MTime` must equal the file's modification time. Otherwise the image itself is loaded. Fileflux only reads the cache and never creates thumbnails.

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
scripts/install-desktop.sh                  # install the desktop entry (needs `fileflux` on PATH)
scripts/install-desktop.sh --default-images # also make it the default for image types
scripts/install-desktop.sh --uninstall      # undo exactly what was installed
```

The script writes only to `$XDG_DATA_HOME/applications` and `$XDG_CONFIG_HOME/mimeapps.list`, and `--uninstall` restores the defaults it replaced.

## Recent files

The sidebar's Places section has a **Recent** row. It lists up to 200 files that applications opened, newest first, from the freedesktop `recently-used.xbel` (`$XDG_DATA_HOME/recently-used.xbel`, else `~/.local/share/recently-used.xbel`). The view is read-only and updates live when that file changes. Missing files and non-local entries (`sftp://`, `smb://`) are skipped. The Modified column shows when each file was last used. Enter opens a file as usual, and the context menu's **Open containing folder** goes to its folder with the file selected. Nothing can be created, pasted or dropped into Recent, and Fileflux never writes `recently-used.xbel`.

## Sidebar

Right-click a row of Places, Favorites or Devices: **Open**, **Open in new tab**, **Open in terminal**, **Add to favorites** / **Remove from favorites**, **Copy path** and **Properties** (opens the folder with the details panel shown). The **Home Trash** row offers Open, Open in new tab and **Empty Trash…**, which asks first. Devices have extra actions: see [Devices](devices.md).

## Current scope

- No dual-pane view; tabs cannot be reordered or dragged between windows.
- The details panel previews image files only.
- Modified dates are shown in UTC without a timezone database.
- Per-folder view memory is not implemented: the active tab's view, sort and filters are the default for the next launch.
