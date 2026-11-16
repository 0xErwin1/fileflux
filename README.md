# Fileflux

A file explorer in the Bolt family, built with GPUI and Bolt — the design system shared with [DBFlux](https://github.com/0xErwin1/dbflux) and [ReqFlux](https://github.com/0xErwin1/reqflux). This repository is at the project-setup stage: it compiles, launches a themed native window, and nothing more yet.

## Quick path

Prerequisites: Nix with flakes enabled, direnv installed, and the direnv hook enabled in your shell.

1. Review the project's `.envrc` (`use flake`), then run `direnv allow` once to authorize it locally.
2. Enter the project directory; direnv loads the development shell from `flake.nix` (Rust toolchain, mold, native libraries).
3. `cargo run` — build and launch the setup window.
4. Confirm the window opens titled *Fileflux*, with a themed sidebar and "Fileflux is ready".

Review and authorize `.envrc` again after changing it. The `.direnv/` cache stays local and is ignored by Git. No global Rust installation is required. For shells without direnv integration, `nix develop` remains a manual fallback.

## Checks

| Command | What it does |
|---|---|
| `cargo check` | Type-checks the crate |
| `cargo test` | Runs the tests (none yet) |
| `cargo fmt` | Formats the sources |
| `cargo clippy` | Lints with the family lint set |

## Current scope

The window is an honest stand-in, not a file explorer: a themed sidebar and a content area that say the app is ready. There is no navigation, no filesystem access, and no controls that pretend to work. File browsing comes next.

## Design

The intended look lives in [`design/`](design/) — a design-only translation of the Bolt system for Fileflux, with the file-explorer screens. It is not an implementation; the runtime look comes from Bolt itself.

## Pinned stack

The GPUI stack follows DBFlux, exactly as ReqFlux does, so all three apps build against one GPUI and one design system:

| Package | Pin | Why |
|---|---|---|
| `gpui-pre` 0.3.9 | git rev `bf3cbd0` | DBFlux's patched GPUI (frame-observer hooks) |
| `gpui-component` 0.7.3 | git rev `bf3cbd0` | DBFlux's patched editor build |
| `gpui-pre-linux` 0.3.9 | git rev `bf3cbd0` | DBFlux's Wayland keymap fix |
| `bolt` | git rev `bf3cbd0` | The family design system |

The rev is DBFlux's main where Bolt was merged; move it together with DBFlux. The Rust toolchain is pinned at 1.99.1 in [`rust-toolchain.toml`](rust-toolchain.toml) and installed by the Nix shell — never globally. `flake.lock` pins Nix inputs to the same revisions ReqFlux ships, so the shell contents are proven.
