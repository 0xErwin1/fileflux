# Development

Build, test and run Fileflux locally. The Nix shell provides the pinned Rust toolchain, `mold` and the native libraries, so nothing is installed globally.

## Quick path

Prerequisites: Nix with flakes enabled, direnv, and the direnv hook in your shell.

1. Create your own `.envrc` in the project root containing `use flake`. It is not tracked: each developer creates and owns theirs.
2. Run `direnv allow` once to authorize it. Enter the directory and direnv loads the dev shell from `flake.nix`.
3. `cargo run -- /tmp/fixture` launches the explorer on a scratch folder.
4. Before you push, run the [checks](#checks).

Without direnv, use the shell on demand: `nix develop --command cargo run`.

## Checks

All four must pass before a change is ready; CI runs the same ones.

| Command | Purpose |
|---|---|
| `cargo fmt --all -- --check` | Formatting |
| `cargo clippy --all-targets --locked -- -D warnings` | Lints, with the project lint set from `Cargo.toml` |
| `cargo test --locked` | Unit and integration tests |
| `cargo build --release --locked` | Release build |

`cargo check --all-targets` is the quick type-check while iterating.

CI runs fmt, clippy `-D warnings`, tests and a release build; a nightly build publishes binaries. GUI checks are not part of CI.

## Run against a scratch folder

Never test file operations on your real data. Point Fileflux at a throwaway directory:

```sh
mkdir -p /tmp/fixture/{docs,pics}
touch /tmp/fixture/docs/a.txt /tmp/fixture/docs/b.txt
cargo run -- /tmp/fixture
```

A path on the command line opens only that folder and does not touch your saved tabs. Your preferences are still read and written, and the favorites are the GTK bookmarks, so use a throwaway config when you need isolation:

```sh
export XDG_CONFIG_HOME=$(mktemp -d) XDG_STATE_HOME=$(mktemp -d)
cargo run -- /tmp/fixture
```

Set `RUST_LOG=debug` for logs (`env_logger`).

## Native GUI checks

Automated tests cover the model and state. Checks that need a real window run under a **private Xvfb**, never your real display, so synthetic input cannot land in your desktop session.

```sh
Xvfb :99 -screen 0 1280x800x24 &
export DISPLAY=:99
unset WAYLAND_DISPLAY
nix develop --command python3 scripts/native-smoke.py
```

Rules:

- Pin `DISPLAY` to the Xvfb server and **unset `WAYLAND_DISPLAY`** before launching anything. Without that, GPUI may open on your real Wayland session.
- Use a free display number (`:99` is an example) and stop the server when you are done.
- Use a synthetic `HOME` and XDG directories; never real user data.

`scripts/native-smoke.py` (X11 or XWayland, standard library plus `libXtst`) launches the local build against a synthetic HOME/XDG fixture, drives its own window with XTest, and asserts the window title around path edits and failed submits. What it cannot automate it reports as `MANUAL` (exit 3); it never claims a visual check it did not perform.

## Scripts and assets

| Path | Purpose |
|---|---|
| `scripts/install-desktop.sh` | Install or remove the desktop entry, optionally as default image viewer (`--default-images`, `--uninstall`). Writes only to `$XDG_DATA_HOME/applications` and `$XDG_CONFIG_HOME/mimeapps.list` |
| `scripts/native-smoke.py` | Native smoke check described above |
| `packaging/fileflux.desktop` | The desktop entry the install script uses |
| `examples/read_dir_bench.rs` | Directory-scan benchmark: `cargo run --release --example read_dir_bench [entries]` (creates and removes its own million-file fixture under the system temp dir) |
| `design/` | Design reference: `system.md` (tokens and rules, with `system.html`) and `file-explorer/` HTML mockups (`01-familiar-grid` is the implemented look; `02`/`03` are alternatives; `04-refined-shell` shows the refined shell) |

The mockups are static HTML; open them in a browser.

## Project layout

See [Architecture](architecture.md#crate-layout). In short: `src/core/` for GPUI-free logic with unit tests, `src/explorer/` and `src/viewer/` for the windows, `tests/` for integration tests.

## Tips

- Put logic in `fileflux::core` where it can be tested without a window; keep GPUI glue thin.
- Lints deny `unsafe_code` and warn on `unwrap`, `expect`, `panic` and indexing. Handle errors with `Result`.
- The linker is `mold` (set in `.cargo/config.toml`), provided by the Nix shell. Outside Nix, install it or override the rustflags.
- Fontconfig is loaded on demand (`RUST_FONTCONFIG_DLOPEN=1`), so the build does not need `fontconfig.pc`.
- The GPUI stack is pinned to a DBFlux revision; do not bump a single crate on its own (see [Architecture](architecture.md#pinned-stack)).

## Next step

Read [CONTRIBUTING.md](../CONTRIBUTING.md) for commit conventions and the safety rules for file operations.
