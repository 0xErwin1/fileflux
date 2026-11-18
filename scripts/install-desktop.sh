#!/usr/bin/env bash
# Installs (or removes) Fileflux's desktop entry for the current user, so it
# shows up in "Open With" menus and can be the default image viewer.
#
#   scripts/install-desktop.sh                    install the desktop entry
#   scripts/install-desktop.sh --default-images   ...and make Fileflux the default
#                                                 app for the image types it opens
#   scripts/install-desktop.sh --uninstall        undo exactly what was installed
#
# The entry goes to ${XDG_DATA_HOME:-$HOME/.local/share}/applications and the
# defaults to ${XDG_CONFIG_HOME:-$HOME/.config}/mimeapps.list (through
# `xdg-mime default`). The entry runs `fileflux %F`, so `fileflux` must be on
# your PATH. Running it again changes nothing; --uninstall restores the
# defaults that were replaced and leaves every other setting alone.

set -euo pipefail

APP_ID="fileflux.desktop"
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SOURCE="$SELF_DIR/../packaging/$APP_ID"

DATA_HOME="${XDG_DATA_HOME:-${HOME:?HOME or XDG_DATA_HOME must be set}/.local/share}"
CONFIG_HOME="${XDG_CONFIG_HOME:-${HOME:?HOME or XDG_CONFIG_HOME must be set}/.config}"
APPLICATIONS="$DATA_HOME/applications"
TARGET="$APPLICATIONS/$APP_ID"
MIMEAPPS="$CONFIG_HOME/mimeapps.list"
STATE_DIR="$DATA_HOME/fileflux"
# One `mime=previous explicit default` line per type this script took over.
STATE="$STATE_DIR/previous-defaults"
# First state line when mimeapps.list did not exist before this script ran.
CREATED_MARK="# mimeapps.list-created"

usage() {
  sed -n '2,/^$/{/^$/!p}' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

say() { printf 'fileflux: %s\n' "$*"; }
die() { printf 'fileflux: %s\n' "$*" >&2; exit 1; }

# The image MIME types the desktop entry claims (everything but directories).
image_mimes() {
  sed -n 's/^MimeType=//p' "$SOURCE" | tr ';' '\n' | grep -v '^$' | grep -v '^inode/'
}

# The explicit default of $1 in mimeapps.list ([Default Applications]), if any.
explicit_default() {
  [ -f "$MIMEAPPS" ] || return 0
  awk -v key="$1" '
    /^\[/ { in_defaults = ($0 == "[Default Applications]"); next }
    in_defaults && index($0, key "=") == 1 { print substr($0, length(key) + 2); exit }
  ' "$MIMEAPPS"
}

# Sets ($2 non-empty) or removes ($2 empty) the explicit default of $1.
write_default() {
  local mime="$1" value="$2" tmp
  [ -f "$MIMEAPPS" ] || return 0
  tmp="$(mktemp "$MIMEAPPS.XXXXXX")"
  awk -v key="$mime" -v value="$value" '
    /^\[/ { in_defaults = ($0 == "[Default Applications]") }
    in_defaults && index($0, key "=") == 1 {
      if (value != "") print key "=" value
      next
    }
    { print }
  ' "$MIMEAPPS" > "$tmp"
  cat "$tmp" > "$MIMEAPPS"
  rm -f "$tmp"
}

refresh_database() {
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APPLICATIONS" && say "updated the desktop database"
  else
    say "update-desktop-database not found; skipped (menus may need a re-login)"
  fi
}

install_entry() {
  [ -f "$SOURCE" ] || die "missing $SOURCE"
  mkdir -p "$APPLICATIONS"
  if [ -f "$TARGET" ] && cmp -s "$SOURCE" "$TARGET"; then
    say "desktop entry already installed: $TARGET"
  else
    cp "$SOURCE" "$TARGET"
    say "installed $TARGET"
    refresh_database
  fi
  command -v fileflux >/dev/null 2>&1 ||
    say "note: 'fileflux' is not on PATH; the entry will not launch until it is"
}

set_defaults() {
  command -v xdg-mime >/dev/null 2>&1 || die "xdg-mime not found; cannot set defaults"
  # A desktop-specific list beats mimeapps.list when queries are answered.
  local specific
  for specific in ${XDG_CURRENT_DESKTOP:+${XDG_CURRENT_DESKTOP//:/ }}; do
    specific="$CONFIG_HOME/$(printf '%s' "$specific" | tr '[:upper:]' '[:lower:]')-mimeapps.list"
    [ ! -f "$specific" ] || say "note: $specific also exists and is consulted first"
  done
  mkdir -p "$STATE_DIR"
  if [ ! -f "$STATE" ]; then
    # Remember whether mimeapps.list is ours to remove again.
    if [ -f "$MIMEAPPS" ]; then : > "$STATE"; else echo "$CREATED_MARK" > "$STATE"; fi
  fi
  local mime current
  while IFS= read -r mime; do
    current="$(explicit_default "$mime")"
    case "$current" in
      "$APP_ID" | "$APP_ID;")
        say "already the default for $mime"
        continue
        ;;
    esac
    if ! grep -q "^$mime=" "$STATE"; then
      printf '%s=%s\n' "$mime" "$current" >> "$STATE"
    fi
    xdg-mime default "$APP_ID" "$mime"
    say "default for $mime: $APP_ID${current:+ (was $current)}"
  done < <(image_mimes)
}

uninstall() {
  local removed=0
  if [ -f "$STATE" ]; then
    local mime previous current
    local created=0
    while IFS='=' read -r mime previous; do
      [ -n "$mime" ] || continue
      if [ "$mime" = "$CREATED_MARK" ]; then created=1; continue; fi
      current="$(explicit_default "$mime")"
      case "$current" in
        "$APP_ID" | "$APP_ID;")
          write_default "$mime" "$previous"
          say "default for $mime: ${previous:-removed (back to the system default)}"
          ;;
        *) say "left $mime alone (its default is no longer $APP_ID)" ;;
      esac
    done < "$STATE"
    # A mimeapps.list that xdg-mime created and nothing else uses goes away.
    if [ "$created" -eq 1 ] && [ -f "$MIMEAPPS" ] &&
      ! grep -qv -e '^[[:space:]]*$' -e '^\[Default Applications\]$' "$MIMEAPPS"; then
      rm -f "$MIMEAPPS"
      say "removed $MIMEAPPS (created by the install)"
    fi
    rm -f "$STATE"
    rmdir "$STATE_DIR" 2>/dev/null || true
    removed=1
  fi
  if [ -f "$TARGET" ]; then
    rm -f "$TARGET"
    say "removed $TARGET"
    refresh_database
    removed=1
  fi
  [ "$removed" -eq 1 ] || say "nothing to uninstall"
}

mode=install
defaults=0
for arg in "$@"; do
  case "$arg" in
    --default-images) defaults=1 ;;
    --uninstall) mode=uninstall ;;
    -h | --help) usage; exit 0 ;;
    *) usage >&2; die "unknown option '$arg'" ;;
  esac
done

if [ "$mode" = uninstall ]; then
  [ "$defaults" -eq 0 ] || die "--default-images cannot be combined with --uninstall"
  uninstall
else
  install_entry
  if [ "$defaults" -eq 1 ]; then
    set_defaults
  fi
fi
