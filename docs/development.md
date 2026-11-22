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

The crate builds two binaries: `fileflux` (the app) and `fileflux-owner-helper` (`src/bin/fileflux-owner-helper.rs`), the small root program the details panel's owner change runs through `pkexec`. It uses only `std` and `rustix` (it includes `src/core/owner_helper.rs` by path and does not link the GUI), and `cargo run` starts `fileflux` (`default-run`). Install it in the same directory as `fileflux`: Fileflux looks for it only there (never in `PATH`) and otherwise falls back to `pkexec chown`. Ship `packaging/dev.fileflux.owner-helper.policy` into `share/polkit-1/actions`, with its `org.freedesktop.policykit.exec.path` set to the helper's installed path (the Nix package substitutes its store path). Ship `packaging/dev.fileflux.open-as-admin.policy` the same way, with `exec.path` set to the installed `fileflux` launcher (for the Nix source package that is the wrapper `$out/bin/fileflux`, for `nix/binary.nix` `$out/bin/<appId>`): it lets "Open as Administrator" run `pkexec <fileflux> --new-instance -- <folder>` with `DISPLAY` and `XAUTHORITY` kept (`exec.allow_gui`); without it Fileflux falls back to `pkexec env DISPLAY=… XAUTHORITY=…`. The tests never run pkexec, the helper as root or through pkexec: they exercise its pure parts and run the binary as the current user to check that it refuses without root.

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

## Driving a running window from an agent

`cargo build --features ui-automation` links the [gpui-mcp](https://github.com/themixednuts/gpui-mcp) bridge into the window, the same instrumentation DBFlux and ReqFlux carry. The feature is off by default: a release build compiles neither the bridge nor its dependencies, and `src/ui_automation.rs` keeps the gate so every window calls `install` unconditionally.

With the feature on, an MCP client started as the same user finds the window, reads the element tree it rendered and injects input:

```sh
cargo build --features ui-automation
./target/debug/fileflux ~/some/folder &
gpui-mcp --app-id fileflux          # the stdio MCP server, from the DBFlux tree
```

For an agent, prefer the isolated sandbox over the desktop session:

```sh
scripts/ui-sandbox.sh start [folder]   # build with the feature, launch on Xvfb :97
DISPLAY=:97 gpui-mcp --app-id fileflux  # screenshots need the same DISPLAY
scripts/ui-sandbox.sh stop
```

The script runs Xvfb with openbox (native capture finds no window without a window manager), a private D-Bus session (on the real bus the single-instance check hands the folder to an already running Fileflux and exits), and a synthetic HOME and XDG config/data/state/cache. `XDG_RUNTIME_DIR` stays real because the discovery descriptors live there. The sandbox root is `${XDG_RUNTIME_DIR:-/tmp}/fileflux-ui-sandbox-$UID`, created with mode 0700 and refused when it is a symlink or owned by another user; a recorded pid is only signalled when its command line still matches what the script started (Xvfb's display argument, the build's binary, and the sandbox `HOME`, not just a program name). `stop` and `status` also look in the old root `${TMPDIR:-/tmp}/fileflux-ui-sandbox`, so a sandbox started before the move is still stopped. `FILEFLUX_UI_DISPLAY` overrides the display.

Use it to check what a screenshot cannot: which element a click actually lands on, where a drag preview sits relative to the pointer, what the status bar renders for a given selection. The bridge reads frames in-process, so it works on Wayland; the server's native **screenshots** are X11-only, so on Wayland capture the window another way. Prefer this over XTest-driven checks above when the question is about the element tree rather than pixels, and keep both when the question is visual.

### Measuring a big folder

To see what a folder with thousands of images and videos costs the UI thread, enter it in the sandbox and read the frames: `mark_frames`, enter the folder through the path field (Ctrl+L, the path, Enter), wait, then `get_frame_report` (`app_draw_ms`, `prepaint_ms`, `views_reused`). Use a fresh `XDG_CACHE_HOME` for a cold thumbnail cache and keep it for a warm one, and measure a `--release` build as well as the debug one: a debug GPUI frame costs about ten times more, and that is what a developer running `cargo run` feels. The bridge's `ping` is answered off the UI thread, so it says nothing about a busy window; time a foreground timer against its deadline instead. Frame cost does not depend on the folder (a 3-entry folder draws as slowly as a 1100-entry one, the grid being virtualized), so what matters is how many frames the folder causes: thumbnails are published together every 150 ms and redraw the window only when one on screen changed.

## Nix packages

`flake.nix` outputs, for `x86_64-linux` and `aarch64-linux` (the prebuilt nightly only for `x86_64-linux`):

| Output | What it is |
|---|---|
| `packages.fileflux` (also `default`, `fileflux-source`) | The source build with `rustPlatform.buildRustPackage`, the desktop entry, `bin/fileflux-owner-helper` and the polkit actions for it and for Open as Administrator in `share/polkit-1/actions` |
| `packages.fileflux-nightly` | The prebuilt binary of the rolling `nightly` release (`nix/binary.nix`, pinned by `nix/nightly-info.nix`), installed as `fileflux-nightly` |
| `apps.fileflux`, `apps.fileflux-nightly` | `nix run` entry points for the two packages |
| `overlays.default` | `pkgs.fileflux`, `pkgs.fileflux-source`, `pkgs.fileflux-nightly`, built from the consumer's package set |
| `devShells.default` | The shell described above |

The hashes in `nix/nightly-info.nix` are placeholders on `main`; the nightly workflow rewrites them on the `nightly` ref, so consume the prebuilt package as `github:0xErwin1/fileflux/nightly#fileflux-nightly`.

## Scripts and assets

| Path | Purpose |
|---|---|
| `scripts/install-desktop.sh` | Install or remove the desktop entry, optionally as default image viewer and file manager (`--default-images`, `--default-folders`, `--uninstall`). Writes only to `$XDG_DATA_HOME/applications` and `$XDG_CONFIG_HOME/mimeapps.list` |
| `scripts/native-smoke.py` | Native smoke check described above |
| `scripts/ui-sandbox.sh` | Isolated `ui-automation` session for the gpui-mcp server (`start [folder]`, `stop`, `status`), described above |
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
