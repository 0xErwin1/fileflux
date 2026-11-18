# Fileflux UX polish and large-folder performance
Goal: clearer context-menu targets, sidebar context menu, resizable side panels, better toast placement, and no UI freeze on huge folders. Branch: main (local commits, no push).

## Specs
- S1: "me cuesta diferenciar donde puedo hacer click derecho y ver el menu contextual de donde estoy parado." (screenshot: grid of the Home folder)
- S2: "Me falta opciones en la sidebar para abrir, abrir en una tab, marcar favorito (cuando este)"
- S3: "Tambien me falta que se puedan redimencionar las sidebars"
- S4: "las notificaciones parecen medio tiradas tal vez un poco mas arriba no estarian mal" (screenshot: toast "Moved “syncthing.lock” back" floating over the grid below the subbar, right side)
- S5: "Sobre las pruebas de 990k archivos, hacel[o]" — the earlier finding: filtering a ~990,000-entry folder froze the UI for 30 s+ in a debug build.
- Agent plan announced to the user: hover highlight on tiles/rows; the right-clicked target keeps an accent outline while the menu is open; menu header naming the target ("report.pdf", "3 items", "Home (this folder)"); sidebar menu Open / Open in new tab / Add or Remove favorite / Copy path / Properties; drag the borders of the sidebar and the details panel, widths persisted in settings.toml, double-click resets; toasts top-centered under the toolbar in the title band so they never cover Sort/Filter/New folder/Empty Trash; measure in release, move per-keystroke filter/sort work off the UI thread.
- S6: "Me gustaria que los sub menus de un menu contextual no remplace el menu sino que sea escalado" — cascading (flyout) submenus instead of drill-in pages.

## Tasks
- T1 | S1-S4 | route: writer (parallel with T2, disjoint surfaces) | done | Context target highlight + menu header, sidebar context menu, resizable sidebar and details panel (persisted), toast placement | commit: 2f4be0e
- T2 | S5 | route: writer (parallel with T1) | done (state side) | Profile and fix huge-folder filter/sort freeze: incremental/cached keys, off-UI-thread arrangement with generation guard, release measurements before/after | commit: 82db1bc
- T3 | S5 | route: writer | done | Wire the async arrangement in the UI: build ArrangeIndex on the scan thread, commit_indexed, arrange_tab background jobs with generation guard, visible_shared in render paths, move refresh_snapshot clone off the UI thread, native validation on a 1M-entry folder | commit: 0068b56
- T4 | S6 | route: blocked on Bolt | blocked | Cascading submenus for Sort by / Filter (context menus and subbar buttons) | commit: -

## Log
- L1: "[Image#1] me cuesta diferenciar donde puedo hacer click derecho y ver el menu contextual de donde estoy parado. Me falta opciones en la sidebar para abrir, abrir en una tab, marcar favorito (cuando este) Tambien me falta que se puedan redimencionar las sidebars [Image#2] las notificaciones parecen medio tiradas tal vez un poco mas arriba no estarian mal Sobre las pruebas de 990k archivos, hacel"
- L2: T1 (2f4be0e): compile-level RED then GREEN; native Xvfb PASS for hover/outline/headers (tile, row, empty space, sidebar), sidebar favorite add/remove in settings.toml, open in new tab, Properties, Trash Empty from anywhere, panel resize persisted + double-click reset, toast band in grid/list/Trash and 900px fallback. Not native: <760px window, sidebar Copy path.
- L3: T2 (82db1bc): release 1M entries — keystroke 140-220 -> 4-18 ms, filters 156-340 -> 4-12 ms, sort Size 2818 -> 336, Type 9259 -> 696, cached re-sort 2 ms; read_directory real 1M files 3.6-4.35 s -> 1.76 s. Equivalence tests vs reference over seeded random listings; RED by mutation only. UI wiring not applied (view.rs out of surface) -> T3.
- L4: T3 (0068b56): RED 4 settle_arrange tests vs stub + compile RED; tests 176+375+9+4+30. Native 1,000,300-entry folder, max UI stall (repaint probe, floor ~110-160 ms): release typing 229-476 -> 113-225 ms, sort Type 1013 -> 173 ms; debug typing up to 1988 -> <=329 ms, sort Size 2491 -> 670 ms; no lost input events after (8 lost before in debug). The original 30 s debug freeze no longer reproduces. 'Sorting…' appears after ~150 ms with the previous order kept.
- L5: "Me gustaria que los sub menus de un menu contextual no remplace el menu sino que sea escalado" → T4.
- L6: "No hay sub menus? Eso deberia de estar, es algo que falta quitar de dbflux y generalizar clarmaente. Dame letra para mandar otro agente" → cancelled the in-app hand-rolled cascade writer (its partial edits to arrange_menu.rs/ops.rs/ops_tests.rs discarded) to avoid duplicating what belongs in Bolt; T4 waits for a generic Bolt submenu extracted from DBFlux, then Fileflux bumps the bolt rev and adopts it.
