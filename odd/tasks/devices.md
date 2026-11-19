# Fileflux devices: mount, unmount, eject
Goal: manage removable drives and partitions from the sidebar. Branch: main (local commits, no push).

## Specs
- S1: "Si, dale con eso" — approval of the agent's proposal, quoted verbatim: "Te recomiendo seguir con **montar, desmontar y expulsar USB y particiones**."; "con `udisksctl`, sin root y sin shell, como hacen los file managers de escritorio. Mostraría los dispositivos conectados pero sin montar, para poder montarlos con un click. Expulsar se haría con confirmación si hay operaciones en curso sobre ese disco, y apagaría el dispositivo."; "desmontar mientras una copia o un movimiento escribe en ese disco puede corromper datos. Expulsar tiene que esperar a esas operaciones o pedir cancelarlas. Además, las pestañas abiertas en ese disco tienen que subir a otra carpeta en lugar de quedar rotas."
- S2 (original list): "Montar, desmontar y expulsar USB."

## Tasks
- T1 | S1-S2 | route: writer | done | Device discovery (mounted + unmounted mountable block devices), mount/unmount/eject via udisksctl argv without shell, busy-operation guard, tab fallback, live device updates, sidebar UI + context menu, tests + native validation | commit: 85af66c
- T2 | S1 | route: independent gentle-ai-verify (Risk 1, 4) | done (FAIL) | Verify busy guard, eject ordering, tab fallback, system-disk exclusion against the real (read-only) lsblk of this machine, parser robustness | commit: n/a
- T3 | S1 | route: writer | pending | Fix verifier blockers A (operations started after an eject began are not waited for; plan does not re-check before run_plan) and B (paths compared textually; symlinks into the device escape guard/tab move/watch release) + advisories: replace hand-written JSON with serde_json, --block-device=, unmount-failure notice text, '0 operations' toast, all mount points of a device, dual-boot system-disk rule (only hide system partitions, not the whole disk) | commit: -

## Log
- L1: "Que es lo siguiente?" → agent recommended devices.
- L2: "Si, dale con eso"
- L3: T1 (85af66c): RED 32/36 core + 13 explorer vs stubs, GREEN; lib suite x15. Tests 211+391+9+4+30. Native Xvfb with fake lsblk/udisksctl PASS: rows (mounted + 'Not mounted'), click-to-mount argv + navigate, eject tooltip, menu, eject during a 6000-file copy waits (unmount logged only after the copy finished, 0 watches on the device, then power-off), tab moved out before unmount with notice, busy error toast, polling refresh only while focused (2 s). Decisions: polling instead of /dev/disk/by-uuid inotify; any disk with a system mount or active swap hidden from the unmounted list; fallback for /run/media/<user>/X lands on /run/media/<user>. Hand-written lsblk JSON reader (serde_json exists in the lock: candidate to replace).
- L4: T2 independent verify: FAIL. Blocker A: Eject with nothing busy, then click device row and Ctrl+V a 30k-file copy -> unmount at 1217 files, power-off at 1218, 'removed safely' while copying. Blocker B: symlink ~/usblink -> device, paste through it, Eject -> no busy dialog, tab not moved, watch kept, power-off at 2321 files. PASS: real-machine discovery read-only (System, boot, data; nvme0n1p3/p4 NTFS mountable; nix absent is pre-existing dedup), eject ordering, failed unmount aborts power-off, process runner argv/timeout/LC_ALL. Advisories routed to T3.
