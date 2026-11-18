# Fileflux refined shell (visual pass)
Goal: bring the native explorer to the approved direction in design/file-explorer/04-refined-shell.html, read-only only. Branch: main (local commits, no push).
Reference artboard: design/file-explorer/04-refined-shell.html; tokens: design/system.md. CRUD affordances in the artboard (context menu, New folder, rename, trash) are OUT of scope here: the explorer stays read-only (explorer-mvp S3) until CRUD is its own feature.

## Specs
- S1: "Creo que esto es muy mejorable [...] el MVP me gusta pero el estilo deja mucho que deciar para ser bolt."
- S2: "El tema design, me gusta, metelo en el design asi lo puedo ver." / "sino segui con lo que hay despyes lo veo" — the user approved the proposed visual pass shown in 04-refined-shell and asked to continue implementing it.
- S3 (sidebar, from the approved proposal): "Cada lugar muestra su ruta completa al lado del nombre. Mete ruido, se trunca y le quita peso al nombre. La ruta debería ir en un tooltip." / "\"Explorer\" arriba, \"Favorites (session only)\" y el pie \"Local files, read-only\" son texto que no aporta nada." / "Los volúmenes no muestran capacidad ni uso. Una barra fina con el espacio libre es lo que le da cara de herramienta seria."
- S4 (toolbar): "La ruta es un campo de texto plano. Debería ser una barra de breadcrumbs con segmentos clicables, que pase a modo edición con click o `Ctrl+L`." / "Todos los iconos pesan igual. Grilla/lista debería ser un control segmentado y no dos botones sueltos."
- S5 (grid): "Los iconos de carpeta son planos, chicos y del mismo azul saturado." / "Sobra aire alrededor de cada tile." / "La selección casi no se ve [...]: falta un tinte de acento y un focus ring." / "La primera fila queda pegada al borde del toolbar."
- S6 (list): "La columna Type repite \"Folder\" y Size muestra \"—\" en cada carpeta: es ruido. Para carpetas puedes mostrar la cantidad de items." / "La fecha sale en UTC absoluto (\"2026-11-18 12:01 UTC\"). Va en hora local, o relativa si es reciente." / "Los separadores entre filas pesan mucho y los headers no indican por qué columna se ordena."
- S7 (status bar): "La línea fija de atajos de teclado se come el espacio. Ahí debería ir la selección (\"3 seleccionados · 12 MB\") y el espacio libre del volumen."
- S8 (preview direction shown in 04): details panel (Ctrl+I toggle) showing the selected entry's name, kind, size or item count, modified, permissions, path, and an image preview for image files; multi-selection shows count and total size. Read-only: no Open/Rename/Trash buttons yet.

## Tasks
- T1 | S3 | route: writer wave 1 | done | Sidebar names-only + tooltips, drop noise labels/footer/header, volume usage meter + "X free of Y" via statvfs; Xvfb screenshot observed | commit: e237e4c
- T2 | S6 | route: writer wave 1 | done | List: drop Type column, local/relative dates, lighter separators, sort indicator + click-to-sort (Xvfb: click flipped to Name desc, folders first); folder item counts need an Entry field -> T4 follow-ups | commit: 6a9e556
- T3 | S5 | route: writer wave 1 | done | Grid: two-tone larger folder, image thumbnails (Xvfb: real JPG thumbnails rendered), size subline, selection + focus ring, top padding | commit: b5f0c2b
- T4 | S4,S6,S7 | route: writer wave 2 | pending | Breadcrumbs with edit mode, segmented view toggle, folder title subbar, status bar selection/free space; integration follow-ups: list sort order must drive keyboard/range selection, one decimal size formatter everywhere, Size right-aligned, sort arrow in tint, root volume label | commit: -
- T5 | S8 | route: writer wave 2 (after T4) | pending | Details/preview inspector panel | commit: -

## Log
- L1: "Creo que esto es muy mejorable [Image#1] [Image#3] el MVP me gusta pero el estilo deja mucho que deciar para ser bolt. Tambien de paso dame una lista de feature a agregar, falta todo lo que es CRUD, preview y demas"
- L2: "El tema design, me gusta, metelo en el design asi lo puedo ver. No se si podes hacer que se levante sobre tailscale asi lo veo desde el telefono porque no voy a estar." → artboard 04-refined-shell added.
- L3: "no resulve no lo puedo verlo seguramente es tema de firewall. sino segui con lo que hay despyes lo veo" → implement 04 visual pass, read-only scope; CRUD stays a separate feature.
- L4: Wave 1 integrated: fmt/clippy/tests (38+38+5) green on combined tree; Xvfb (headless X, real screen locked) screenshots observed sidebar meters, grid thumbnails, list sort toggle. Defects found: keyboard order ignores list sort (writer report), three size formatters disagree (grid 3.0 MB vs list 2.9 MB), Size column left-aligned, sort arrow drawn in warning-like color, root volume labelled '/'. Routed to T4.
