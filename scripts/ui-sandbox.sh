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
#   when the process still looks like what this script started: its command
#   line arguments (Xvfb's display, the build's binary) and the sandbox HOME
#   match, not just a program name. stop and status also look under the old
#   root "${TMPDIR:-/tmp}/fileflux-ui-sandbox", so a sandbox started before the
#   move is still found and stopped (same checks, never created or chmod-ed).
#
# The MCP server must run with DISPLAY set to the same display to take
# screenshots: `DISPLAY=:97 gpui-mcp --app-id fileflux`.
set -euo pipefail

display="${FILEFLUX_UI_DISPLAY:-:97}"
root="${XDG_RUNTIME_DIR:-/tmp}/fileflux-ui-sandbox-$UID"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
binary="$repo/target/debug/fileflux"

# The pre-move root, still looked at by stop and status so a sandbox started
# by an older version of this script is not left running unseen.
legacy_root="${TMPDIR:-/tmp}/fileflux-ui-sandbox"

# Checks the private root and tightens its mode to 0700 when it exists; it
# never creates it (make_root does). It mutates the mode, so only
# start/stop/status of the current root call it. The root must be a real
# directory this user owns, never a symlink.
ensure_private_root() {
    [[ -e "$root" || -L "$root" ]] || return 0
    if [[ -L "$root" || ! -d "$root" || "$(stat -c %u "$root")" != "$UID" ]]; then
        echo "refusing $root: it must be a directory owned by uid $UID, not a symlink" >&2
        exit 1
    fi
    chmod 0700 "$root"
}

make_root() {
    ensure_private_root
    [[ -d "$root" ]] || mkdir -m 0700 -- "$root"
    ensure_private_root
}

# Whether a root other than the current one can be trusted for pid files:
# a real directory of this user, not a symlink. Read only, nothing is created
# or changed there.
trusted_root() {
    [[ -d "$1" && ! -L "$1" && "$(stat -c %u "$1")" == "$UID" ]]
}

# Whether the process still looks like the one started for that part under
# that root. The command line is compared argument by argument (a substring
# of the whole line would match any process that merely mentions the name):
# - xvfb: the program is Xvfb and its display argument is this display.
# - wm: one argument is openbox, and the process was started with the
#   sandbox's HOME, so it is this sandbox's window manager.
# - app: the program is dbus-run-session, it runs this build's binary, and
#   its HOME is the sandbox root's.
matches_part() {
    local part="$1" dir="$2" pid="$3" argv=() arg matched=false
    mapfile -d '' -t argv <"/proc/$pid/cmdline" 2>/dev/null || return 1
    [[ ${#argv[@]} -gt 0 ]] || return 1
    case "$part" in
        xvfb)
            [[ "${argv[0]##*/}" == Xvfb && "${argv[1]:-}" == "$display" ]] ;;
        wm)
            for arg in "${argv[@]}"; do
                [[ "${arg##*/}" == openbox ]] && matched=true
            done
            [[ $matched == true ]] && tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null \
                | grep -Fxq "HOME=$dir/home" ;;
        app)
            for arg in "${argv[@]:1}"; do
                [[ "$arg" == "$binary" ]] && matched=true
            done
            [[ "${argv[0]##*/}" == dbus-run-session && $matched == true ]] \
                && tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null \
                | grep -Fxq "HOME=$dir/home" ;;
        *) return 1 ;;
    esac
}

# A pid file under `dir` is live when the pid exists and is still the process
# started for that part; anything else is stale and is dropped without
# signalling it.
running_in() {
    local dir="$1" part="$2" file pid
    file="$dir/$part.pid"
    [[ -f "$file" && ! -L "$file" ]] || return 1
    pid="$(cat "$file")"
    if [[ "$pid" =~ ^[0-9]+$ ]] && matches_part "$part" "$dir" "$pid"; then
        return 0
    fi
    rm -f "$file"
    return 1
}

stop_in() {
    local dir="$1" part="$2" pid
    if running_in "$dir" "$part"; then
        pid="$(cat "$dir/$part.pid")"
        kill -- "-$pid" 2>/dev/null || kill "$pid" 2>/dev/null || true
    fi
    rm -f "$dir/$part.pid"
}

running() { running_in "$root" "$1"; }
stop_one() { stop_in "$root" "$1"; }

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

    # The pid is that of the launching shell until it execs dbus-run-session;
    # sleep before the first identity check so that gap is not read as a stale
    # pid (which would drop the pid file). The bridge can take over 20 s to
    # listen while the private session bus retries desktop portals that cannot
    # reach a compositor, so wait up to 60 s.
    for _ in $(seq 1 120); do
        sleep 0.5
        if grep -q "UI automation bridge listening" "$root/app.log" 2>/dev/null; then
            echo "Fileflux running on $display, sandbox $root"
            return 0
        fi
        running app || break
    done
    echo "Fileflux did not start its automation bridge; see $root/app.log" >&2
    return 1
}

case "${1:-}" in
    start) shift; start "$@" ;;
    stop)
        ensure_private_root
        for part in app wm xvfb; do stop_one "$part"; done
        if trusted_root "$legacy_root"; then
            for part in app wm xvfb; do stop_in "$legacy_root" "$part"; done
        fi
        echo "sandbox stopped" ;;
    status)
        ensure_private_root
        for part in xvfb wm app; do
            running "$part" && echo "$part: running" || echo "$part: stopped"
        done
        if trusted_root "$legacy_root"; then
            for part in xvfb wm app; do
                if running_in "$legacy_root" "$part"; then
                    echo "$part: running (old root $legacy_root)"
                fi
            done
        fi ;;
    *) echo "usage: $0 start [folder] | stop | status" >&2; exit 2 ;;
esac
