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
- T1 | S2-S5 | route: writer (parallel with tabs T2, disjoint surfaces) | done | Viewer window module, CLI contract change, .desktop + install script, tests + native validation | commit: 3dfd526
- T2 | S3 | route: writer after tabs T3 | done | Explorer hookup: Enter/double-click on an image opens the viewer window (other files keep xdg-open) | commit: 2e13242; rotation via image crate 96f4a7f
- T3 | S4 | route: ask user | dropped | Install the .desktop on the user's real system and set xdg-mime defaults (outside the repo: needs explicit OK) | commit: n/a

## Log
- L1: "En paralelo, investigate que tan viable es  tener un visualizador de imagnes dentro de la app, sin abri el por defecto del sistema (en mi caso brave porque no tengo visualizador y quiero que fileflux lo llege a ser)"
- L2: Explorer research: viable without new deps; risks: no HEIC/AVIF/RAW, huge images fully decoded and may exceed GPU texture max, no single-instance. User decisions recorded as S2-S4.
- L3: T1 (3dfd526): RED 36/39 viewer pure tests vs stubs then GREEN; tests 152+283+9+4. Native Xvfb PASS (a)-(g): CLI routing + stderr notes, nav without wrap, fit/zoom-around-pointer (pixel invariant), pan, rotation incl. EXIF-6 JPEG and SVG, animated GIF frames, corrupt and too-large messages, info panel, Esc/Q/F, install script in fixture (xdg-mime default fileflux.desktop, uninstall restores byte-identical mimeapps.list). Rotation without a direct `image` dependency: rotate GPUI's BGRA and re-encode as an uncompressed PNG (hand-rolled encoder/crc32/adler32, ~100 lines) — candidate to replace with the `image` crate (already in Cargo.lock). INCIDENT: the writer's XTest helper inherited DISPLAY=:0 and injected clicks at (700,400),(1236,76)x3 and keys Right,Right,Left,End,Right,Home,Left,PageDown,PageUp,Right,i,Escape,i into the user's real XWayland session; two GUI-launching integration tests hung and were removed. Reported to the user; all writers now pin DISPLAY to their Xvfb.
- L4: User tested a PNG: "Me sigue abriendo por defecto en brave en vez de ir directamente en la app. Y en open with no me sale la opcion de la app. Estoy probando con png" — cause: explorer Enter/double-click still used xdg-open (T2 in progress) and fileflux.desktop is not installed / fileflux not on PATH. Then: "No es necesario. Con que cuando hago doble click me abra en la app me sirve" → T3 (system install) dropped; T2 is the acceptance path.
- L5: T2 (2e13242, 96f4a7f): RED 3/4 open_target/menu tests vs stubs, GREEN; tests 152+285+9+4; Cargo.lock diff = one direct edge to image 0.25.10, no new packages. Native Xvfb (pinned DISPLAY): grid double-click opens viewer at 2/3, list sorted by size follows displayed order, Enter on .txt calls (fake) xdg-open, context menu Open in viewer, rotation via RenderImage keeps BGRA order. Image crate now gives rotated pixels to GPUI without re-encoding.
