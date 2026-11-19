# Contributing to Fileflux

Thanks for helping. Fileflux moves and deletes people's files, so correctness and safety come before speed of delivery.

## Quick path

1. Set up the dev shell (below) and build.
2. Make a small change with its tests and docs.
3. Run the [checks](#checks-that-must-pass); open a pull request with a Conventional Commit title.

## Setup

Prerequisites: Nix with flakes enabled, direnv, and the direnv hook in your shell.

1. Create your own `.envrc` in the project root containing `use flake` (it is not tracked).
2. Run `direnv allow`. direnv loads the dev shell (pinned Rust toolchain, `mold`, native libraries).
3. `cargo run -- /tmp/fixture` runs the app on a scratch folder.

Without direnv: `nix develop --command cargo run`. Details, native GUI checks and scratch-folder practice are in [docs/development.md](docs/development.md).

## Checks that must pass

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

CI runs fmt, clippy `-D warnings`, tests and a release build, and a nightly build publishes binaries. GUI checks are not part of CI: if you changed the UI, say how you checked it (see [native GUI checks](docs/development.md#native-gui-checks)).

## Commits and pull requests

- Use [Conventional Commits](https://www.conventionalcommits.org/): `feat:`, `fix:`, `perf:`, `refactor:`, `docs:`, `test:`, `chore:`, with an optional scope (`fix(trash): …`). Describe the change in the imperative: `fix: keep a navigation over a stale tab refresh`.
- Keep commits small and reviewable: one logical change each, with its tests and docs in the same commit.
- A pull request title follows the same convention. State what to review first and what is out of scope.
- New behavior needs a test that fails without it. A bug fix needs a regression test.
- Update the docs for any visible change: [docs/](docs/README.md), and the `[Unreleased]` section of [CHANGELOG.md](CHANGELOG.md).
- Do not bump the GPUI stack (`gpui-*`, `bolt`) on its own: it is pinned to one DBFlux revision (see [architecture](docs/architecture.md#pinned-stack)).

## Code conventions

- Idiomatic Rust: `Result` and `Option`, enums and pattern matching, ownership instead of cloning workarounds. `unsafe_code` is denied.
- The lints in `Cargo.toml` (`unwrap_used`, `expect_used`, `panic`, `indexing_slicing`, …) apply; clippy must pass with `-D warnings`.
- Put logic in `src/core/` (no GPUI) so it can be unit-tested without a window; keep the explorer and viewer thin.
- Never block the UI thread: filesystem, subprocess and network-adjacent work runs in the background (see [threading model](docs/architecture.md#threading-model)).

## Safety rules for file operations

Changes to anything that creates, renames, copies, moves, trashes, restores or deletes must keep these invariants ([why and how](docs/architecture.md#safety-invariants)):

- **Trash first.** Del moves to the Trash; permanent deletion only behind a confirmation whose default is Cancel.
- **No silent overwrite.** Never replace an existing entry without an explicit user choice. Use `renameat2(RENAME_NOREPLACE)` (with the existing fallbacks) for moves and for finishing copies.
- **Partial, then rename.** Build copies under a hidden temporary name and rename into place; never leave a half file under a real name. Delete a source only after its replacement is verified.
- **Identity-checked undo.** Undo acts only on the item it recorded (device and inode); if it changed, report and touch nothing.
- **Never follow symlinks** for copy, move or delete.
- **No shell.** Start programs with explicit arguments.
- **Tests use scratch directories** (`mktemp -d`, never your real home), and tests clean up after themselves. Never run a manual check of a destructive operation against real data.

## License of contributions

Fileflux is licensed under the [Apache License 2.0](LICENSE). By submitting a contribution you agree that it is licensed under the same terms (inbound = outbound), as described in section 5 of the license. Do not submit code you do not have the right to license that way.

## Reporting problems

Use the issue templates for bugs and feature requests. For a bug, include your Fileflux version, distribution, and whether you run Wayland or X11.
