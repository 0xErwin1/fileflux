# Fileflux desktop standards integration
Goal: reuse existing desktop standards instead of app-private state. Branch: main (local commits, no push).

## Specs
- S1: "Estaria bien que tomaras mis favoritos del global de mi sistema. Todo lo que puedas usar que ya sea un estadndar, usalo"
- S2 (agent plan told to the user): GTK bookmarks `~/.config/gtk-3.0/bookmarks` become the source of truth for favorites (read and write), watched with inotify, one-time migration of settings.toml favorites, preserving labels, order and unknown lines (sftp://, smb://), atomic writes. Next by value: `.hidden` per-folder files (read-only, joins the hidden filter); Recents from `~/.local/share/recently-used.xbel` (read-only first); freedesktop thumbnail cache `~/.cache/thumbnails` (read-only first).

## Tasks
- T1 | S1-S2 | route: writer after devices T4 | pending | GTK bookmarks as favorites (read/write, watch, migrate, preserve unknown lines, labels) + `.hidden` support + entry-menu favorites (ux-polish T5) | commit: -
- T2 | S1-S2 | route: writer | pending | Recent files from recently-used.xbel (read-only) in the sidebar | commit: -
- T3 | S1-S2 | route: writer | pending | Freedesktop thumbnail cache read (normal/large, Thumb::URI + MTime validation) for grid thumbnails | commit: -

## Log
- L1: "Estaria bien que tomaras mis favoritos del global de mi sistema. Todo lo que puedas usar que ya sea un estadndar, usalo"
