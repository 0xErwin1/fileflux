# Settings and files

Fileflux remembers your preferences, favorites and open tabs in two small TOML files. There is no settings screen: changes are saved on their own.

## Quick path

1. Change something (view, sort, a favorite, a panel width, open a tab).
2. Wait about half a second; it is saved off the UI thread, and once more when the window closes.
3. To start over, delete the file (see [Resetting](#resetting)).

## Files and locations

| File | Location | Holds |
|---|---|---|
| `settings.toml` | `$XDG_CONFIG_HOME/fileflux/` (default `~/.config/fileflux/`) | Preferences: default view (`grid`, `list` or `compact`; a file with the older values loads unchanged), default sort and filters (sort, `hide_hidden`, folders first, natural order, categories, date and size), sidebar and details panel widths (`sidebar_width`, `details_width`; a file without `details_width` loads the default), the optional list columns (`list_columns`, see below), the icon size of each view (`grid_zoom`, `list_zoom`, `compact_zoom`, see below), whether the sidebar starts collapsed, whether the details panel is open, and the window size |
| `session.toml` | `$XDG_STATE_HOME/fileflux/` (default `~/.local/state/fileflux/`) | The open tabs (folder, Trash or Recent, view, sort and filters of each) and which one is active, and for the dual-pane view the second pane's tabs, its active tab, whether the view was split and which pane had the focus (a `[split]` table, written only once the view has been split; a file without it opens as a single pane, so older files load unchanged). A tab showing the results of a subfolder search is saved as the folder it searched: the query and results are not restored |

| `folders.toml` | `$XDG_STATE_HOME/fileflux/` (default `~/.local/state/fileflux/`) | The [view remembered for each folder](#per-folder-view-memory): `[[folders]]` entries, least recently used first, each with the absolute `path`, the `view` and the `arrangement` (sort key and direction, folders first, natural order, hidden files, and the type, date and size filters) |

A third, optional file is written by you, never by Fileflux:

| File | Location | Holds |
|---|---|---|
| `actions.toml` | `$XDG_CONFIG_HOME/fileflux/` (default `~/.config/fileflux/`) | Your [custom actions](#custom-actions-actionstoml) |

`settings.toml`, `session.toml` and `folders.toml` are created with mode `0600` because they list your folders.

Favorites are **not** in `settings.toml`: they are the lines of the GTK bookmarks file, `$XDG_CONFIG_HOME/gtk-3.0/bookmarks` (default `~/.config/gtk-3.0/bookmarks`), shared with Nautilus, Thunar and the GTK file chooser. See [Favorites and GTK bookmarks](user-guide.md#favorites-and-gtk-bookmarks).

## Custom actions (`actions.toml`)

User-defined context menu entries, as `[[action]]` tables. Fileflux only reads this file. It is checked whenever a context menu opens and read again when its modification time or size changes, so edits apply without a restart. A missing file offers no actions. A file with any error offers none at all and shows one notice (and logs a warning) naming the error and its line; fix it and open a menu again. Unknown keys are errors, so a typo such as `appear_on` is caught. Fields, placeholders and examples are in the [user guide](user-guide.md#custom-actions).

## Defaults and migration

The defaults are **Modified, descending** (folders first, natural order) with **hidden files hidden** (dotfiles and the names in a folder's `.hidden` file). Both files are `version = 2`.

A version 1 file is migrated when it is read, and rewritten as version 2 at the next save:

- A saved sort of *Name, ascending* (the old default) becomes *Modified, descending*, and a saved `hide_hidden = false` (the old default) becomes `true`. This applies to the preferences in `settings.toml` and to each tab in `session.toml`.
- Any other saved choice is kept: Name descending, Size, Type, a filter, hidden files shown by a tab you changed, and so on. A key the file omits takes the new default.
- The version 1 `favorites` list is moved into the GTK bookmarks once: the missing ones are appended in order, then `settings.toml` is saved without them. If the bookmarks cannot be written, the favorites stay in `settings.toml` and the move is retried at the next launch.
- A file written by a newer version is never migrated or overwritten.

## List columns

`list_columns` (under `[preferences]`) lists the optional columns of the list view besides Name, which is always shown. The values are `size`, `modified`, `type`, `permissions`, `owner` and `group`:

```toml
[preferences]
list_columns = ["size", "modified", "type", "owner"]
```

A file without the key (every file written before it existed, in version 1 or 2) shows today's three columns, Name, Size and Modified, so the version stays 2 and an older build can still read the file. `list_columns = []` shows Name alone. The columns are always drawn in the order above, whatever order the file lists them in, and repeats are dropped. An unknown name makes the file invalid like any other wrongly typed value: it is kept aside and the defaults load. Sorting by Type, Permissions, Owner or Group is saved with the rest of the sort as `sort_key = "type"`, `"permissions"`, `"owner"` or `"group"`.

## Icon size

`grid_zoom`, `list_zoom` and `compact_zoom` (under `[preferences]`) hold the icon size of each view, set with Ctrl+=, Ctrl+-, Ctrl+0 or Ctrl+wheel (see [Icon size](user-guide.md#icon-size)). The grid takes `"small"`, `"medium"`, `"large"` or `"larger"`; the list and the compact view take `"small"`, `"medium"` or `"large"`:

```toml
[preferences]
grid_zoom = "large"
list_zoom = "medium"
compact_zoom = "small"
```

A file without the keys (every file written before they existed, in version 1 or 2) draws medium, the size of before, so the version stays 2 and an older build can still read the file. A name the view has no level for, such as `list_zoom = "larger"`, makes the file invalid like any other wrongly typed value: it is kept aside and the defaults load.

## What is saved and when

`session.toml` records a Recent tab as `recent = true`, like `trash = true` for the Trash.

A saved tab whose folder is inside the Trash reopens as the Trash view: folders in the Trash are never opened as normal folders.


- **The default view, sort and filters are the active tab's.** A new tab already starts from the tab it was opened from, so whatever the active tab shows is saved as the default for the next launch. A folder whose view you changed is the exception: it shows its own (see below).
- **Restoring.** Starting `fileflux` without a path reopens the saved tabs, in order, with the saved one active, and the split view with both panes' tabs when it was split (in a window narrower than 1100 px it shows one pane at a time). Startup never checks the folders on the UI thread: each tab opens as saved and loads in the background. A tab whose folder no longer exists then switches to its nearest existing parent (home if none), with a status-bar notice.
- **A path on the command line wins.** `fileflux ~/x` opens only that folder and neither reads nor overwrites the saved session, so a quick launch never replaces the tabs of your last regular run. Preferences are still loaded and saved.
- **Autosave.** A change is written once it has stayed unchanged for about 500 ms, so a drag or a burst of clicks is one write.

## Per-folder view memory

`folders.toml` remembers, for each folder where you changed the view (grid, list or compact), the sort key and direction, *Folders first*, the hidden-files toggle or a type, date or size filter, what that tab showed afterwards. Changing any of them in a folder tab records the folder; the Trash, Recent and search results are never recorded. Going into a folder (opening it, the path bar, Back, Forward, Up, a new tab, a restored session) shows what is remembered for it. A folder without an entry keeps what the tab shows, as before: the last choice carries over to folders you have not customized.

- **Key.** The absolute path as Fileflux shows it; symlinks are not resolved, so a link and its target are remembered separately. A path that is not UTF-8 is stored as `{ bytes = [...] }`.
- **Bounded.** At most 1000 folders. Recording a folder makes it the most recently used; past the limit the least recently used is forgotten. Only a change counts as a use: merely visiting a folder does not reorder the file.
- **Reset.** The empty-space menu of a remembered folder has **Reset View for This Folder**: it deletes the entry and shows the view, sort and filters the settings held at launch.
- **Saving.** Written like `session.toml`: atomically, once the memory has stayed unchanged for about 500 ms, and when the window closes. It is a separate file with its own `version` (currently 1) and, like the others, is moved aside as `folders.toml.bad-<UTC timestamp>` with one notice when it cannot be parsed, and never overwritten when a newer Fileflux wrote it. The last writer wins when two instances run.

## Safe to break

| Situation | What Fileflux does |
|---|---|
| Unknown keys | Ignored. Keys you add by hand are dropped the next time Fileflux writes the file |
| Missing keys | Take their defaults |
| File cannot be parsed | Loads the defaults, shows a status-bar notice, and first copies the file beside it as `settings.toml.bad-<UTC timestamp>` (same for `session.toml` and `folders.toml`). The next autosave writes a valid file, so the notice shows once. If the copy cannot be made, the file is left exactly as it is and Fileflux does not write it |
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

Delete `settings.toml`, `session.toml`, `folders.toml`, or any of them; the next launch starts from the defaults (deleting `folders.toml` forgets every folder's view).

```sh
rm ~/.config/fileflux/settings.toml ~/.local/state/fileflux/session.toml ~/.local/state/fileflux/folders.toml
```

Adjust the paths if you set `XDG_CONFIG_HOME` or `XDG_STATE_HOME`.

## Related

Desktop integration (`scripts/install-desktop.sh`) is separate: it writes only to `$XDG_DATA_HOME/applications` and `$XDG_CONFIG_HOME/mimeapps.list`. See the [user guide](user-guide.md#making-fileflux-your-default-image-viewer).
