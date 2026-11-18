# Fileflux settings persistence
Goal: remember preferences, favorites and open tabs between sessions. Branch: main (local commits, no push).

## Specs
- S1: "estaria bien ir pensando en presisntecia de perferencias y settings"
- S2: "Si, avanza" — approval of the agent's proposal, quoted verbatim: "Formato y lugar: `$XDG_CONFIG_HOME/fileflux/settings.toml` (normalmente `~/.config/fileflux/`)"; "Preferencias globales: vista por defecto (grilla o lista), orden y filtros por defecto, mostrar ocultos, carpetas primero, orden natural, ancho de la sidebar y si arranca colapsada, panel de detalles abierto o cerrado, y tamaño de la ventana."; "Favoritos: hoy se pierden al cerrar"; "Opcional: las pestañas abiertas al cerrar, para restaurarlas al abrir."; "Escritura segura: se escribe a un archivo temporal y después se renombra, así un crash nunca deja el archivo a medias. Un archivo corrupto o de una versión vieja no rompe el arranque: se usan los valores por defecto y se avisa en la barra de estado. Cada archivo lleva un campo `version` para poder migrarlo más adelante."; "Interfaz: primero, que lo que cambias desde la app (vista, orden, favoritos) se guarde solo."; "Te propongo empezar por favoritos, preferencias globales y pestañas restauradas, y dejar lo de \"recordar por carpeta\" para después."

## Tasks
- T1 | S1-S2 | route: writer | pending | settings.toml (prefs + favorites) and state file for restored tabs, atomic writes, tolerant load with status notice, version field, autosave on change (debounced), CLI path still wins, tests + native validation | commit: -

## Log
- L1: "Primero, me gustaria que estuvieran tambien en el menu contextual. Segundo, estaria bien ir pensando en presisntecia de perferencias y settings"
- L2: "Si, avanza" → T1 with favorites, global preferences and restored tabs; per-folder memory deferred.
