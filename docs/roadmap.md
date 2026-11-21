# Roadmap

What Fileflux still needs to stand in for a desktop file manager such as PCManFM, plus one feature beyond it: a disk usage analyzer. The list comes from an audit of the documentation and a keyword search of `src/` on 2026-11-20; nothing here was checked by running the application.

## Quick path

1. Start with [Priority 1](#priority-1-daily-use-gaps): the gaps felt in everyday use.
2. Resolve the [open questions](#open-questions) before planning the items that depend on them.
3. Read [Disk usage analyzer](#disk-usage-analyzer) before estimating it: its accounting rules decide the design.

## Where Fileflux already leads

Undo, the dual pane, type/date/size filters, live refresh of every tab, rubber-band selection, the built-in image viewer and the eject safety rules have no PCManFM equivalent. They are not on this list.

## Priority 1: daily-use gaps

| Item | What is missing | Today |
|---|---|---|
| File clipboard interchange | Copy or cut files in Fileflux and paste them in another application. Needs the `x-special/gnome-copied-files` and `text/uri-list` clipboard targets, which the GPUI clipboard (text and images only) cannot offer or read until DBFlux's `gpui-pre-linux` gains custom MIME types | Pasting files other applications copied as `file://` URIs or absolute paths in text works, as a copy (cut only with a leading `cut` line). Other applications still receive only the paths as plain text, and cannot tell copy from cut |
| Change the owner | Change an entry's owner, which needs root or `CAP_CHOWN`: an elevation flow (polkit or a helper), which Fileflux does not have | The details panel edits permissions and group of one or several entries, and shows the owner read-only |

## Priority 2: expected file manager features

| Item | What is missing | Today |
|---|---|---|
| Views | A compact view and an icon size control | Grid and a list with Name, Size, Modified and optional Type, Permissions, Owner and Group columns |
| Search | File content, regular expressions, case sensitivity | Case-insensitive name substring, capped at 10,000 matches |
| Network | A "Connect to server" dialog; network bookmarks in the sidebar | Only mounts gvfs already made are browsed; `sftp://` and `smb://` bookmarks are hidden |
| Encrypted drives | Unlock a LUKS container | Shown as Locked |
| Desktop integration | The `org.freedesktop.FileManager1` D-Bus interface ("Show in folder" from other applications) and a single instance | Nothing |

## Priority 3: refinements

| Item | What is missing | Today |
|---|---|---|
| Pause and resume | Pause a running copy or move | Cancel only |
| Custom actions | User-defined context menu entries | Nothing |
| Per-folder view memory | Remember view, sort and filters for each folder | The active tab's choice is the default for the next launch |
| Tabs | Reorder tabs; drag a tab to another window | Fixed order |
| Open with | Several files at once | One selected file |
| Optical media | List and mount discs | Excluded from Devices |
| Translations | A translated interface | English only |

## Disk usage analyzer

PCManFM has no equivalent: it reports the free space of a volume and the size of a folder in Properties. Fileflux already shows free space in the status bar and a usage meter for each volume in the sidebar. The analyzer adds three things on top of recursive folder size:

1. A ranked view of the heaviest folders and files under a location.
2. Navigation into any folder of that ranking.
3. Cleanup from the ranking through the existing Trash and delete paths, so the confirmation and Undo rules stay the same.

The numbers are only useful under these accounting rules:

| Rule | Reason |
|---|---|
| Count allocated blocks, not the apparent size | Sparse and compressed files report more than they occupy |
| Count each device and inode pair once | Hard links would be counted once per name |
| Never follow symbolic links | A link loop never ends, and a target outside the folder is not its content |
| Stay on one filesystem | A mount below the folder belongs to another volume |
| Walk off the UI thread, with cancel and streamed results | A home folder holds millions of entries |
| State that freed space is an estimate | With reflinks or snapshots (btrfs), deleting a file may free less than its size |

## Out of scope unless decided otherwise

- Desktop management (icons and wallpaper on the root window), which PCManFM offers through `--desktop`.
- Formatting and partitioning drives.

## Open questions

The audit did not settle these:

- [ ] Is there a preferences dialog, or are settings edited only through the TOML files?
- [ ] Is there a way to open a folder with elevated privileges?
- [ ] What happens when opening an executable file or a `.desktop` launcher?
- [ ] Is Fileflux registered as a handler for `inode/directory`?
- [ ] Can favorites be renamed or reordered from the sidebar?
- [ ] Do the mouse back and forward buttons navigate in the explorer?
