#!/usr/bin/env bash
# Run a ui-automation build of Fileflux in an isolated X session that the
# gpui-mcp server can read, click and screenshot.
#
#   scripts/ui-sandbox.sh start [folder]   build, then launch on the sandbox display
#   scripts/ui-sandbox.sh stop             stop the app, the window manager and Xvfb
#   scripts/ui-sandbox.sh status           print the sandbox state
#
# Isolation, and why each piece exists:
# - Xvfb on FILEFLUX_UI_DISPLAY (default :97), WAYLAND_DISPLAY unset: the window
#   never opens on the real desktop.
# - openbox: native screenshots enumerate windows through the window manager;
#   on a bare Xvfb the capture finds no window.
# - dbus-run-session: a private session bus. On the real bus the single-instance
#   check hands the folder to an already running Fileflux and exits.
# - Synthetic HOME and XDG config/data/state/cache: no real settings or
#   history are read or rewritten. XDG_RUNTIME_DIR stays real, because the
#   gpui-mcp discovery descriptors live there.
#
# The MCP server must run with DISPLAY set to the same display to take
# screenshots: `DISPLAY=:97 gpui-mcp --app-id fileflux`.
set -euo pipefail

display="${FILEFLUX_UI_DISPLAY:-:97}"
root="${TMPDIR:-/tmp}/fileflux-ui-sandbox"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
binary="$repo/target/debug/fileflux"

running() { [[ -f "$root/$1.pid" ]] && kill -0 "$(cat "$root/$1.pid")" 2>/dev/null; }

stop_one() {
    if running "$1"; then
        kill -- "-$(cat "$root/$1.pid")" 2>/dev/null || kill "$(cat "$root/$1.pid")" 2>/dev/null || true
    fi
    rm -f "$root/$1.pid"
}

start() {
    local folder="${1:-$root/home/demo}"
    cargo build --manifest-path "$repo/Cargo.toml" --features ui-automation
    mkdir -p "$root"/{home/demo/folder-a,home/demo/folder-b,config,data,state,cache}
    [[ -e "$root/home/demo/notes.txt" ]] || echo "sandbox" >"$root/home/demo/notes.txt"

    if ! running xvfb; then
        setsid Xvfb "$display" -screen 0 1400x900x24 >"$root/xvfb.log" 2>&1 &
        echo $! >"$root/xvfb.pid"
        sleep 1
    fi
    if ! running wm; then
        local wm=(openbox)
        command -v openbox >/dev/null || wm=(nix shell nixpkgs#openbox -c openbox)
        env -u WAYLAND_DISPLAY -u DBUS_SESSION_BUS_ADDRESS DISPLAY="$display" HOME="$root/home" \
            setsid "${wm[@]}" >"$root/wm.log" 2>&1 &
        echo $! >"$root/wm.pid"
        sleep 3
    fi
    stop_one app
    env -u WAYLAND_DISPLAY -u DBUS_SESSION_BUS_ADDRESS \
        DISPLAY="$display" HOME="$root/home" \
        XDG_CONFIG_HOME="$root/config" XDG_DATA_HOME="$root/data" \
        XDG_STATE_HOME="$root/state" XDG_CACHE_HOME="$root/cache" \
        RUST_LOG="${RUST_LOG:-fileflux=info,gpui_mcp=info}" \
        setsid dbus-run-session -- "$binary" "$folder" >"$root/app.log" 2>&1 &
    echo $! >"$root/app.pid"

    for _ in $(seq 1 20); do
        if grep -q "UI automation bridge listening" "$root/app.log" 2>/dev/null; then
            echo "Fileflux running on $display, sandbox $root"
            return 0
        fi
        running app || break
        sleep 0.5
    done
    echo "Fileflux did not start its automation bridge; see $root/app.log" >&2
    return 1
}

case "${1:-}" in
    start) shift; start "$@" ;;
    stop) stop_one app; stop_one wm; stop_one xvfb; echo "sandbox stopped" ;;
    status)
        for part in xvfb wm app; do
            running "$part" && echo "$part: running" || echo "$part: stopped"
        done ;;
    *) echo "usage: $0 start [folder] | stop | status" >&2; exit 2 ;;
esac
