# Keyboard shortcuts

Every key the explorer and the image viewer bind. Shortcuts also appear in toolbar tooltips and context menus.

## Explorer

| Key | Action | Notes |
|---|---|---|
| Ctrl+T | New tab | On the current folder, to the right of the active tab; tab shortcuts act on the focused pane |
| Ctrl+W | Close tab | The last tab is never closed |
| Ctrl+Tab / Ctrl+PageDown | Next tab | Wraps around |
| Ctrl+Shift+Tab / Ctrl+PageUp | Previous tab | Wraps around |
| Alt+1 … Alt+9 | Jump to tab 1–9 | Nothing happens when there is no such tab |
| F3 | Split into two panes / back to one | Also the toolbar's Split view button; see [Dual pane](user-guide.md#dual-pane) |
| F6 | Switch to the other pane | Only while split |
| F5 | Copy the selection to the other pane's folder | Conflict dialog, progress and Undo as for paste; refused into Trash, Recent and search results |
| Shift+F5 | Move the selection to the other pane's folder | Same |
| Ctrl+L | Edit the path | Enter navigates; Escape restores |
| Ctrl+F | Search file names | Current folder only; Escape clears |
| Ctrl+Shift+F | Search in subfolders | Turns on *Search in subfolders* and focuses the search field; see [Search in subfolders](user-guide.md#search-in-subfolders) |
| Alt+← / Alt+→ | Back / Forward | |
| Alt+↑ | Up one folder | |
| Ctrl+R | Reload | |
| Ctrl+I | Toggle the details panel | Same as Properties |
| Ctrl+H | Show or hide hidden files | Dotfiles and the names in a folder's `.hidden` file. Hidden by default; showing them adds a "Hidden shown" chip |
| ↑ ↓ ← → | Move the selection | In the order the list is sorted |
| Home / End | First / last entry | |
| Shift+↑ ↓ ← → | Extend the selection | From the anchor |
| Shift+Home / Shift+End | Extend to the first / last entry | |
| Ctrl+A | Select all visible entries | Respects the filter |
| Type a name | Jump to the first visible entry starting with it | Case-insensitive, in the displayed order; the file view must have the keyboard (not a text field). The typed prefix resets after about a second, on Escape, on navigation, or when the selection changes by mouse or arrow keys. Repeating one letter (`a`, `a`, …) cycles through the entries starting with it |
| Enter | Open | Folders navigate; images open the viewer; other files open with the default application |
| F2 | Rename | One entry, in place |
| Del | Move to Trash | Offers Undo |
| Shift+Del | Delete permanently | Asks first; Cancel is the default |
| Ctrl+Shift+N | New folder | Starts an inline rename |
| Ctrl+C / Ctrl+X / Ctrl+V | Copy / Cut / Paste | Paste also takes files another application put on the system clipboard as text (`file://` URIs or absolute paths); see the user guide's Clipboard section |
| Ctrl+Z | Undo the last operation | |
| Ctrl+Shift+C | Copy path | |
| Menu / Shift+F10 | Open the context menu | |
| Escape | Cancel or clear | See below |

**Escape** does the nearest useful thing: it restores the path field or clears the search and returns the keyboard to the entries (in a subfolder search it first stops the search that is still running and keeps what it found; pressed again it leaves the search); it cancels an inline rename, an open menu, a dialog or a drag; elsewhere it clears the selection.

Inside a context menu, ↑ ↓ move (wrapping at the ends), Enter confirms, → opens a submenu (Enter does too), ← closes the open submenu, and Escape closes the open submenu first and the menu next. Enter on a sort or filter choice applies it and keeps the menu open. Inside a dialog, ↑ ↓ move the focus and Escape cancels.

While a text field has the keyboard, the file operations (Ctrl+A, F2, Del, Ctrl+C/X/V/Z and the rest) are left to the field; the tab shortcuts, F3 and F6 still work (F5 and Shift+F5 are left to the field). Behind a dialog or an open menu, none of these shortcuts act.

## Image viewer

| Key | Action |
|---|---|
| ← / PageUp | Previous image |
| → / PageDown | Next image |
| Home / End | First / last image |
| `+`, `=`, Ctrl+`=` | Zoom in |
| `-`, Ctrl+`-` | Zoom out |
| `0` / Ctrl+`0` | Fit to window |
| `1` / Ctrl+`1` | Show at 100% |
| `R` | Rotate 90° clockwise |
| Shift+`R` | Rotate 90° counter-clockwise |
| `I` | Toggle the info panel |
| `F` / F11 | Toggle fullscreen |
| Esc / `Q` | Close the window |

Mouse: the wheel and the back/forward buttons navigate, Ctrl+wheel zooms around the pointer, double-click toggles fit and 100%, and dragging pans.

## Mouse and drag modifiers

| Gesture | Action |
|---|---|
| Click / Ctrl+click / Shift+click | Select / toggle / select a range |
| Middle-click a folder, sidebar row or breadcrumb | Open in a background tab |
| Middle-click a tab | Close it |
| Drop with Ctrl | Copy |
| Drop with Shift | Move |
| Drop with Ctrl+Shift | Link: creates symbolic links to the dropped entries in the target folder (see [Links](user-guide.md#links)) |
