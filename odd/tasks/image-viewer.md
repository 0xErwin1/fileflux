# Fileflux image viewer
Goal: view images inside Fileflux and make it usable as the system default image viewer. Branch: main (local commits, no push).
Research (read-only, see Log L2): GPUI decodes png/jpeg/gif/webp(animated)/bmp/tiff/ico/svg/qoi/tga/hdr/exr with EXIF orientation applied; no avif/heic/raw; full decode in memory; zoom/pan via sized+offset img (Lumia approach), rotation needs CPU rotate (image crate already in lock); no single-instance mechanism.

## Specs
- S1: "investigate que tan viable es  tener un visualizador de imagnes dentro de la app, sin abri el por defecto del sistema (en mi caso brave porque no tengo visualizador y quiero que fileflux lo llege a ser)"
- S2: Decision "¿Qué hace `fileflux foto.png` cuando el archivo es una imagen?" → "Abre el visor (recomendado)": "Imagen → visor; otros archivos siguen abriendo su carpeta. Necesario para ser visor por defecto."
- S3: Decision "¿Cómo se muestra el visor?" → "Ventana propia de visor (recomendado)": "Sin sidebar, flechas para anterior/siguiente en la carpeta; la misma vista se abre desde el explorador con Enter/doble click."
- S4: Decision "¿Qué entra en la primera versión?" → "Básico + default (recomendado)": "Ajustar/zoom/pan, rotar, anterior/siguiente, GIF/WebP animados, info, .desktop + xdg-mime. Sin HEIC/RAW ni mosaicos."
- S5: Reference shared by the user: https://github.com/iFence/Lumia (GPUI image viewer), "Por si queres inspiracion".

## Tasks
- T1 | S2-S5 | route: writer (parallel with tabs T2, disjoint surfaces) | pending | Viewer window module, CLI contract change, .desktop + install script, tests + native validation | commit: -
- T2 | S3 | route: inline after tabs T2 | pending | Explorer hookup: Enter/double-click on an image opens the viewer window (other files keep xdg-open) | commit: -
- T3 | S4 | route: ask user | pending | Install the .desktop on the user's real system and set xdg-mime defaults (outside the repo: needs explicit OK) | commit: n/a

## Log
- L1: "En paralelo, investigate que tan viable es  tener un visualizador de imagnes dentro de la app, sin abri el por defecto del sistema (en mi caso brave porque no tengo visualizador y quiero que fileflux lo llege a ser)"
- L2: Explorer research: viable without new deps; risks: no HEIC/AVIF/RAW, huge images fully decoded and may exceed GPU texture max, no single-instance. User decisions recorded as S2-S4.
