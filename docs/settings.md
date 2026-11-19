# Settings and files

Fileflux remembers your preferences, favorites and open tabs in two small TOML files. There is no settings screen: changes are saved on their own.

## Quick path

1. Change something (view, sort, a favorite, a panel width, open a tab).
2. Wait about half a second; it is saved off the UI thread, and once more when the window closes.
3. To start over, delete the file (see [Resetting](#resetting)).

## Files and locations

| File | Location | Holds |
|---|---|---|
| `settings.toml` | `$XDG_CONFIG_HOME/fileflux/` (default `~/.config/fileflux/`) | Preferences: default view (grid or list), default sort and filters (sort, `hide_hidden`, folders first, natural order, categories, date and size), sidebar and details panel widths (`sidebar_width`, `details_width`; a file without `details_width` loads the default), whether the sidebar starts collapsed, whether the details panel is open, and the window size |
| `session.toml` | `$XDG_STATE_HOME/fileflux/` (default `~/.local/state/fileflux/`) | The open tabs (folder or Trash, view, sort and filters of each) and which one is active |

Both files are created with mode `0600` because they list your folders.

Favorites are **not** in `settings.toml`: they are the lines of the GTK bookmarks file, `$XDG_CONFIG_HOME/gtk-3.0/bookmarks` (default `~/.config/gtk-3.0/bookmarks`), shared with Nautilus, Thunar and the GTK file chooser. See [Favorites and GTK bookmarks](user-guide.md#favorites-and-gtk-bookmarks).

## Defaults and migration

The defaults are **Modified, descending** (folders first, natural order) with **hidden files hidden** (dotfiles and the names in a folder's `.hidden` file). Both files are `version = 2`.

A version 1 file is migrated when it is read, and rewritten as version 2 at the next save:

- A saved sort of *Name, ascending* (the old default) becomes *Modified, descending*, and a saved `hide_hidden = false` (the old default) becomes `true`. This applies to the preferences in `settings.toml` and to each tab in `session.toml`.
- Any other saved choice is kept: Name descending, Size, Type, a filter, hidden files shown by a tab you changed, and so on. A key the file omits takes the new default.
- The version 1 `favorites` list is moved into the GTK bookmarks once: the missing ones are appended in order, then `settings.toml` is saved without them. If the bookmarks cannot be written, the favorites stay in `settings.toml` and the move is retried at the next launch.
- A file written by a newer version is never migrated or overwritten.

## What is saved and when

- **The default view, sort and filters are the active tab's.** A new tab already starts from the tab it was opened from, so whatever the active tab shows is saved as the default for the next launch. Per-folder memory is not implemented.
- **Restoring.** Starting `fileflux` without a path reopens the saved tabs, in order, with the saved one active. Startup never checks the folders on the UI thread: each tab opens as saved and loads in the background. A tab whose folder no longer exists then switches to its nearest existing parent (home if none), with a status-bar notice.
- **A path on the command line wins.** `fileflux ~/x` opens only that folder and neither reads nor overwrites the saved session, so a quick launch never replaces the tabs of your last regular run. Preferences are still loaded and saved.
- **Autosave.** A change is written once it has stayed unchanged for about 500 ms, so a drag or a burst of clicks is one write.

## Safe to break

| Situation | What Fileflux does |
|---|---|
| Unknown keys | Ignored. Keys you add by hand are dropped the next time Fileflux writes the file |
| Missing keys | Take their defaults |
| File cannot be parsed | Loads the defaults, shows a status-bar notice, and first copies the file beside it as `settings.toml.bad-<UTC timestamp>` (same for `session.toml`). The next autosave writes a valid file, so the notice shows once. If the copy cannot be made, the file is left exactly as it is and Fileflux does not write it |
| File written by a newer Fileflux | Loads the defaults with a notice and never overwrites it |
| A save fails | One notice, never blocks the app. The change stays pending, the background retries after 30 seconds, and closing the window tries once more |

Every file carries a `version`.

## Atomic writes

Each file is written to a uniquely named temporary file (`<name>.<pid>.<n>.tmp`, created exclusively and never through a symlink) in the same directory, synced, and renamed over the old one, so a crash never leaves half a file. Saves are numbered: a slow background save never overwrites what the window close wrote after it.

## Symlinks

If `settings.toml` or `session.toml` is a symlink (for example from a dotfile manager), Fileflux writes through to the file it points at and the symlink stays. If that target cannot be written (a read-only store), the save fails with the usual one-time notice and nothing is replaced.

## Multiple instances

Each instance saves on its own. Before writing `settings.toml`, Fileflux re-reads it:

- Every setting is this instance's (the last writer wins).
- Favorites are not part of this file, so they are not merged: every instance shares the GTK bookmarks file, each change re-reads it before writing, and an edit made elsewhere shows in this window live.
- The session is not merged: the tabs of the last instance to save are the ones restored.
- If the file was meanwhile changed to something invalid or to a newer version, it is left untouched and the save reports a notice.

## Paths are lossless

A UTF-8 path is stored as a plain string; any other path is stored as `{ bytes = [...] }`, so folders with non-UTF-8 names round-trip.

## Resetting

Delete `settings.toml`, `session.toml`, or both; the next launch starts from the defaults.

```sh
rm ~/.config/fileflux/settings.toml ~/.local/state/fileflux/session.toml
```

Adjust the paths if you set `XDG_CONFIG_HOME` or `XDG_STATE_HOME`.

## Related

Desktop integration (`scripts/install-desktop.sh`) is separate: it writes only to `$XDG_DATA_HOME/applications` and `$XDG_CONFIG_HOME/mimeapps.list`. See the [user guide](user-guide.md#making-fileflux-your-default-image-viewer).
