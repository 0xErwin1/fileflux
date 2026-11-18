# Fileflux sort and filters
Goal: sort and filter the entries of a folder in grid and list, per tab, session-only. Branch: main (local commits, no push).

## Specs
- S1: "me falta filtros y sort en los archivos"
- S2: Filters chosen: "Por tipo" (Chips: Carpetas, Imágenes, Videos, Audio, Documentos, Archivos comprimidos, Código, Otros.); "Mostrar/ocultar ocultos" (Toggle para dotfiles (Ctrl+H), recordado por pestaña.); "Por fecha de modificación" (Hoy, últimos 7 días, últimos 30 días, este año, más antiguos.); "Por tamaño" (Rangos: < 1 MB, 1–100 MB, 100 MB–1 GB, > 1 GB.). The existing name search stays and combines with them.
- S3: Sort chosen: "Nombre, tamaño, fecha, tipo" (Los cuatro criterios, ascendente/descendente, en grilla y lista por igual.); "Carpetas primero (toggle)" (Hoy siempre van primero; permitir mezclarlas.); "Orden natural de números" ("IMG_2" antes que "IMG_10" en vez de orden alfabético estricto.)
- S4: Controls: "Botones en la subbarra (recomendado)" — "Junto a \"Pictures · 4 folders · 7 files\": menú Ordenar y menú Filtrar, con chips activos visibles y limpiables."
- S5: Persistence: "Solo por pestaña, en la sesión (recomendado)" — "No escribe configuración en disco, igual que favoritos hoy."
- S6: "Primero, me gustaria que estuvieran tambien en el menu contextual." — Sort and Filter submenus also in the empty-space context menu.

## Tasks
- T1 | S1-S5 | route: writer | pending | Sort model shared by grid and list (4 keys, asc/desc, folders-first toggle, natural order), filter model (type categories, hidden toggle Ctrl+H, date ranges, size ranges) combined with name search, subbar Sort/Filter menus + removable active chips, per-tab state, keyboard/selection/viewer/paste order follow the result, counts "N of M", tests + native validation | commit: -

## Log
- L1: "me falta filtros y sort en los archivos"
- L2: Answers recorded as S2-S5 (all filters, all sort options, subbar controls, per-tab session-only).
- L3: "Primero, me gustaria que estuvieran tambien en el menu contextual. Segundo, estaria bien ir pensando en presisntecia de perferencias y settings" → S6 added to T1; persistence is a design discussion (separate feature), not part of T1.
