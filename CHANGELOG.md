# Changelog

All notable changes to Fileflux are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project intends to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once it has a first release. The crate is currently at `0.1.0-dev.0`; nothing has been released yet.

## [Unreleased]

### Changed

- Dragging entries now needs the pointer to move 8 px from the press (the desktop's drag threshold, GPUI's own is 2 px). Before that the drag is inert: no preview, no drop-target highlight, no drop and no selection change, and releasing there acts as the click it was, so a slight slip while clicking can no longer move a file. The rubber band uses the same threshold to tell a drag from a click on empty space.

### Added

- Rubber-band selection: drag on the empty space of the folder pane, in the grid or the list and in both panes, to select the entries a rectangle covers, live. Ctrl toggles and Shift adds against the selection from before the drag, the pane scrolls while the pointer rests near or beyond the top or bottom edge, Escape cancels and restores the earlier selection, and a plain click on empty space still clears the selection. The band starts from any empty point of the pane, including the margins and the blank part of a tile around its icon and name; only the drawn icon, name or list row is the entry. Hit testing is layout arithmetic, so it stays fast on huge folders.
- Drag files out to other applications: dragging the selection past the window's edge hands it to the desktop as a file list (`text/uri-list`), so it can be dropped on a browser, chat or another file manager. Wayland only for now; on X11 the drag stays inside Fileflux.
- Cascading submenus in context menus, on Bolt's cascade (DBFlux `7ceb70b`): the empty-space menu's **Sort by** and **Filter** open beside the menu instead of replacing it, with hover (after 150 ms), →/Enter, ←/Escape, a flip to the left at the window's right edge, and live check marks that keep the menu open. The subbar Sort and Filter buttons open the same options. The drill-in pages and their "Back" rows are gone.
- Dual pane: F3 or the toolbar's Split view button shows two folders side by side. Each pane has its own tabs, location, history, view, sort, filters and selection; the focused pane has a tinted top edge and the sidebar, breadcrumbs, path field, search and keyboard act on it. F6 switches panes, and clicking a pane focuses it. F5 copies and Shift+F5 moves the selection to the folder the other pane shows, through the same paste engine as Ctrl+V (conflict dialog, progress card, Undo), and refuses with a toast into the Trash, Recent and search results. Drag and drop works between panes, live refresh covers both, and the session remembers the split and both panes' tabs (a `session.toml` without them loads as one pane). The split needs a window at least 1100 px wide; a narrower window shows one pane at a time, and the details panel is not drawn while split.
- Missing image thumbnails are generated into the freedesktop cache (256 px, EXIF-upright, two at a time).
- Open with: Other application with a filter, and Remember for <type> writing mimeapps.list.

- Search in subfolders: a toggle by the search box and Ctrl+Shift+F search file names in the folder and everything under it. Results stream into a read-only view that shows each match's folder; the walk is breadth-first, off the UI thread, cancellable with Escape, never follows symlinked folders, stays on one filesystem, follows the hidden-files rule, and stops at 10,000 matches ("N found · searching…", "stopped at 10,000"). Enter, Open containing folder and the file operations work on results. A search tab is not saved in the session.
- Recent view in Places from the freedesktop `recently-used.xbel` (read-only, live, Open containing folder, restorable tab).
- Grid thumbnails reuse current entries from the freedesktop thumbnail cache.

- Native Bolt file explorer on GPUI: places sidebar, breadcrumb path bar with an editable field, back/forward/up history, grid and list views, folder subbar, status bar with free space, and a dark theme.
- Command line: open a folder or file's parent, open images in the viewer, `--help` and `--version`, clear errors with exit status 2 for missing paths.
- Image viewer in a window of its own: folder browsing, zoom, pan, rotation, info panel, fullscreen, animated GIF and WebP, and `scripts/install-desktop.sh` to register Fileflux as an image handler.
- Open images from the explorer in the viewer, in the explorer's sort and filter order.
- Details panel with an image preview, kind, size, modified time and permissions.
- Live refresh of every tab's folder through one shared inotify watcher thread.
- Tabs with per-tab location, history, selection, view, sort and filter; background tabs stay current; open in new tab by middle click or from menus.
- Sort (name, size, modified, type, natural order, folders first) and filters (type, hidden files, modified, size, name search), in the subbar and the context menu.
- File operations: new folder and file, inline rename, Move to Trash and restore with Undo, permanent delete behind a confirmation, Empty Trash, Copy path, Open with…, Open in terminal, Properties.
- Copy, cut and paste with byte-based progress, cancel, and a conflict dialog (Skip, Keep both, Replace); drag and drop to move, copy or trash.
- Identity-checked undo for rename, trash, create, copy and move.
- Context menus that name their target, a sidebar menu, and resizable sidebar and details panel.
- Settings and session persistence: preferences and open tabs saved atomically to `settings.toml` and `session.toml`, tolerant of broken files, symlinks and several instances.
- Devices: mount, unmount and eject from the sidebar through `udisksctl`, with busy and draining guards.
- Favorites are your GTK bookmarks (`~/.config/gtk-3.0/bookmarks`), shared with Nautilus and Thunar: edits are atomic, keep every other line, label and comment, and show live in the sidebar. Favorites an older `settings.toml` held are moved there once.
- Hidden files follow the `.hidden` file convention (names listed one per line, as in Nautilus and Thunar) as well as dotfiles.
- Open in terminal on a folder, a sidebar row and the empty space, resolved like xdg-terminal-exec (`$TERMINAL`, `xdg-terminal-exec`, `xdg-terminals.list`, desktop entries, known terminals), with no shell.
- Phones (MTP), network shares and other gvfs mounts are listed under Devices with friendly names, and the device list refreshes from events (inotify and mount table) instead of polling.
- Continuous integration (fmt, clippy, tests, release build), a nightly workflow that publishes a prebuilt binary to a rolling `nightly` release, and flake packages (`fileflux`, `fileflux-source`, `fileflux-nightly`), apps and an overlay.
- Apache-2.0 license, user documentation under `docs/`, `CONTRIBUTING.md`, and issue and pull request templates.

### Changed

- The narrow-window sidebar overlay now spans the area above the status bar like the docked sidebar, instead of ending short with a clipped bottom row and a corner artifact.
- Folders are sorted by Modified, newest first, and hidden files are hidden by default (Ctrl+H shows them, with a "Hidden shown" chip). Settings and session files move to `version = 2`: a saved *Name, ascending* sort and `hide_hidden = false` (the old defaults) are migrated, any other saved choice is kept.
- Toasts stack in the bottom-right corner and close sooner: 3 s for info and success, 6 s with Undo, 8 s for errors and warnings, at most 3 at once.
- The explorer started as a read-only browser and now supports file operations built on safety defaults: Trash first, no silent overwrite, partial-then-rename copies.
- Image thumbnails, two-tone folders and a full-width details table replace the first plain listing.
- The viewer rotates decoded pixels with the `image` crate.
- Devices lists mounted volumes, one entry per device, and unmounted removable filesystems.
- CI runs with read-only permissions; the nightly build waits for CI to succeed on the exact commit, builds with `--locked`, publishes before pruning, and keeps the 14 newest per-build assets by upload time. Third-party actions are pinned by commit.

### Fixed

- Failed or stale submits in the path field no longer change the committed folder, and the fields stay in sync after navigation.
- Command-line paths containing `..` are folded, and the current directory is needed only for relative paths.
- File operation dialogs and undo hardened against stale or background results.
- Temporary transfer files are hidden from folder listings.
- "Open with…" refuses `Exec=` entries that quote a field code, so a file name cannot run as a shell command.
- Navigation wins over the refresh of a finished operation, and affected folders are de-duplicated.
- Watched folders are identified by their kernel watch descriptor; replaced folders are re-watched and lost watch removals recovered.
- Settings saves hardened against crashes, concurrent instances and symlinked files.
- Operations on a device being ejected are blocked, and symlinks are resolved in device checks.
- The Empty Trash action empties only the items the confirmation counted.
- Listing a folder never blocks on a FIFO or device node named `.hidden`.
- Every navigation is gated while a device is being ejected, and refused undos stay available during an eject.
- Bookmark edits made by other programs are kept when Fileflux writes the file, the favorites migration finishes cleanly, and relative legacy favorites are skipped.
- Only an invalid saved sort is reset on load, not a valid one.
- Copies on FUSE filesystems (gvfs, sshfs) tolerate `EINVAL` from the no-replace rename, and a direct copy is cleaned up by file identity if it fails or is cancelled.

### Performance

- Directory listings are indexed once and sort and filter results are cached.
- Large folders are arranged on the background executor, so typing and clicking stay responsive on very large folders.
- Folder item counts and permissions are read off the UI thread.
