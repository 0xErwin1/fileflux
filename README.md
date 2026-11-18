# Fileflux

A file explorer in the Bolt family, built with GPUI and Bolt — the design system shared with [DBFlux](https://github.com/0xErwin1/dbflux) and [ReqFlux](https://github.com/0xErwin1/reqflux). Real navigation with honest states, plus the first file operations (create, rename, Trash and restore, permanent delete, open, undo) built on safety defaults: see [File operations](#file-operations). Copy, cut, paste and drag & drop are still to come.

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
```

| Input | Result |
|---|---|
| no `PATH` | Opens your home directory (`/` when `$HOME` is unset). |
| a directory | Opens it. Relative paths resolve against the current directory; a leading `~` expands against `$HOME` even when quoted. |
| a file | Opens its parent directory. |
| a missing path | Prints `fileflux: '<absolute path>' does not exist` and exits with status 2; no window opens. |
| `-h`, `--help` / `-V`, `--version` | Prints the usage or `fileflux <version>` and exits with status 0. |
| an unknown option or a second path | Prints the error plus `Try 'fileflux --help' for more information.` and exits with status 2. |

Use `--` before a directory whose name starts with `-`. Pointing the explorer at a scratch directory (`fileflux /tmp/fixture`) keeps manual checks away from your real files.

## What the MVP does

| Area | Behavior |
|---|---|
| Navigation | Sidebar places (Home, Desktop, Documents, Downloads, Music, Pictures, Videos per your freedesktop config), scoped **Home Trash**, and mounted disk partitions/USB storage from the mount table — one entry per device, without virtual or duplicate bind mounts; the root filesystem reads "System". Names only (full paths in tooltips); each volume shows a thin usage meter and "X free of Y" from `statvfs`. Mounting, unmounting and ejecting are not performed. |
| Path bar | A breadcrumb bar (Home icon and "Home" under `$HOME`, else `/`) with chevron separators; each segment navigates, the current folder reads strong, long paths collapse their leading segments instead of overflowing. Clicking the bar's empty space or Ctrl+L switches to the editable field: Enter navigates absolute, relative, or `~/…` paths — no shell evaluation. Escape, a click elsewhere or leaving the field restores the committed path and returns to the breadcrumbs. Invalid or missing paths report an error and keep the current listing, history, and selection. |
| History | Back/forward/up with disabled states (Alt+Left/Right/Up). Failed loads change nothing. |
| Views | A folder subbar under the toolbar shows the current folder and its "4 folders · 7 files" count. A segmented grid/list toggle switches presentation. Icon grid of fixed-size tiles (equal-size rows, left-aligned partial rows — the last row never stretches) with two-tone folders, image thumbnails and a size subline (folders show "N items"), and a details list drawn by the pinned shared `gpui_component::table::DataTable` (`Name/Size/Modified`, no row-number gutter, local/relative dates, right-aligned sizes; folders show "N items", counted in the background after the listing appears): click a header to sort (the arrow draws in the tint color, folders stay first), columns spread over the pane edge to edge with drag-to-resize honored. Sizes use one decimal SI formatter everywhere. The view toggle keeps folder, query and selection. |
| Search | Ctrl+F filters file names in the current folder only — case-insensitive, nonrecursive, with a live "n of m" count; no matches is distinct from an empty folder; Escape clears. |
| Selection | Click selects, Ctrl+click toggles, Shift+click selects a range in the displayed order, Shift+Arrow and Shift+Home/End extend it from the anchor, Escape clears; arrow keys and Home/End move through the visible entries in the order the list is sorted and scroll them into view; a filtered-out selection stays selected. The table's internal selection is off, so pointer and keyboard selection stay one source of truth. |
| Open | Double click or Enter navigates into folders and opens files, symlinks and broken links with the default application (`xdg-open <path>`, detached, no shell; a failure to start it is reported in the status bar). Entries in the Trash are not opened: restore them first. |
| Details panel | A 300 px read-only panel on the right of the document island, toggled by the toolbar panel button or Ctrl+I (open by default; hidden automatically when the window is too narrow to leave the entries 520 px after the island's own padding). Nothing selected: the folder glyph, name, item total ("N of M" while a filter is active) and full location. One entry: a preview (image files draw the image, contain-fit; others the folder/file glyph), name, kind ("Image · JPG"), size, local modified time, permissions (`rw-r--r--`, read off the UI thread with one `stat`, at most one probe at a time and debounced; `unavailable` in muted text when it fails), the item count for folders and path (Trash entries show their original location). Several: the count and the total size of the selected files. The panel is read-only; the actions live in the context menu and the keyboard. |
| Status bar | Left: "N items" or "N selected · <total size of the selected files>", plus loading, error and status notices, the rename editor's refusal reason, and a compact indicator ("Moving 20000 items to Trash · 9655 of 20000 items") while an operation that takes more than ~400 ms runs. Right: the free space of the volume that holds the current folder (longest mount-path prefix) with a small success-colored square. Shortcuts live in the toolbar tooltips. |
| Live refresh | The current folder is watched (inotify); creates, deletes, moves and edits made elsewhere show up within about a second, keeping the selection of entries that still exist, the filter and the scroll position. If the folder itself is deleted, the explorer falls back to the nearest existing parent with a notice. |
| Favorites | Session-only: the toolbar star adds/removes the current directory. Nothing is written to configuration; the Trash cannot be favorited. |
| Loading | Directory scans, Trash reads and places discovery run off the UI thread with a generation guard: stale completions (including a newer failed request's) never overwrite newer results. |

Non-UTF8 file names keep their exact raw bytes for navigation; lossy text appears only in display strings.

## File operations

Everything that changes the disk runs off the UI thread, reports its result (toast, or an inline error) and never overwrites anything silently.

| Action | How |
|---|---|
| Context menu | Right click an entry or the empty space; also the toolbar "more" button (…), the Menu key and Shift+F10. Arrow keys, Enter and Escape drive it. On an entry: Open (Enter), Rename (F2, one entry), Copy path (Ctrl+Shift+C), Properties (toggles the details panel, Ctrl+I), Move to Trash (Del), Delete permanently… (Shift+Del). On empty space: New folder (Ctrl+Shift+N), New file, Open terminal here (`$TERMINAL`, else `xdg-terminal-exec`; omitted when neither exists), Copy folder path. "Open with…" is not offered (it needs an application chooser). |
| New folder / New file | The subbar's **New folder** chip, Ctrl+Shift+N or the menu create "Untitled folder" / "Untitled file" (numbered when taken) and start the inline rename on it. |
| Rename | F2 edits the name in place in the grid and the list; the stem (up to the last dot) is preselected, folders select the whole name. Enter commits, Escape cancels, leaving the field commits. A refused name (empty, contains `/`, already exists) stays in the field with the reason under it (and in the status bar); leaving the field with a refused name cancels the rename with a message. |
| Move to Trash | Del moves each selected entry to the freedesktop Trash (home Trash on the home device, else `$topdir/.Trash-$uid` of its mount) and offers **Undo** in the toast. If an item has no usable Trash a dialog asks whether to delete it permanently instead; nothing is deleted without that answer. |
| Delete permanently | Shift+Del opens a confirmation naming the item or the count, with **Cancel** as the default button (Enter alone never deletes) and the danger button "Delete permanently". Folders are removed recursively; symlinks are never followed. |
| Trash view | Restore puts items back (on a name conflict both are kept: the restored one gets "name (2)" and the toast names it). Delete permanently… and the **Empty Trash** button each ask first. The view refreshes after every operation. |
| Undo | Ctrl+Z (or the toast's Undo button) undoes the last rename, Move to Trash or New folder/file (a new, still-empty item goes to the Trash, never straight to deletion). The history is per session and survives navigation. An entry that changed since — renamed, replaced, no longer empty, emptied from the Trash — is reported as "Can't undo: …" and nothing is touched. Permanent deletion cannot be undone. |
| Progress | An operation running longer than ~400 ms shows a non-modal card bottom-right (title, bar, "N of M items", current item, **Cancel**; several stack) and a status-bar indicator, and ends in a summary toast ("Cancelled: moved 12441 items to Trash" keeps its Undo). Cancelling stops before the next item; what was done stays done. The card (`explorer/progress.rs`) is reusable by byte-based operations. |

Safety defaults: Del never deletes — it moves to the Trash; permanent deletion only comes from Shift+Del, the Trash view or an unavailable Trash, always behind a confirmation; renames and creations never replace an existing entry; operations act only on the entries visible under the current filter; Trash write support covers the home Trash and `$topdir/.Trash-$uid` (not the shared `$topdir/.Trash`).

## Checks

| Command | What it does |
|---|---|
| `cargo check --all-targets` | Type-checks everything |
| `cargo test` | Runs the model and state tests |
| `cargo fmt --all -- --check` | Formats the sources |
| `cargo clippy --all-targets -- -D warnings` | Lints with the family lint set |
| `cargo build --locked` | Builds the binary |

Keyboard: Ctrl+L path · Ctrl+F search · Alt+←/→/↑ history · Ctrl+R reload · Ctrl+I details panel · Enter open(selected) · F2 rename · Del move to Trash · Shift+Del delete permanently · Ctrl+Shift+N new folder · Ctrl+Z undo · Ctrl+Shift+C copy path · Menu / Shift+F10 context menu · Escape cancel/clear. Escape while the path field or the search field is focused restores the committed path or clears the query and returns the keyboard to the folder contents; Escape elsewhere clears the selection.

## Native smoke helper

`nix develop --command python3 scripts/native-smoke.py` (X11/XWayland, stdlib + `libXtst` only) launches the local build against a synthetic HOME/XDG fixture, drives its own window via XTest, and asserts the title retention around path edits and failed submits. It reports what it cannot automate honestly as `MANUAL` (exit 3) and never claims a visual check it did not perform.

## Current scope

No copy/cut/paste or drag & drop yet, no tabs; the details panel previews image files only. The window title shows the current folder plus Fileflux; the empty top gutter matches the 8 px side gutters (no brand text or icon). Modification dates are displayed in UTC without a timezone database. The sidebar toggle hides or shows the sidebar at any window width, and narrow windows park it as an overlay; path, search and view controls always stay reachable. The app always starts in Bolt's dark palette, whatever the operating system's appearance; there is no theme toggle.

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
