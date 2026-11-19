# Keyboard shortcuts

Every key the explorer and the image viewer bind. Shortcuts also appear in toolbar tooltips and context menus.

## Explorer

| Key | Action | Notes |
|---|---|---|
| Ctrl+T | New tab | On the current folder, to the right of the active tab |
| Ctrl+W | Close tab | The last tab is never closed |
| Ctrl+Tab / Ctrl+PageDown | Next tab | Wraps around |
| Ctrl+Shift+Tab / Ctrl+PageUp | Previous tab | Wraps around |
| Alt+1 … Alt+9 | Jump to tab 1–9 | Nothing happens when there is no such tab |
| Ctrl+L | Edit the path | Enter navigates; Escape restores |
| Ctrl+F | Search file names | Current folder only; Escape clears |
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
| Enter | Open | Folders navigate; images open the viewer; other files open with the default application |
| F2 | Rename | One entry, in place |
| Del | Move to Trash | Offers Undo |
| Shift+Del | Delete permanently | Asks first; Cancel is the default |
| Ctrl+Shift+N | New folder | Starts an inline rename |
| Ctrl+C / Ctrl+X / Ctrl+V | Copy / Cut / Paste | |
| Ctrl+Z | Undo the last operation | |
| Ctrl+Shift+C | Copy path | |
| Menu / Shift+F10 | Open the context menu | |
| Escape | Cancel or clear | See below |

**Escape** does the nearest useful thing: it restores the path field or clears the search and returns the keyboard to the entries; it cancels an inline rename, an open menu, a dialog or a drag; elsewhere it clears the selection.

Inside a context menu, ↑ ↓ move, Enter confirms and Escape closes. Inside a dialog, ↑ ↓ move the focus and Escape cancels.

While a text field has the keyboard, the file operations (Ctrl+A, F2, Del, Ctrl+C/X/V/Z and the rest) are left to the field; the tab shortcuts still work. Behind a dialog or an open menu, none of these shortcuts act.

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
| Drop with Shift | Move (Ctrl wins when both are held) |
