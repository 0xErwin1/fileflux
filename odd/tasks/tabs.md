# Fileflux tabs
Goal: several folders open at once in one window, each with its own state. Branch: main (local commits, no push).
Reference: design/system.md tokens; tab strip sits inside the document island above the toolbar, Bolt style (square, 26–30px, tint marker on the active tab, no rounded browser tabs).

## Specs
- S1: "Tambien de paso dame una lista de feature a agregar, falta todo lo que es CRUD, preview y demas" — the accepted list included "Pestañas, y quizás panel dual."
- S2: "Funciona y veo bien las cosas" — after the agent announced "Lo siguiente de la lista es pestañas o panel dual, o montar y expulsar USB. Si no me dices otra cosa, sigo con pestañas.", the user accepted the current state without redirecting.
- S3 (behavior, proposed by the agent from common file managers): Ctrl+T opens a new tab on the current folder; Ctrl+W closes the current tab (the last tab is never closed: it stays); Ctrl+Tab / Ctrl+Shift+Tab and Ctrl+PageDown / Ctrl+PageUp cycle; Alt+1..9 jumps to a tab; middle-click on a folder (grid, list, sidebar, breadcrumb) and a context-menu "Open in new tab" open it in a new background tab; each tab keeps its own location, back/forward history, selection, view mode, sort, filter, scroll and path-edit state; the tab strip shows the folder name (Trash shows "Trash"), full path in a tooltip, a close button on hover/active, and appears only when there are 2+ tabs; tabs are session-only; the CLI path opens as the first tab.

- S4: "A2 no podemos tener un wacher? Que mire eventos de esa carpeta para poder tener actualizaciones en tiempo real que es lo mas normal en estos casos" — every tab, active or not, updates in real time from filesystem events, including a tab still navigating into a folder.

## Tasks
- T1 | S1-S3 | route: writer | done | Per-tab state extraction, tab strip, shortcuts, middle-click and "Open in new tab", live watcher and ops refresh per active tab, tests + native validation | commit: d8f3b3b, d13b93d

- T2 | S4 | route: writer | done (fixes pending) | One shared watcher: a single dedicated thread with one inotify fd and one watch per distinct folder (refcounted across tabs; added when a tab's scan starts so a pending navigation is covered), events dispatched to every tab showing or loading that folder, debounced per folder; replaces per-tab watchers and activation-only refresh; closes A2 | commit: 09147ce
- T3 | S4 | route: writer | pending | Fix verifier F1 (folder replaced at the same path left the tab with a dead handle: retarget compares paths only) and F2 (Gone lost on queue overflow; attach bumps a dead entry): handle identity/liveness, overflow re-validation of every watched folder, regression tests from the verifier; also A1 surface a dead hub thread | commit: -

## Log
- L1: "Tambien de paso dame una lista de feature a agregar, falta todo lo que es CRUD, preview y demas"
- L2: "Funciona y veo bien las cosas" → continue with tabs as announced.
- L3: T1 (d8f3b3b): RED 18/20 failing stubs then GREEN; tests 132+232+5+4. Writer native PASS (a)-(f) except Ctrl+1/2 N/A (never existed; not added). Background tabs drop their watcher and re-read on activation (a watcher parks a shared executor thread). Native review not due (medium). Independent verify (Risk item 4): no blockers; races with 1M-entry scans, closing origin tab mid-copy, cut/paste/undo across tabs, background freshness all PASS. Advisories: A1 stale failed refresh could override a navigation and A3 duplicate affected folders fixed in d13b93d (A3 RED observed); A2 (tab navigating into an affected folder skipped by refresh) not reproduced, left open; A4 rename dropped on tab switch (documented) and cut notice lingering; filtering 990k entries in debug build blocks UI ~30s (not tab-related, unchecked at base).
- L4: "A2 no podemos tener un wacher? Que mire eventos de esa carpeta para poder tener actualizaciones en tiempo real que es lo mas normal en estos casos" → T2: one inotify instance on one dedicated thread multiplexing all tab folders.
- L5: T2 (09147ce) independent verify: suite and 40 watch-test loops PASS, 5x8s stress with 1750 folder delete/recreate cycles no deadlock and 0 leaked watches, ENOSPC/EACCES safe, idle 1.2% CPU / constant threads. FAIL F1 (regression): rm -rf DIR && mkdir DIR leaves the tab with 0 watches forever (fallback commits the same path, retarget keeps the dead handle). FAIL F2: IN_IGNORED lost on queue overflow, hub keeps a dead entry and later watch() reuses it. Routed to T3.
