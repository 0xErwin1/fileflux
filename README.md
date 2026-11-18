# Fileflux

A read-only file explorer in the Bolt family, built with GPUI and Bolt — the design system shared with [DBFlux](https://github.com/0xErwin1/dbflux) and [ReqFlux](https://github.com/0xErwin1/reqflux). This is the agreed MVP (S2–S5): real read-only navigation, honest states, no destructive operations, no write access anywhere.

## Quick path

Prerequisites: Nix with flakes enabled, direnv installed, and the direnv hook enabled in your shell.

1. Review the project's `.envrc` (`use flake`), then run `direnv allow` once to authorize it locally.
2. Enter the project directory; direnv loads the development shell from `flake.nix` (Rust toolchain, mold, native libraries).
3. `cargo run` — build and launch the explorer at your home directory.
4. Confirm the window opens titled *Home — Fileflux*, with the places sidebar on the left and the folder grid in the middle.

Review and authorize `.envrc` again after changing it. For shells without direnv integration, `nix develop` remains a manual fallback: `nix develop --command cargo run`.

## What the MVP does

| Area | Behavior |
|---|---|
| Navigation | Sidebar places (Home, Desktop, Documents, Downloads, Music, Pictures, Videos per your freedesktop config), scoped **Home Trash**, and mounted disk partitions/USB storage from the mount table — one entry per device, without virtual or duplicate bind mounts. Paths are exact, with no invented free-space numbers; mounting, unmounting and ejecting are not performed. |
| Path field | Always editable (Ctrl+L); Enter navigates absolute, relative, or `~/…` paths — no shell evaluation. Escape restores the committed path. Invalid or missing paths report an error and keep the current listing, history, and selection. |
| History | Back/forward/up with disabled states (Alt+Left/Right/Up). Failed loads change nothing. |
| Views | Icon grid of fixed-size tiles (104 px wide, equal-size rows, left-aligned partial rows — the last row never stretches) and a details list drawn by Bolt's shared data grid (`Name/Type/Size/Modified` with the family's row-number gutter, sideways scrolling and drag-to-resize); view toggle keeps folder, query and selection. |
| Search | Ctrl+F filters file names in the current folder only — case-insensitive, nonrecursive, with a live "n of m" count; no matches is distinct from an empty folder; Escape clears. |
| Selection | Click selects, Ctrl+click toggles, Escape clears; arrow keys and Home/End move through visible entries and scroll them into view; a filtered-out selection stays selected. |
| Open | Double click or Enter navigates into folders. Files, symlinks, and broken links are never opened and answer honestly in the status bar that Fileflux is read-only. |
| Favorites | Session-only: the toolbar star adds/removes the current directory. Nothing is written to configuration; the Trash cannot be favorited. |
| Loading | Directory scans, Trash reads and places discovery run off the UI thread with a generation guard: stale completions (including a newer failed request's) never overwrite newer results. |

The Trash is the freedesktop home Trash only, labeled as such, read-only — no restore, delete, or empty. Non-UTF8 file names keep their exact raw bytes for navigation; lossy text appears only in display strings.

## Checks

| Command | What it does |
|---|---|
| `cargo check --all-targets` | Type-checks everything |
| `cargo test` | Runs the model and state tests |
| `cargo fmt --all -- --check` | Formats the sources |
| `cargo clippy --all-targets -- -D warnings` | Lints with the family lint set |
| `cargo build --locked` | Builds the binary |

Keyboard: Ctrl+L path · Ctrl+F search · Alt+←/→/↑ history · Ctrl+R reload · Enter open(selected) · Escape cancel/clear. Escape while the path field or the search field is focused restores the committed path or clears the query and returns the keyboard to the folder contents; Escape elsewhere clears the selection.

## Native smoke helper

`nix develop --command python3 scripts/native-smoke.py` (X11/XWayland, stdlib + `libXtst` only) launches the local build against a synthetic HOME/XDG fixture, drives its own window via XTest, and asserts the title retention around path edits and failed submits. It reports what it cannot automate honestly as `MANUAL` (exit 3) and never claims a visual check it did not perform.

## Current scope

Read-only by design (S3): no destructive operations, no copy/move, no tabs, no previews. The window title shows the current folder plus Fileflux; the empty top gutter matches the 8 px side gutters (no brand text or icon). Modification dates are displayed in UTC without a timezone database. The sidebar toggle hides or shows the sidebar at any window width, and narrow windows park it as an overlay; path, search and view controls always stay reachable.

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
