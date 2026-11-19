# Changelog

All notable changes to Fileflux are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project intends to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once it has a first release. The crate is currently at `0.1.0-dev.0`; nothing has been released yet.

## [Unreleased]

### Added

- Native Bolt file explorer on GPUI: places sidebar, breadcrumb path bar with an editable field, back/forward/up history, grid and list views, folder subbar, status bar with free space, and a dark theme.
- Command line: open a folder or file's parent, open images in the viewer, `--help` and `--version`, clear errors with exit status 2 for missing paths.
- Image viewer in a window of its own: folder browsing, zoom, pan, rotation, info panel, fullscreen, animated GIF and WebP, and `scripts/install-desktop.sh` to register Fileflux as an image handler.
- Open images from the explorer in the viewer, in the explorer's sort and filter order.
- Details panel with an image preview, kind, size, modified time and permissions.
- Live refresh of every tab's folder through one shared inotify watcher thread.
- Tabs with per-tab location, history, selection, view, sort and filter; background tabs stay current; open in new tab by middle click or from menus.
- Sort (name, size, modified, type, natural order, folders first) and filters (type, hidden files, modified, size, name search), in the subbar and the context menu.
- File operations: new folder and file, inline rename, Move to Trash and restore with Undo, permanent delete behind a confirmation, Empty Trash, Copy path, Open with…, Open terminal here, Properties.
- Copy, cut and paste with byte-based progress, cancel, and a conflict dialog (Skip, Keep both, Replace); drag and drop to move, copy or trash.
- Identity-checked undo for rename, trash, create, copy and move.
- Context menus that name their target, a sidebar menu, and resizable sidebar and details panel.
- Settings and session persistence: favorites, preferences and open tabs saved atomically to `settings.toml` and `session.toml`, tolerant of broken files, symlinks and several instances.
- Devices: mount, unmount and eject from the sidebar through `udisksctl`, with busy and draining guards.
- Apache-2.0 license, user documentation under `docs/`, `CONTRIBUTING.md`, and issue and pull request templates.

### Changed

- The explorer started as a read-only browser and now supports file operations built on safety defaults: Trash first, no silent overwrite, partial-then-rename copies.
- Image thumbnails, two-tone folders and a full-width details table replace the first plain listing.
- The viewer rotates decoded pixels with the `image` crate.
- Navigation shows only mounted partitions and USB storage, one entry per device.

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

### Performance

- Directory listings are indexed once and sort and filter results are cached.
- Large folders are arranged on the background executor, so typing and clicking stay responsive on very large folders.
- Folder item counts and permissions are read off the UI thread.
