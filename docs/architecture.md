# Architecture

How Fileflux is organized, how work is spread across threads, and the safety invariants that file operations keep. Read this before changing `src/core/ops.rs`, `src/core/trash.rs` or the explorer's operation code.

## Quick path

1. `src/core/` is GPUI-free logic, exported as the library `fileflux::core`. It is unit-tested without a window.
2. `src/explorer/` and `src/viewer/` are the GUI, compiled into the `fileflux` binary only.
3. Anything that touches the disk runs off the UI thread and goes through `core::ops` or `core::trash`.

## Crate layout

One package, one library and one binary.

| Part | Path | Role |
|---|---|---|
| Library `fileflux` | `src/lib.rs` | Exposes `fileflux::cli`, `fileflux::core` and `fileflux::dbus`. No GPUI dependency in these modules |
| D-Bus | `src/dbus.rs` | Single instance (`io.github.fileflux.Fileflux`) and `org.freedesktop.FileManager1`, served on zbus's own thread; requests reach the UI thread through a channel that `src/explorer/desktop.rs` drains |
| Binary `fileflux` | `src/main.rs`, `src/gui.rs`, `src/theme.rs` | Parses the command line through `fileflux::cli`, starts GPUI, installs Bolt's dark theme, opens the explorer or the viewer inside `gpui_component`'s `Root` |
| Explorer | `src/explorer/` | The window: state, views, sidebar, toolbar, tabs, dialogs, progress, key bindings |
| Viewer | `src/viewer/` | The image viewer window: navigation, zoom, rotation, info panel |
| Integration tests | `tests/` | `cli.rs`, `settings.rs`, `bookmarks.rs`, `devices_real_location.rs`, `open_with_quoting.rs` |
| Benchmark | `examples/read_dir_bench.rs` | Directory-scan timing helper |

### `fileflux::core`

| Module | Responsibility |
|---|---|
| `fs` | Read-only directory scanning: one level, metadata only, partial failures reported |
| `navigation` | In-memory history over locations; a pure state machine |
| `places` | Sidebar places, freedesktop user dirs, mounted volumes, gvfs mounts, usage |
| `devices` | `lsblk -J` discovery and the `udisksctl` mount, unmount and eject plans |
| `ops` | Create, rename, copy, move: never overwrite silently, never follow symlinks for copy or move, never delete a source before its replacement is verified |
| `trash` | The freedesktop Trash: list, trash, restore, empty |
| `search` | Recursive name search: a breadth-first walk that never follows symlinked folders, stays on the root's device, follows the listing's hidden rule, checks a cancel flag before every entry, hands matches over in batches and stops at a cap |
| `watch` | inotify through `rustix`: the folder `WatchHub` and the device `DeviceWatcher` |
| `settings` | `settings.toml` and `session.toml`: tolerant loading, version migration, atomic saving, merging |
| `bookmarks` | The GTK bookmarks file as the favorites: parsing, labels, order-preserving atomic edits |
| `admin` | "Open as Administrator": the `pkexec` command lines (policy installed or the `env DISPLAY=… XAUTHORITY=…` fallback), the environment allowlist, the refusals, the menu offer and the single-instance claim decision; pure, pkexec is never run by a test |
| `terminal` | "Open in terminal": the xdg-terminal-exec lookup and the command per terminal (no shell) |
| `apps` | "Open with…": `.desktop` discovery and `Exec=` parsing (no shell) |
| `links` | Symbolic links: "Link to" naming, create-then-retry for a free name (never replaces), absolute targets |
| `archive` | Compress and extract through `bsdtar` (no shell): suffix rules, member and symlink safety checks, temp-then-rename, cancel by killing the child |
| `du` | The size of one folder: allocated blocks, each `(device, inode)` once, no symlink follow, one filesystem, an incomplete flag, a cancel flag and streamed running totals |
| `usage` | The disk usage analyzer's model: one walk that measures every folder under a root (the `du` accounting rules, hard links once across the whole analysis), ranks children, finds a folder of the finished analysis for instant drill-down, and subtracts a removed item from its ancestors as an estimate; at most 200 files kept per folder, the rest folded into a bucket |
| `usage_chart` | The sunburst geometry, pure: rings by depth, angle by share of the parent, thin slices folded into "Other", polar hit testing |
| `images`, `kinds` | Which files are images; the file categories used by sort and filter |

### Explorer modules

| Group | Modules |
|---|---|
| State and arrangement | `state`, `entries`, `arrange`, `arrange_menu` |
| Operations | `actions`, `ops`, `paste`, `clipboard`, `dnd`, `progress` |
| Disk usage | `usage` (the session on a tab: ranking rows, drill-down reuse, the estimate after a cleanup, menus, the walk), `usage_view` (the chart and the list) |
| Chrome | `view`, `toolbar`, `tabstrip`, `tabs`, `panes` (the two tab lists, focus, split layout and F5/Shift+F5 planning), `sidebar`, `table`, `inspector`, `permissions` (the details panel's permission and group editor), `overlay` |
| Devices and persistence | `devices`, `persist`, `favorites` (the bookmarks watch), `toasts` (timeouts, stack limit, placement) |

## Threading model

The UI thread only draws and handles input. Everything that can block runs elsewhere, and every result returns tagged so a stale one can be ignored.

| Work | Where it runs | Guard |
|---|---|---|
| Directory scans, Trash reads, places discovery | GPUI's background executor | A generation counter: a stale completion never overwrites a newer result |
| Recursive search | A walk on the background executor, collected by the UI a few times a second | A cancel flag (`AtomicBool`) the walk checks before every entry, set when the search is replaced, stopped, or its tab navigates away or closes; a generation counter refuses the batches of a walk that is no longer the tab's |
| Disk usage analysis | A walk of `core::usage` on the background executor, collected by the UI about every 150 ms | A cancel flag the walk checks before every entry, set when the tab leaves the view, a new walk replaces it, or Stop is pressed; the session ignores a walk that is no longer the tab's |
| Arranging large folders (sort, filter, search) | Background executor jobs | The listing is indexed once. The previous order stays on screen and drives selection until the latest request completes; the latest change wins. Small folders are arranged in place |
| Live refresh of folders | One shared watch thread (`WatchHub`): a dedicated thread blocked in `poll` on one inotify instance, not a worker of the executor | One watch per distinct folder however many tabs show it, released when the last tab leaves. Watches are identified by the kernel's watch descriptor, and replaced folders are re-watched |
| Device refresh | One `DeviceWatcher` thread (`/dev`, `/dev/disk/by-id`, `/dev/bus/usb`, `/proc/self/mountinfo`, gvfs) | 300 ms of quiet before a re-read; at most one re-read in flight; a slow poll only when a watch cannot be established |
| File operations (create, rename, trash, copy, move, delete, restore, undo) | Off the UI thread (background work started by the operation code) | Cancellable between items; progress reported to the UI; the UI re-reads the affected folders on completion |
| `udisksctl`, `lsblk`, `gio` | Off the UI thread, no shell | 30 s timeout |
| `statvfs` on gvfs mounts | A throwaway thread | 2 s limit: a locked phone can stall the kernel call |
| Settings and session saves | Off the UI thread | See below |

**Persistence.** The explorer takes a snapshot of its persisted state on every pump tick and hands it to the persistence layer, which waits for 500 ms without change, then returns a save job to run off the UI thread. Saves are numbered so a slow one never overwrites what the window close wrote after it, and closing the window flushes what is pending. Files are written to a temporary file, synced and renamed (see [Settings](settings.md#atomic-writes)).

**Navigation beats refresh.** A navigation always wins over the refresh of a finished operation, so a late completion never pulls you back.

## Safety invariants

These hold for every file operation. A change that breaks one needs a new design discussion, not a patch.

| Invariant | How it is kept |
|---|---|
| **Trash first** | Del moves to the Trash. Permanent deletion needs Shift+Del, the Trash view, or an unavailable Trash, behind a confirmation whose default is Cancel and that a background result cannot replace. Danger buttons ignore the pointer for about 400 ms after their dialog appears |
| **No silent overwrite** | Creating, renaming and pasting never replace an existing entry without an explicit **Replace**. Replace moves the old item aside, deletes it only after the new one is in place, and puts it back if that fails |
| **`renameat2` with `RENAME_NOREPLACE`** | Moves and finishing a copy use `renameat2(RENAME_NOREPLACE)`, so a name that appears meanwhile is never clobbered. Where the filesystem or kernel lacks it (`EINVAL`, `ENOTSUP`, `ENOSYS`: FUSE, gvfs), a pre-check plus a plain rename is used |
| **Partial, then rename** | A copy is built under a hidden temporary name and renamed into place, so a cancelled or failed copy never leaves a half file under the real name. Temporary names are hidden from listings. A move across devices copies, verifies, and only then removes the source |
| **Identity-checked undo** | Each undo entry records the device and inode of what it acted on (and, for the Trash, the item's `.trashinfo` origin). Undo acts only if the item at that place is still the same one; otherwise it reports "Can't undo: … changed since …" and touches nothing. Undo of a copy trashes, never deletes |
| **Symlinks are never followed** | Copies recreate links; deletes remove the link; the busy-device check resolves real locations instead |
| **Visible only** | Operations act only on entries visible under the current filter |
| **Dialogs are stable** | Each danger button acts on exactly the paths its dialog listed; dialogs from background results queue behind the open one |
| **Devices are guarded** | Unmount and eject are refused or delayed while an operation uses the device (see [Devices](devices.md#safety-rules)) |
| **No shell** | `xdg-open`, `udisksctl`, `lsblk`, `gio`, terminal and "Open with…" programs are started with explicit arguments. `Exec=` lines that would let a file name run as a shell command are not listed |
| **Safe settings** | A broken or newer file is backed up or left alone, never half-written |

Names are bounded to 255 bytes: invented names ("name (2).ext") shorten the stem on a character boundary, and Trash names whose `.trashinfo` would not fit are stored under a shortened name with the exact original path recorded.

## Elevation

Two features use `pkexec` and neither ever asks for or stores a password: the owner change (`core::elevate`, the small `fileflux-owner-helper`) and **Open as Administrator** (`core::admin`).

**Open as Administrator** starts a *separate* Fileflux process as root; the user's instance never gains privileges. It runs `pkexec <fileflux> --new-instance -- <folder>` when the polkit action `dev.fileflux.open-as-admin` is installed with `exec.path` equal to that launcher (the Nix `.fileflux-wrapped` name is mapped back to the `fileflux` wrapper) and `exec.allow_gui=true`, which lets pkexec keep `DISPLAY` and `XAUTHORITY`. Otherwise it runs `pkexec env DISPLAY=… XAUTHORITY=… <fileflux> --new-instance -- <folder>`, with only those two variables and only when set. No shell is involved, the folder is one argument after `--`, and `DBUS_SESSION_BUS_ADDRESS`, `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR` are never passed, so the root instance runs on X11/XWayland, keeps its settings under root's own home (pkexec sets `HOME=/root`) and cannot reach the user's session bus.

The root instance never joins the single-instance protocol: `--new-instance` and any process whose effective uid is 0 skip `dbus::claim`, so it neither forwards its folder to the user's window nor takes the user's D-Bus name. While `geteuid()` is 0 every explorer window shows a fixed red banner (Bolt's `danger` colour) and every window title, explorer and image viewer, ends with " (root)". The menu row is hidden for root and refused for the Trash, Recent, search, disk usage and for FUSE and gvfs paths. A dismissed or missing pkexec leaves only a notice.

## Pinned stack

The GPUI stack follows DBFlux, as ReqFlux does, so all three apps build against one GPUI and one Bolt. `Cargo.toml` pins `gpui-pre` (0.3.9), `gpui-base`, `gpui-component` (0.7.3), `gpui-pre-linux` and `bolt` to one DBFlux git revision through `[patch.crates-io]`: DBFlux's patched GPUI (frame-observer hooks), its patched editor build and its Wayland keymap fix. Move that revision together with DBFlux. `flake.lock` pins the Nix inputs to the revisions ReqFlux ships. The Rust toolchain is pinned in `rust-toolchain.toml` and installed by the Nix shell. 

## Design

The implemented look follows [`design/file-explorer/01-familiar-grid.html`](../design/file-explorer/01-familiar-grid.html) and [`design/system.md`](../design/system.md): a desk with 8 px gutters (the empty top gutter has no brand text or icon), a 290 px sidebar, opposing 14 px island chamfers, square selection rows with a 2 px tint bar, and Bolt's palette and font roles. The `02` and `03` artboards remain alternatives.

## Related

[Development](development.md) · [Contributing](../CONTRIBUTING.md)
