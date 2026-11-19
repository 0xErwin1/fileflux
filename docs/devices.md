# Devices

Mount, unmount and eject drives from the sidebar, and browse phones and network shares. Fileflux never mounts or unmounts anything on its own.

## Quick path

1. Plug in a USB drive; it appears under **Devices** (a few seconds at most).
2. Click it to mount and open it, or right-click for **Mount**, **Unmount** or **Eject**.
3. Wait for the toast "<name> can be removed safely" before unplugging.

## What is listed

The **Devices** section shows mounted volumes (one entry per device, with a usage meter; the root filesystem reads "System") and, under them, filesystems that are not mounted yet: muted, with "Not mounted" under the name. Unmounted filesystems come from `lsblk -J` on removable, USB and hot-plug disks and on non-system disks.

Left out: swap, RAID/LVM members, loop, zram and optical devices, empty card readers, and partitions that hold a system mount (`/`, `/boot`, `/efi`, `/usr`, `/var`, `/nix`, `/etc`, `/home`, `/opt`, `/swap`, `/root`) or active swap, together with the encrypted container above them.

- **Dual boot:** the other partitions of the system disk (a Windows or data partition) are listed and can be mounted and unmounted, but the system disk is never ejected.
- An encrypted (LUKS) container nobody unlocked shows as **Locked**; unlocking is not supported yet.
- A device without a label is called "<size> Volume".

## Actions

Everything goes through `udisksctl`: no root, no shell, always with `--no-user-interaction`, off the UI thread, with a 30 s timeout.

| Action | How | What runs |
|---|---|---|
| Mount | Click the unmounted row, or right-click → **Mount** | `udisksctl mount --block-device=<device> --no-user-interaction`; on success the tab goes to the mount point reported |
| Unmount | Right-click a mounted removable volume → **Unmount** | `udisksctl unmount --block-device=<device> --no-user-interaction` |
| Eject | The eject icon on a removable row, or right-click → **Eject** | Every mounted partition of the disk is unmounted (deepest first), then `udisksctl power-off --block-device=<disk> --no-user-interaction` |

| Action | Allowed for |
|---|---|
| Mount | An unmounted, unlocked filesystem |
| Unmount | A mount under `/run/media` or `/media`, or any mount on a removable or hot-plug disk; never a system or `fstab` mount elsewhere |
| Eject | Removable or hot-plug disks only |

A busy or unauthorized device shows a toast with the reason `udisksctl` gave ("Could not unmount KINGSTON: target is busy").

## Safety rules

| Rule | Behavior |
|---|---|
| Running operations are counted | Before an unmount or eject, operations that read or write on the device are counted. If any, a dialog says "N operations are using <name>" and lists them, with **Wait and eject** (or **Wait and unmount**) and **Cancel** (the default). Waiting queues the action and runs it once nothing uses the device |
| Draining | From the moment an unmount or eject is queued or running, the device is *draining*: a paste, drag and drop, new folder/file, rename, Move to Trash, delete or undo there is refused with "<name> is being ejected" ("…unmounted" for a plain unmount). So is opening it: a sidebar click, breadcrumb, typed path, double-click, Back/Forward or a new tab |
| Re-check | Right before every unmount the running operations are checked again, off the UI thread. If one started meanwhile, the plan stops with "Could not eject <name>: an operation started using it" and nothing is unmounted |
| Honest success | "<name> can be removed safely" appears only after the power-off itself succeeded |
| Real locations | Whether something is "on the device" is decided by where it really is, not by how its path is written: a symlink pointing to the device, a path with `..`, or a destination that does not exist yet all count. Every mount point of a device counts (a bind or subvolume mount too) and every one is unmounted. A trashed symlink counts too: the check errs on the safe side |
| Tabs move out first | Tabs showing (or opening) a folder on the device move to the nearest existing ancestor outside the mount, and their live-refresh watches are released before the unmount, because an inotify watch can keep a mount busy. The notice is written after the outcome is known: "<name> was unmounted; showing <folder>" or "Could not unmount <name>; <reason>". A failed unmount does not move the tabs back; Back returns to the device |

## Missing tools

- Without `udisksctl`, the mount and eject actions and the unmounted rows are hidden, with a one-line notice under the section label.
- Without `lsblk`, only mounted volumes show, with a notice.

## Phones and gvfs mounts

Everything in `$XDG_RUNTIME_DIR/gvfs/` (where gvfs mounts phones, shares and remote machines) is listed in the Devices section, below the disks, with a friendly name parsed from the mount name:

| Mount name | Shown as | Icon |
|---|---|---|
| `mtp:host=Google_Pixel_8_39060DLJH0031N` | "Google Pixel 8" (serial dropped) | phone |
| `smb-share:server=nas,share=media` | "media on nas" | network |
| `sftp:host=x` | "x" | server |

A name that does not parse is shown as it is. Listing only reads the directory; it never mounts or opens anything. There is no usage meter unless `statvfs` answers within 2 s (a locked phone can stall it). An MTP phone usually holds storages such as "Internal shared storage": click the phone and navigate into one.

**What works:** browse, open, copy to and from, move, rename, create folders and files, and delete.

**What is different:**

- **No Trash.** Delete asks the usual "no Trash" confirmation and deletes permanently. Fileflux never creates a `.Trash-<uid>` on a phone or share.
- **No eject and no Mount.** gvfs mounts them. **Unmount** is in the right-click menu only when `gio` is installed (`gio mount -u <path>`, no shell); without it, unmount from your system's file manager. The same busy and draining guards as for disks apply.
- **FUSE-safe operations.** A copy is still built under a hidden name and renamed into place. If the filesystem cannot do the no-replace rename (`EINVAL`, `ENOTSUP`, `ENOSYS`), a pre-check and a plain rename are used (with the usual race window). If it cannot rename the finished copy at all, the item is copied straight to its final name, only when that name does not exist, and what was written is deleted if the copy fails or is cancelled. A name somebody else took meanwhile is never touched. Unsupported `fsync` is not an error, and neither are refused modification times and permissions: the toast's details say "Modification times and permissions could not be set on this filesystem". This also applies to other FUSE mounts (sshfs, ntfs-3g…), found by filesystem type in the mount table. On a local disk those failures are still failures.

## Refresh

The list updates by events, not by polling, whether the window is focused or not.

- One background thread watches `/dev` (block nodes appearing and disappearing), `/dev/disk/by-id` and `/dev/bus/usb` (inotify), `/proc/self/mountinfo` (`poll` with `POLLPRI`, which the kernel raises on every mount or unmount, by any tool) and `$XDG_RUNTIME_DIR/gvfs` (inotify; its parent is watched for the directory to appear).
- An event waits for 300 ms of quiet (a burst is one read, at most 1.5 s late), then re-reads the mount table, the gvfs directory and `lsblk` on a background thread, never two at once.
- Plugging in a USB device schedules two more reads, 2 s and 5 s later, because udisks and gvfs mount it a moment after the kernel node appears. The window also re-reads once when it regains focus.
- Only when a watch cannot be established (no inotify, `/dev` or the mount table unwatchable) does a slow 30 s poll take over.
- Mounting or unmounting from Fileflux refreshes at once.
- For gvfs this is best effort: inotify only sees changes made through the kernel, so a mount gvfs adds without one appears at the latest when the window regains focus or after the 2 s and 5 s follow-up reads.

## Related

[User guide](user-guide.md) · [Architecture](architecture.md#threading-model)
