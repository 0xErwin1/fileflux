# Fileflux

A file explorer in the Bolt family, built with GPUI and Bolt — the design system shared with [DBFlux](https://github.com/0xErwin1/dbflux) and [ReqFlux](https://github.com/0xErwin1/reqflux). Real navigation with honest states, plus the first file operations (create, rename, Trash and restore, permanent delete, open, undo) built on safety defaults: see [File operations](#file-operations). Copy, cut and paste with progress and conflict handling are in, and so is drag & drop (see [File operations](#file-operations)).

## Quick path

Prerequisites: Nix with flakes enabled, direnv installed, and the direnv hook enabled in your shell.

1. Review the project's `.envrc` (`use flake`), then run `direnv allow` once to authorize it locally.
2. Enter the project directory; direnv loads the development shell from `flake.nix` (Rust toolchain, mold, native libraries).
3. `cargo run` — build and launch the explorer at your home directory (`cargo run -- ~/Pictures` opens another folder; see [Command line](#command-line)).
4. Confirm the window opens titled *Home — Fileflux*, with the places sidebar on the left and the folder grid in the middle.

Review and authorize `.envrc` again after changing it. For shells without direnv integration, `nix develop` remains a manual fallback: `nix develop --command cargo run`.

## Command line

```text
fileflux [OPTIONS] [PATH]
fileflux [OPTIONS] IMAGE...
```

| Input | Result |
|---|---|
| no `PATH` | Opens your home directory (`/` when `$HOME` is unset). |
| a directory | Opens it. Relative paths resolve against the current directory; a leading `~` expands against `$HOME` even when quoted. |
| a file | Opens its parent directory. |
| an image file (png, jpg/jpeg, gif, webp, bmp, tif/tiff, ico, svg, qoi, tga, hdr, exr) | Opens the [image viewer](#image-viewer) on it, browsing the other images of its folder. |
| several image paths | Opens the viewer on exactly those images; directories and other files among them are ignored with a note on stderr. |
| a missing path | Prints `fileflux: '<absolute path>' does not exist` and exits with status 2; no window opens. |
| `-h`, `--help` / `-V`, `--version` | Prints the usage or `fileflux <version>` and exits with status 0. |
| an unknown option, or a second path when the first is not an image | Prints the error plus `Try 'fileflux --help' for more information.` and exits with status 2. |

Use `--` before a directory whose name starts with `-`. Pointing the explorer at a scratch directory (`fileflux /tmp/fixture`) keeps manual checks away from your real files.

## Image viewer

Opening an image (`fileflux photo.png`) shows it in a window of its own, with no sidebar.

- **Navigate:** ←/→, PageUp/PageDown, Home/End, the mouse back/forward buttons, or the wheel. It does not wrap at the ends. Images are listed in the explorer's name order.
- **Zoom:** the image fits the window by default and is never enlarged beyond 100% on fit. Ctrl+wheel or `+`/`-` zoom around the pointer, `0` fits, `1` shows 100%, double-click toggles. Drag to pan.
- **Rotate:** `R` / `Shift+R` turn the image 90° clockwise / counter-clockwise. Animated images cannot be rotated.
- **Other keys:** `I` toggles the info panel, `F`/`F11` fullscreen, `Esc`/`Q` closes the window.
- **Animation and limits:** animated GIF and WebP play. Images beyond 16384 px on a side or 100 megapixels, damaged files, and unsupported formats (HEIC, AVIF, RAW) show a message instead.

### Making Fileflux your default image viewer

```sh
scripts/install-desktop.sh                  # install the desktop entry (needs `fileflux` on PATH)
scripts/install-desktop.sh --default-images # also make it the default for image types
scripts/install-desktop.sh --uninstall      # undo exactly what was installed
```

The script only writes to `$XDG_DATA_HOME/applications` and `$XDG_CONFIG_HOME/mimeapps.list`, and `--uninstall` restores the defaults it replaced.

## What the MVP does

| Area | Behavior |
|---|---|
| Navigation | Sidebar places (Home, Desktop, Documents, Downloads, Music, Pictures, Videos per your freedesktop config), scoped **Home Trash**, and mounted disk partitions/USB storage from the mount table — one entry per device, without virtual or duplicate bind mounts; the root filesystem reads "System". Names only (full paths in tooltips); each volume shows a thin usage meter and "X free of Y" from `statvfs`. Mounting, unmounting and ejecting are not performed. |
| Path bar | A breadcrumb bar (Home icon and "Home" under `$HOME`, else `/`) with chevron separators; each segment navigates, the current folder reads strong, long paths collapse their leading segments instead of overflowing. Clicking the bar's empty space or Ctrl+L switches to the editable field: Enter navigates absolute, relative, or `~/…` paths — no shell evaluation. Escape, a click elsewhere or leaving the field restores the committed path and returns to the breadcrumbs. Invalid or missing paths report an error and keep the current listing, history, and selection. |
| History | Back/forward/up with disabled states (Alt+Left/Right/Up). Failed loads change nothing. |
| Views | A folder subbar under the toolbar shows the current folder and its "4 folders · 7 files" count. A segmented grid/list toggle switches presentation. Icon grid of fixed-size tiles (equal-size rows, left-aligned partial rows — the last row never stretches) with two-tone folders, image thumbnails and a size subline (folders show "N items"), and a details list drawn by the pinned shared `gpui_component::table::DataTable` (`Name/Size/Modified`, no row-number gutter, local/relative dates, right-aligned sizes; folders show "N items", counted in the background after the listing appears): click a header to sort (the arrow draws in the tint color, folders stay first), columns spread over the pane edge to edge with drag-to-resize honored. Sizes use one decimal SI formatter everywhere. The view toggle keeps folder, query and selection. |
| Search | Ctrl+F filters file names in the current folder only — case-insensitive, nonrecursive, with a live "n of m" count; no matches is distinct from an empty folder; Escape clears. |
| Selection | Ctrl+A selects every entry visible under the filter. Click selects, Ctrl+click toggles, Shift+click selects a range in the displayed order, Shift+Arrow and Shift+Home/End extend it from the anchor, Escape clears; arrow keys and Home/End move through the visible entries in the order the list is sorted and scroll them into view; a filtered-out selection stays selected. The table's internal selection is off, so pointer and keyboard selection stay one source of truth. |
| Open | Double click or Enter navigates into folders. On an image file (the extensions listed under [Image viewer](#image-viewer)) it opens the viewer window on it, and Left/Right walk the images of the folder in the order the explorer shows them (current sort and filter). Other files, symlinks and broken links open with the default application (`xdg-open <path>`, detached, no shell; a failure to start it is reported in the status bar). Entries in the Trash are not opened: restore them first. |
| Details panel | A 300 px read-only panel on the right of the document island, toggled by the toolbar panel button or Ctrl+I (open by default; hidden automatically when the window is too narrow to leave the entries 520 px after the island's own padding). Nothing selected: the folder glyph, name, item total ("N of M" while a filter is active) and full location. One entry: a preview (image files draw the image, contain-fit; others the folder/file glyph), name, kind ("Image · JPG"), size, local modified time, permissions (`rw-r--r--`, read off the UI thread with one `stat`, at most one probe at a time and debounced; `unavailable` in muted text when it fails), the item count for folders and path (Trash entries show their original location). Several: the count and the total size of the selected files. The panel is read-only; the actions live in the context menu and the keyboard. |
| Status bar | Left: "N items" or "N selected · <total size of the selected files>", plus loading, error and status notices (long names and paths end in an ellipsis instead of running under the right-hand side), the rename editor's refusal reason, and a compact indicator ("Moving 20000 items to Trash · 9655 of 20000 items") while an operation that takes more than ~400 ms runs. Right: the free space of the volume that holds the current folder (longest mount-path prefix) with a small success-colored square. Shortcuts live in the toolbar tooltips. |
| Live refresh | The folder of every tab, active or in the background, is watched (inotify, one shared watcher thread, one watch per distinct folder however many tabs show it); creates, deletes, moves and edits made elsewhere show up within about a second, keeping the selection of entries that still exist, the filter and the scroll position. A tab that is still opening a folder already watches it, so a change during the scan is read right after it commits. If a tab's folder is deleted, that tab falls back to the nearest existing parent with a notice, even in the background. The Trash is not watched; it is re-read when its tab is shown. |
| Favorites | Session-only: the toolbar star adds/removes the current directory. Nothing is written to configuration; the Trash cannot be favorited. |
| Loading | Directory scans, Trash reads and places discovery run off the UI thread with a generation guard: stale completions (including a newer failed request's) never overwrite newer results. |

Non-UTF8 file names keep their exact raw bytes for navigation; lossy text appears only in display strings.

## Tabs

Several folders open at once in one window. The tab strip sits at the top of the document island, above the toolbar, and appears only while two or more tabs are open: square Bolt tabs 30 px tall, the active one on the raised fill with strong text and a 2 px tint bar along its bottom edge. A tab shows the folder name (`Home` for the home directory, `Trash` for the Trash view) and carries the full path as its tooltip; the close button shows on the active tab and on hover. Tabs are session-only (nothing is written), and the command-line path opens as the first tab. The window title follows the active tab.

| Action | How |
|---|---|
| New tab | Ctrl+T opens a tab on the current folder, to the right of the active one, and switches to it. It starts with the view mode and sort of the tab it came from, and a fresh history. |
| Close tab | Ctrl+W, the close button or a middle click on the tab. **The last tab is never closed.** Closing the active tab activates its right neighbour (the left one when it was the rightmost). |
| Switch | Ctrl+Tab / Ctrl+Shift+Tab or Ctrl+PageDown / Ctrl+PageUp cycle (wrapping around); Alt+1 … Alt+9 jump to that tab (nothing happens when there is no such tab); click a tab. |
| Open in a new tab | Middle-click a folder in the grid, the list, a sidebar row (Places, Favorites, Devices, Home Trash) or a breadcrumb segment, or use **Open in new tab** in the context menu of a selected folder. The tab opens in the background, to the right of the active tab; several in a row keep their click order. Files and symlinks do nothing. |

Each tab keeps its own location, back/forward history, selection, view mode, sort, filter text, scroll position and path-edit state: navigating in one tab never moves another one's selection or filter. Window-wide on purpose: the places sidebar and favorites, the clipboard (copy in one tab, paste in another), the undo stack, the progress cards, dialogs (modal over the whole window) and toasts.

- **Focus.** Switching tabs puts the keyboard on the new tab's entries (or on its path field when that tab was mid path edit). An inline rename and an open context menu belong to the tab they started in and are cancelled when the tab changes.
- **Operations.** A file operation keeps running when you switch tabs; its toast is global. When it finishes, every tab showing a folder it changed is re-read (a trash refreshes the folder, and the Trash view), and the tab it was started from always is; operations whose reach is not known up front (restore, undo, paste) re-read every tab.
- **Background tabs are never stale.** Every tab holds its own watch on its folder through one shared watcher thread (a dedicated thread blocked in the kernel, not a worker of the background executor), so a background tab updates while you work elsewhere and is already current when you switch to it. Tabs on the same folder share one inotify watch, released when the last of them leaves. Only a tab that could not be watched (the Trash, or a failed watch) is re-read when it becomes active, keeping its selection, filter and scroll position.

## File operations

Everything that changes the disk runs off the UI thread, reports its result (toast, or an inline error) and never overwrites anything silently.

| Action | How |
|---|---|
| Context menu | Right click an entry or the empty space; also the toolbar "more" button (…), the Menu key and Shift+F10. Arrow keys, Enter and Escape drive it. On an entry: Open (Enter), **Open in viewer** (one selected image), **Open with…** (one selected file), Cut (Ctrl+X), Copy (Ctrl+C), Paste (Ctrl+V; **Paste into folder** when exactly one folder is selected; dimmed while the clipboard is empty), Rename (F2, one entry), Copy path (Ctrl+Shift+C), Properties (toggles the details panel, Ctrl+I), Move to Trash (Del), Delete permanently… (Shift+Del). On empty space: New folder (Ctrl+Shift+N), New file, Paste, Open terminal here (`$TERMINAL`, else `xdg-terminal-exec`; omitted when neither exists), Copy folder path. |
| Open with… | The menu item for one selected file (not folders, not in the Trash). The file's MIME type comes from `xdg-mime query filetype <path>` (no shell; the file extension is the fallback when `xdg-mime` is missing), the default application (`xdg-mime query default`) is listed first, then every other application whose `.desktop` file lists that type in `MimeType=`, found in `$XDG_DATA_HOME/applications` (else `~/.local/share/applications`) and each `$XDG_DATA_DIRS/applications` (else `/usr/local/share:/usr/share`); earlier directories hide a same-named file of later ones, and `NoDisplay=true`, `Hidden=true`, `Terminal=true` and applications whose `Exec=` takes no file (`%f %F %u %U`) or puts a field code inside a quoted argument (forbidden by the Desktop Entry spec; with `sh -c "… %f"` it would let a file name run as a shell command) are not listed. Every `text/*` type also matches applications that list `text/plain`. Up/Down choose, Enter or a click opens: `Exec=` is split by the Desktop Entry quoting rules, `%f`/`%F` become the path, `%u`/`%U` its `file://` URI, `%i %c %k` and the other codes are dropped, `%%` is a percent sign, and the program is started without a shell, detached, in the file's folder. When nothing handles the type a toast says so. There is no "Other application…" entry. |
| New folder / New file | The subbar's **New folder** chip, Ctrl+Shift+N or the menu create "Untitled folder" / "Untitled file" (numbered when taken) and start the inline rename on it. |
| Rename | F2 edits the name in place in the grid and the list; the stem (up to the last dot) is preselected, folders select the whole name. Enter commits, Escape cancels, leaving the field commits. The name is kept exactly as typed — nothing is trimmed (Linux allows leading and trailing spaces; the toast says when they are kept, and a name of only spaces is refused). A name that contains a control character (a line break, a tab…) cannot be edited inline — the single-line field would silently change it, so F2 on it shows "This name has characters that can't be edited here" and leaves the file untouched. A refused name (empty, blank, contains `/`, already exists) stays in the field with the reason under it (and in the status bar); leaving the field with a refused name cancels the rename with a message and changes nothing on disk. |
| Copy, Cut, Paste | Ctrl+C / Ctrl+X remember the selected entries (and write their paths, one per line, to the system clipboard as plain text, so pasting into a terminal gives paths; the clipboard of the app is internal and not a file clipboard other programs understand). **Cut entries are drawn dimmed** until they are pasted or the clipboard changes (a new Copy/Cut, or any other text copied since, forgets them). Ctrl+V and the menu's **Paste** go into the folder that is shown; the menu on a single selected folder offers **Paste into folder** for that folder instead. Nothing is offered in the Trash view. Pasting runs off the UI thread as a byte-based operation: the progress card reads "Copying 12 items to Pictures" / "Moving “a.txt” to Pictures" with a bar, "1.2 MB of 4 MB", the current file, speed and an estimate once it has run for two seconds, and **Cancel**; the status bar shows the compact indicator, and the summary toast says what was copied or moved, skipped and failed (with Details). Folders are copied recursively, symlinks as links (never followed), permissions and modification times are kept; a copy is built under a hidden temporary name and renamed into place, so a cancelled or failed copy never leaves a half file under the real name; a move renames when it can and, across devices, copies, verifies and only then removes the source. After a cut-and-paste completes the clipboard is empty (a cancelled one keeps what did not move); after a copy it stays. |
| Conflicts | When an item of the paste already exists in the destination, a modal asks “report.pdf” already exists in Documents, with the size and modified date of both items. **Skip** (the default button), **Keep both** (the new one is named "report (2).pdf"), **Replace** (the danger button: the old item is moved aside and deleted only once the new one is in place, and put back if that fails; a read-only folder inside the replaced item is unlocked to delete it, and if the hidden leftover still cannot be removed the summary says "N replaced original could not be removed" and lists it instead of reporting a clean success) and **Cancel** (stops the whole job; what was already done stays done). "Apply to all remaining conflicts" answers the rest of the job without asking again. Replace is never the default and, like every danger button, ignores the pointer for ~400 ms after its dialog appears (it is drawn disabled meanwhile), so the trailing click of a double-click on the dialog before it cannot confirm it. Pasting a copy into the folder it came from keeps both without asking ("report (2).pdf"); moving into the folder the items are already in does nothing and says so; a folder pasted into itself or into one of its own subfolders is an error toast and nothing is touched. |
| Move to Trash | Del moves each selected entry to the freedesktop Trash (home Trash on the home device — known from `$XDG_DATA_HOME` / `$HOME` at startup, so Del works before the sidebar has finished loading — else `$topdir/.Trash-$uid` of its mount) and offers **Undo** in the toast. If an item has no usable Trash a dialog asks whether to delete it permanently instead; nothing is deleted without that answer, and declining (or cancelling the operation before the question) is reported in a toast as "N items not deleted (no Trash available)". |
| Drag & drop | Drag one or more selected entries from the grid or the list (dragging an unselected entry selects it alone first); the preview shows the item's icon and name, or "N items". **Drop targets:** a folder tile or list row, a sidebar Places / Favorites / Devices row, a breadcrumb segment, and the empty space of the shown folder (dropping entries in the folder they already are in does nothing). The target under the pointer gets the selection wash and a tint border; invalid targets — the dragged item itself, anything inside it, a file, the Trash view — show nothing and refuse the drop. **Action:** a drop is a paste with explicit sources, mode and target, so it uses the same transfer job, conflict dialog (Skip / Keep both / Replace / Cancel), progress card and undo as Ctrl+V. By default it **moves on the same device and copies across devices**; hold **Ctrl** at the drop to copy, **Shift** to move (Ctrl wins when both are held). Dropping on the sidebar **Home Trash** row moves to the Trash through the same undoable job as Del. Files dragged in from other applications onto a folder or the shown folder are always **copied**, never moved, and cannot be dropped on the Trash row. **Escape** during a drag cancels it; releasing outside any target does nothing. |
| Delete permanently | Shift+Del opens a confirmation naming the item or the count, with **Cancel** as the default button (Enter alone never deletes) and the danger button "Delete permanently". Folders are removed recursively; symlinks are never followed. Dialogs never change under you: one that finishes in the background (for example "No Trash available" after a large Move to Trash, or a paste's conflict question) waits behind the open one and appears when it closes, and each danger button deletes exactly the paths its own dialog listed. A dialog that has just appeared (opened or promoted from the line) ignores its danger button for ~400 ms. |
| Trash view | Restore puts items back (on a name conflict both are kept: the restored one gets "name (2)" and the toast names it). Delete permanently… and the **Empty Trash** button each ask first. The view refreshes after every operation. |
| Long names | A name is at most 255 bytes. Wherever Fileflux has to invent a numbered name — pasting a copy into its own folder, **Keep both**, a Trash restore that finds its place taken — the part before the extension is shortened (on a character boundary; non-UTF-8 names byte-wise) so "name (2).ext" still fits, and the search for a free name is bounded and can be cancelled. A name whose `.trashinfo` file would not fit (over 245 bytes) is stored in the Trash under a shortened name; the recorded original path stays exact, so Restore returns the original name. |
| Undo | Ctrl+Z (or the toast's Undo button; undoing with Ctrl+Z closes that toast so no stale Undo button stays) undoes the last rename, Move to Trash, New folder/file (a new, still-empty item goes to the Trash, never straight to deletion), copy or move. Undoing a **copy** moves the copies it created to the Trash (never deletes them); undoing a **move** puts the items back where they came from, only if each is still the item that was moved, never replacing anything that took the original place, and reports what could not return (those stay undoable). **Items replaced during a paste are not restorable by Undo** (the toast says so, and they are left out of the undo). The history is per session and survives navigation. Each entry remembers which file it acted on (device and inode, and for the Trash also the item's `.trashinfo` origin); an undo acts only if the item at that place is still the same one. One that changed since — replaced by another file under the same name, no longer empty, emptied from the Trash, its `.trashinfo` rewritten — is reported as "Can't undo: … changed since …" and nothing is touched. Undoing a multi-item Move to Trash restores item by item, can be cancelled, never stops at the first problem, selects what it restored and reports "restored / skipped / failed"; items it did not restore and that can still come back (cancelled, name taken, I/O error) stay undoable through the toast's Undo. Permanent deletion cannot be undone. |
| Progress | An operation running longer than ~400 ms shows a non-modal card bottom-right (title, bar, "N of M items", current item, **Cancel**; several stack) and a status-bar indicator, and ends in a summary toast ("Cancelled: moved 12441 items to Trash" keeps its Undo). Cancelling stops before the next item; what was done stays done. The card (`explorer/progress.rs`) shows bytes, speed and ETA for copy and move. |

Safety defaults: Del never deletes — it moves to the Trash; permanent deletion only comes from Shift+Del, the Trash view or an unavailable Trash, always behind a confirmation that a background result cannot replace and Enter alone cannot accept; renames, creations and pastes never replace an existing entry without an explicit Replace; undo never touches an item that is not the one the operation acted on; a navigation always wins over the refresh of a finished operation; operations act only on the entries visible under the current filter; Trash write support covers the home Trash and `$topdir/.Trash-$uid` (not the shared `$topdir/.Trash`).

## Checks

| Command | What it does |
|---|---|
| `cargo check --all-targets` | Type-checks everything |
| `cargo test` | Runs the model and state tests |
| `cargo fmt --all -- --check` | Formats the sources |
| `cargo clippy --all-targets -- -D warnings` | Lints with the family lint set |
| `cargo build --locked` | Builds the binary |

Keyboard: Ctrl+T new tab · Ctrl+W close tab · Ctrl+Tab / Ctrl+Shift+Tab or Ctrl+PageDown / Ctrl+PageUp switch tabs · Alt+1…9 jump to a tab · Ctrl+L path · Ctrl+F search · Alt+←/→/↑ history · Ctrl+R reload · Ctrl+I details panel · Enter open(selected) · F2 rename · Del move to Trash · Shift+Del delete permanently · Ctrl+Shift+N new folder · Ctrl+C copy · Ctrl+X cut · Ctrl+V paste · Ctrl+Z undo · Ctrl+A select all · Ctrl+Shift+C copy path · Menu / Shift+F10 context menu · Escape cancel/clear. Escape while the path field or the search field is focused restores the committed path or clears the query and returns the keyboard to the folder contents; Escape while dragging cancels the drag; Escape elsewhere clears the selection.

## Native smoke helper

`nix develop --command python3 scripts/native-smoke.py` (X11/XWayland, stdlib + `libXtst` only) launches the local build against a synthetic HOME/XDG fixture, drives its own window via XTest, and asserts the title retention around path edits and failed submits. It reports what it cannot automate honestly as `MANUAL` (exit 3) and never claims a visual check it did not perform.

## Current scope

There is no dual-pane view and tabs cannot be reordered or dragged between windows; the details panel previews image files only. The window title shows the current folder plus Fileflux; the empty top gutter matches the 8 px side gutters (no brand text or icon). Modification dates are displayed in UTC without a timezone database. The sidebar toggle hides or shows the sidebar at any window width, and narrow windows park it as an overlay; path, search and view controls always stay reachable. The app always starts in Bolt's dark palette, whatever the operating system's appearance; there is no theme toggle.

## Design

The implemented look follows [`design/file-explorer/01-familiar-grid.html`](design/file-explorer/01-familiar-grid.html) and [`design/system.md`](design/system.md): desk, 8 px gutters, 290 px sidebar, opposing 14 px island chamfers, square selection rows with the 2 px tint bar, and Bolt's palette and font roles. `02`/`03` artboards remain alternatives.

## Pinned stack

The GPUI stack follows DBFlux, exactly as ReqFlux does, so all three apps build against one GPUI and one design system:

| Package | Pin | Why |
|---|---|---|
| `gpui-pre` 0.3.9 | git rev `bf3cbd0` | DBFlux's patched GPUI (frame-observer hooks) |
| `gpui-component` 0.7.3 | git rev `bf3cbd0` | DBFlux's patched editor build |
| `gpui-pre-linux` 0.3.9 | git rev `bf3cbd0` | DBFlux's Wayland keymap fix |
| `bolt` | git rev `bf3cbd0` | The family design system |

The rev is DBFlux's main where Bolt was merged; move it together with DBFlux. The Rust toolchain is pinned at 1.99.1 in [`rust-toolchain.toml`](rust-toolchain.toml) and installed by the Nix shell — never globally. `flake.lock` pins Nix inputs to the same revisions ReqFlux ships, so the shell contents are proven.
