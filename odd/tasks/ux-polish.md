# Fileflux UX polish and large-folder performance
Goal: clearer context-menu targets, sidebar context menu, resizable side panels, better toast placement, and no UI freeze on huge folders. Branch: main (local commits, no push).

## Specs
- S1: "me cuesta diferenciar donde puedo hacer click derecho y ver el menu contextual de donde estoy parado." (screenshot: grid of the Home folder)
- S2: "Me falta opciones en la sidebar para abrir, abrir en una tab, marcar favorito (cuando este)"
- S3: "Tambien me falta que se puedan redimencionar las sidebars"
- S4: "las notificaciones parecen medio tiradas tal vez un poco mas arriba no estarian mal" (screenshot: toast "Moved “syncthing.lock” back" floating over the grid below the subbar, right side)
- S5: "Sobre las pruebas de 990k archivos, hacel[o]" — the earlier finding: filtering a ~990,000-entry folder froze the UI for 30 s+ in a debug build.
- Agent plan announced to the user: hover highlight on tiles/rows; the right-clicked target keeps an accent outline while the menu is open; menu header naming the target ("report.pdf", "3 items", "Home (this folder)"); sidebar menu Open / Open in new tab / Add or Remove favorite / Copy path / Properties; drag the borders of the sidebar and the details panel, widths persisted in settings.toml, double-click resets; toasts top-centered under the toolbar in the title band so they never cover Sort/Filter/New folder/Empty Trash; measure in release, move per-keystroke filter/sort work off the UI thread.

## Tasks
- T1 | S1-S4 | route: writer (parallel with T2, disjoint surfaces) | pending | Context target highlight + menu header, sidebar context menu, resizable sidebar and details panel (persisted), toast placement | commit: -
- T2 | S5 | route: writer (parallel with T1) | pending | Profile and fix huge-folder filter/sort freeze: incremental/cached keys, off-UI-thread arrangement with generation guard, release measurements before/after | commit: -

## Log
- L1: "[Image#1] me cuesta diferenciar donde puedo hacer click derecho y ver el menu contextual de donde estoy parado. Me falta opciones en la sidebar para abrir, abrir en una tab, marcar favorito (cuando este) Tambien me falta que se puedan redimencionar las sidebars [Image#2] las notificaciones parecen medio tiradas tal vez un poco mas arriba no estarian mal Sobre las pruebas de 990k archivos, hacel"
