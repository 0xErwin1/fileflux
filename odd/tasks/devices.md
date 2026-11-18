# Fileflux devices: mount, unmount, eject
Goal: manage removable drives and partitions from the sidebar. Branch: main (local commits, no push).

## Specs
- S1: "Si, dale con eso" — approval of the agent's proposal, quoted verbatim: "Te recomiendo seguir con **montar, desmontar y expulsar USB y particiones**."; "con `udisksctl`, sin root y sin shell, como hacen los file managers de escritorio. Mostraría los dispositivos conectados pero sin montar, para poder montarlos con un click. Expulsar se haría con confirmación si hay operaciones en curso sobre ese disco, y apagaría el dispositivo."; "desmontar mientras una copia o un movimiento escribe en ese disco puede corromper datos. Expulsar tiene que esperar a esas operaciones o pedir cancelarlas. Además, las pestañas abiertas en ese disco tienen que subir a otra carpeta en lugar de quedar rotas."
- S2 (original list): "Montar, desmontar y expulsar USB."

## Tasks
- T1 | S1-S2 | route: writer | pending | Device discovery (mounted + unmounted mountable block devices), mount/unmount/eject via udisksctl argv without shell, busy-operation guard, tab fallback, live device updates, sidebar UI + context menu, tests + native validation | commit: -

## Log
- L1: "Que es lo siguiente?" → agent recommended devices.
- L2: "Si, dale con eso"
