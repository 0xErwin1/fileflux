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
# - The sandbox root is private to the user: ${XDG_RUNTIME_DIR:-/tmp}/fileflux-ui-sandbox-$UID,
#   mode 0700, refused when it is a symlink or owned by someone else (a shared
#   /tmp path could otherwise be planted). Recorded pids are only signalled
#   when the process still looks like what this script started.
#
# The MCP server must run with DISPLAY set to the same display to take
# screenshots: `DISPLAY=:97 gpui-mcp --app-id fileflux`.
set -euo pipefail

display="${FILEFLUX_UI_DISPLAY:-:97}"
root="${XDG_RUNTIME_DIR:-/tmp}/fileflux-ui-sandbox-$UID"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
binary="$repo/target/debug/fileflux"

# The root must be a real directory this user owns, with no group or other access.
check_root() {
    [[ -e "$root" || -L "$root" ]] || return 0
    if [[ -L "$root" || ! -d "$root" || "$(stat -c %u "$root")" != "$UID" ]]; then
        echo "refusing $root: it must be a directory owned by uid $UID, not a symlink" >&2
        exit 1
    fi
    chmod 0700 "$root"
}

make_root() {
    check_root
    [[ -d "$root" ]] || mkdir -m 0700 -- "$root"
    check_root
}

# What each recorded process must look like on its command line.
expected_command() {
    case "$1" in
        xvfb) echo "Xvfb" ;;
        wm) echo "openbox" ;;
        app) echo "dbus-run-session" ;;
    esac
}

# A pid file is live when the pid exists and is still the process started for
# that part; anything else is stale and is dropped without signalling it.
running() {
    local file="$root/$1.pid" pid cmd
    [[ -f "$file" ]] || return 1
    pid="$(cat "$file")"
    if [[ "$pid" =~ ^[0-9]+$ ]] && cmd="$({ tr '\0' ' ' <"/proc/$pid/cmdline"; } 2>/dev/null)" \
        && [[ "$cmd" == *"$(expected_command "$1")"* ]]; then
        return 0
    fi
    rm -f "$file"
    return 1
}

stop_one() {
    if running "$1"; then
        local pid
        pid="$(cat "$root/$1.pid")"
        kill -- "-$pid" 2>/dev/null || kill "$pid" 2>/dev/null || true
    fi
    rm -f "$root/$1.pid"
}

start() {
    local folder="${1:-$root/home/demo}"
    cargo build --manifest-path "$repo/Cargo.toml" --features ui-automation
    make_root
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
    stop) check_root; stop_one app; stop_one wm; stop_one xvfb; echo "sandbox stopped" ;;
    status)
        check_root
        for part in xvfb wm app; do
            running "$part" && echo "$part: running" || echo "$part: stopped"
        done ;;
    *) echo "usage: $0 start [folder] | stop | status" >&2; exit 2 ;;
esac
