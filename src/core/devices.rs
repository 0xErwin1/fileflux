//! Block devices: discovery from `lsblk -J` and the mount, unmount and
//! eject actions through `udisksctl`.
//!
//! The parsers and planners are pure and run against canned fixtures. The
//! thin runner at the bottom starts a program by argv (never through a
//! shell), reads its output and enforces a timeout; it is meant to be called
//! off the UI thread. Nothing here talks to a device on its own: every
//! action is an explicit call with the device it is about.

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::child;

/// Why a device lookup or action failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceError {
    /// `lsblk` printed something that is not the JSON this module reads.
    Parse(String),
    /// The program could not be started.
    Spawn { tool: String, reason: String },
    /// The program ran past its timeout and was killed.
    TimedOut { tool: String, seconds: u64 },
    /// The program ran and failed; `reason` is the message it gave.
    Failed { reason: String },
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceError::Parse(reason) => write!(f, "could not read the device list: {reason}"),
            DeviceError::Spawn { tool, reason } => write!(f, "could not run {tool}: {reason}"),
            DeviceError::TimedOut { tool, seconds } => {
                write!(f, "{tool} did not answer within {seconds} s")
            }
            DeviceError::Failed { reason } => f.write_str(reason),
        }
    }
}

impl Error for DeviceError {}

/// One node of the `lsblk -J` tree: a disk, a partition or a mapped device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockNode {
    pub name: String,
    /// The device node, e.g. `/dev/sdb1`.
    pub path: PathBuf,
    pub label: Option<String>,
    pub fstype: Option<String>,
    /// Size in bytes (`lsblk -b`).
    pub size: u64,
    /// Real mount points; swap and null entries are not listed.
    pub mountpoints: Vec<PathBuf>,
    /// Whether the swap area is active on this node.
    pub swap_active: bool,
    pub removable: bool,
    pub hotplug: bool,
    /// `disk`, `part`, `crypt`, `loop`, …
    pub kind: String,
    /// The parent device's name, as `lsblk` reports it.
    pub parent: Option<String>,
    pub uuid: Option<String>,
    pub children: Vec<BlockNode>,
}

/// What can be done with a mountable device right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceState {
    Unmounted,
    Mounted(PathBuf),
    /// An encrypted container nobody unlocked: [`Device::can_unlock`].
    Locked,
    /// An audio CD: media without a filesystem, nothing to mount.
    Audio,
}

/// A filesystem the sidebar can show: mounted or not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    /// The device node, e.g. `/dev/sdb1`.
    pub path: PathBuf,
    /// The whole disk it lives on, e.g. `/dev/sdb`.
    pub disk: PathBuf,
    pub label: String,
    pub fstype: Option<String>,
    pub size: u64,
    pub state: DeviceState,
    /// Every mount point of the filesystem (a bind or subvolume mount adds
    /// one); the first is the one in [`DeviceState::Mounted`].
    pub mounts: Vec<PathBuf>,
    /// The disk is removable or hot-pluggable, and holds nothing of the
    /// running system: it can be powered off.
    pub ejectable: bool,
    /// The LUKS container this device was unlocked from (`/dev/sdb1` for
    /// `/dev/mapper/luks-…`): locked again after the device is unmounted.
    pub container: Option<PathBuf>,
    /// An optical drive with a disc in it: ejected by opening the tray, never
    /// powered off.
    pub optical: bool,
}

impl Device {
    /// The mount point, when mounted.
    pub fn mount_point(&self) -> Option<&Path> {
        match &self.state {
            DeviceState::Mounted(path) => Some(path),
            _ => None,
        }
    }

    /// Unmounted and unlocked: a click mounts it.
    pub fn can_mount(&self) -> bool {
        self.state == DeviceState::Unmounted
    }

    /// A locked LUKS container: `gio mount -d` unlocks and mounts it.
    pub fn can_unlock(&self) -> bool {
        self.state == DeviceState::Locked && self.fstype.as_deref() == Some(LUKS)
    }

    /// Mounted by the user's session (removable media), never a system mount.
    pub fn can_unmount(&self) -> bool {
        self.mount_point()
            .is_some_and(|mount| self.ejectable || under_media_root(mount))
    }

    /// The disk can be powered off.
    pub fn can_eject(&self) -> bool {
        self.ejectable
    }
}

/// One unmount step of a [`DevicePlan`]: `udisksctl` unmounts the device
/// wherever it is mounted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnmountStep {
    pub device: PathBuf,
    /// Every mount point the step takes away.
    pub mounts: Vec<PathBuf>,
    /// The tool that does it.
    pub via: UnmountVia,
}

/// The tool an [`UnmountStep`] runs through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnmountVia {
    /// `udisksctl unmount --block-device=<device>`.
    Udisks,
    /// `udisksctl lock --block-device=<container>`: locks the LUKS container
    /// again once nothing of it is mounted. The step has no mounts.
    UdisksLock,
    /// `gio mount -u <path>`: a gvfs mount (phone, share), whose `device`
    /// is the mount path itself.
    Gio,
}

impl UnmountStep {
    /// How deep the deepest of its mounts is.
    fn depth(&self) -> usize {
        self.mounts
            .iter()
            .map(|mount| mount.components().count())
            .max()
            .unwrap_or(0)
    }
}

/// What an unmount or eject does, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevicePlan {
    /// The name the user knows the device by.
    pub label: String,
    /// Unmounted first to last.
    pub unmount: Vec<UnmountStep>,
    /// Powered off after the last unmount (eject of a disk only).
    pub power_off: Option<PathBuf>,
    /// The optical drive whose tray opens after the last unmount (eject of a
    /// disc only, never together with `power_off`): `eject <device>`, or
    /// `gio mount -e <device>` without it.
    pub tray_eject: Option<PathBuf>,
}

impl DevicePlan {
    /// Whether this is an eject: the disk is powered off or the tray opens.
    pub fn is_eject(&self) -> bool {
        self.ejected().is_some()
    }

    /// The device an eject takes away.
    pub fn ejected(&self) -> Option<&Path> {
        self.power_off.as_deref().or(self.tray_eject.as_deref())
    }

    /// Every mount point the plan takes away.
    pub fn mounts(&self) -> Vec<PathBuf> {
        self.unmount
            .iter()
            .flat_map(|step| step.mounts.iter().cloned())
            .collect()
    }
}

/// What `lsblk` reports as the filesystem type of a LUKS container. (`TCRYPT`
/// volumes carry no signature `lsblk` can name, so they never show as locked.)
const LUKS: &str = "crypto_LUKS";

fn under_media_root(path: &Path) -> bool {
    path.starts_with("/run/media") || path.starts_with("/media")
}

// --- lsblk parsing ----------------------------------------------------------

/// The text under `key`, unless it is missing, not text or empty.
fn text(node: &Value, key: &str) -> Option<String> {
    node.get(key)?
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// The integer under `key`. Sizes are exact integers, and some `lsblk`
/// versions print them as strings anyway.
fn number(node: &Value, key: &str) -> Option<u64> {
    match node.get(key)? {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// A boolean column: `true`/`false` on current util-linux, `"1"`/`"0"`
/// strings on older versions.
fn flag(node: &Value, key: &str) -> bool {
    match node.get(key) {
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(number)) => number.as_u64() == Some(1),
        Some(Value::String(text)) => matches!(text.trim(), "1" | "true"),
        _ => false,
    }
}

fn node_from(json: &Value) -> Result<BlockNode, DeviceError> {
    if !json.is_object() {
        return Err(DeviceError::Parse("a device is not an object".to_owned()));
    }
    let name =
        text(json, "name").ok_or_else(|| DeviceError::Parse("a device has no name".to_owned()))?;
    let path = text(json, "path").map_or_else(|| Path::new("/dev").join(&name), PathBuf::from);
    // `mountpoints` (an array, null for none) on current util-linux, the
    // single `mountpoint` string on older versions.
    let raw: Vec<String> = match (json.get("mountpoints"), json.get("mountpoint")) {
        (Some(Value::Array(items)), _) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
        (_, Some(Value::String(mount))) => vec![mount.clone()],
        _ => Vec::new(),
    };
    let swap_active = raw.iter().any(|mount| mount == "[SWAP]");
    let mountpoints = raw
        .into_iter()
        .filter(|mount| mount.starts_with('/'))
        .map(PathBuf::from)
        .collect();
    let children = match json.get("children") {
        Some(Value::Array(items)) => items.iter().map(node_from).collect::<Result<_, _>>()?,
        _ => Vec::new(),
    };
    Ok(BlockNode {
        name,
        path,
        label: text(json, "label"),
        fstype: text(json, "fstype"),
        size: number(json, "size").unwrap_or(0),
        mountpoints,
        swap_active,
        removable: flag(json, "rm"),
        hotplug: flag(json, "hotplug"),
        kind: text(json, "type").unwrap_or_default(),
        parent: text(json, "pkname"),
        uuid: text(json, "uuid"),
        children,
    })
}

/// Parses `lsblk -J` output into the device tree.
pub fn parse_lsblk(json: &str) -> Result<Vec<BlockNode>, DeviceError> {
    let root: Value =
        serde_json::from_str(json).map_err(|error| DeviceError::Parse(error.to_string()))?;
    match root.get("blockdevices") {
        Some(Value::Array(items)) => items.iter().map(node_from).collect(),
        _ => Err(DeviceError::Parse(
            "there is no \"blockdevices\" list".to_owned(),
        )),
    }
}

// --- filtering and planning ---------------------------------------------------

/// Mount points of the running system. A partition with any of them below
/// it (or an active swap) is the system's: it is not offered, and its disk
/// cannot be ejected. The other partitions of that disk (a dual-boot data
/// or Windows partition) are listed and can be mounted.
const SYSTEM_MOUNTS: [&str; 10] = [
    "/boot", "/efi", "/usr", "/var", "/nix", "/etc", "/home", "/opt", "/swap", "/root",
];

fn is_system_mount(path: &Path) -> bool {
    path == Path::new("/") || SYSTEM_MOUNTS.iter().any(|root| path.starts_with(root))
}

fn is_system_itself(node: &BlockNode) -> bool {
    node.swap_active || node.mountpoints.iter().any(|mount| is_system_mount(mount))
}

fn holds_system(node: &BlockNode) -> bool {
    is_system_itself(node) || node.children.iter().any(holds_system)
}

/// Loop, RAM, zram and optical devices are not storage to manage here.
fn is_virtual(node: &BlockNode) -> bool {
    ["loop", "ram", "zram", "sr", "fd", "nbd"]
        .iter()
        .any(|prefix| node.name.starts_with(prefix))
        || matches!(node.kind.as_str(), "loop" | "rom")
}

/// An optical drive (`lsblk` type `rom`), with or without a disc.
fn is_optical(node: &BlockNode) -> bool {
    node.kind == "rom"
}

/// A disc in the drive: `lsblk` reports a size, or a filesystem, only then.
fn has_media(node: &BlockNode) -> bool {
    node.size > 0
        || node
            .fstype
            .as_deref()
            .is_some_and(|fstype| !fstype.is_empty())
}

/// The disc in an optical drive: a data disc (a filesystem such as iso9660
/// or udf) mounts like any volume; an audio CD (media, no filesystem) is
/// listed as [`DeviceState::Audio`]. An empty drive is not listed.
fn optical_device(node: &BlockNode, out: &mut Vec<Device>) {
    if !has_media(node) || holds_system(node) {
        return;
    }
    let fstype = node.fstype.as_deref().filter(|fstype| !fstype.is_empty());
    if fstype.is_some_and(is_not_a_filesystem) {
        return;
    }
    let label = node.label.clone().filter(|label| !label.trim().is_empty());
    let (state, label) = match fstype {
        Some(_) => (
            node.mountpoints
                .first()
                .map_or(DeviceState::Unmounted, |mount| {
                    DeviceState::Mounted(mount.clone())
                }),
            label.unwrap_or_else(|| "Disc".to_owned()),
        ),
        None => (
            DeviceState::Audio,
            label.unwrap_or_else(|| "Audio CD".to_owned()),
        ),
    };
    out.push(Device {
        path: node.path.clone(),
        disk: node.path.clone(),
        label,
        fstype: fstype.map(str::to_owned),
        size: node.size,
        state,
        mounts: node.mountpoints.clone(),
        ejectable: true,
        container: None,
        optical: true,
    });
}

fn is_ejectable_disk(disk: &BlockNode) -> bool {
    disk.removable || disk.hotplug
}

/// Filesystem types that are containers or swap, never mounted directly.
fn is_not_a_filesystem(fstype: &str) -> bool {
    matches!(
        fstype,
        "swap"
            | "LVM2_member"
            | "linux_raid_member"
            | "ddf_raid_member"
            | "isw_raid_member"
            | "zfs_member"
            | "bcache"
            | "ceph_bluestore"
            | "drbd"
    )
}

fn size_label(size: u64) -> String {
    if size == 0 {
        "Volume".to_owned()
    } else {
        format!("{} Volume", super::places::format_bytes(size))
    }
}

fn collect_devices(
    node: &BlockNode,
    disk: &BlockNode,
    container: Option<&Path>,
    out: &mut Vec<Device>,
) {
    // A partition that holds the running system, and the container (LUKS)
    // above it, are not offered; its siblings still are. The disk itself is
    // only a parent here: it is skipped when it is a system filesystem.
    let is_disk = std::ptr::eq(node, disk);
    if is_system_itself(node) || (!is_disk && holds_system(node)) {
        return;
    }
    if !node.children.is_empty() {
        // A container with something below it (partitions, an unlocked
        // volume) is not itself a filesystem.
        let below = (node.fstype.as_deref() == Some(LUKS)).then_some(node.path.as_path());
        for child in &node.children {
            collect_devices(child, disk, below, out);
        }
        return;
    }
    if !matches!(node.kind.as_str(), "part" | "disk" | "crypt" | "lvm") {
        return;
    }
    let Some(fstype) = node.fstype.as_deref().filter(|fstype| !fstype.is_empty()) else {
        return;
    };
    if is_not_a_filesystem(fstype) {
        return;
    }
    let state = if fstype == LUKS {
        DeviceState::Locked
    } else {
        node.mountpoints
            .first()
            .map_or(DeviceState::Unmounted, |mount| {
                DeviceState::Mounted(mount.clone())
            })
    };
    out.push(Device {
        path: node.path.clone(),
        disk: disk.path.clone(),
        label: node
            .label
            .clone()
            .filter(|label| !label.trim().is_empty())
            .unwrap_or_else(|| size_label(node.size)),
        fstype: Some(fstype.to_owned()),
        size: node.size,
        state,
        mounts: node.mountpoints.clone(),
        ejectable: !holds_system(disk)
            && (is_ejectable_disk(disk) || node.removable || node.hotplug),
        container: container.map(Path::to_path_buf),
        optical: false,
    });
}

/// The mountable filesystems, in tree order: mounted and unmounted
/// partitions, and encrypted containers as [`DeviceState::Locked`]. Swap,
/// RAID/LVM members, loop and zram devices, empty readers and the partitions
/// that hold a system mount or swap are left out; the rest of a system disk
/// stays listed, mountable but never ejectable.
pub fn devices(nodes: &[BlockNode]) -> Vec<Device> {
    let mut out = Vec::new();
    for disk in nodes {
        if is_optical(disk) {
            optical_device(disk, &mut out);
            continue;
        }
        if is_virtual(disk) {
            continue;
        }
        collect_devices(disk, disk, None, &mut out);
    }
    out
}

/// The device mounted at `mount`.
pub fn device_for_mount<'a>(devices: &'a [Device], mount: &Path) -> Option<&'a Device> {
    devices
        .iter()
        .find(|device| device.mounts.iter().any(|at| at == mount))
}

/// Unmounting just `device`; `None` when that is not the session's to do.
pub fn unmount_plan(device: &Device) -> Option<DevicePlan> {
    let mount = device.mount_point().filter(|_| device.can_unmount())?;
    let mut unmount = vec![UnmountStep {
        device: device.path.clone(),
        mounts: if device.mounts.is_empty() {
            vec![mount.to_path_buf()]
        } else {
            device.mounts.clone()
        },
        via: UnmountVia::Udisks,
    }];
    // An unlocked volume goes back to locked: unmounting alone would leave
    // the container open.
    if let Some(container) = &device.container {
        unmount.push(lock_step(container));
    }
    Some(DevicePlan {
        label: device.label.clone(),
        unmount,
        power_off: None,
        tray_eject: None,
    })
}

fn lock_step(container: &Path) -> UnmountStep {
    UnmountStep {
        device: container.to_path_buf(),
        mounts: Vec::new(),
        via: UnmountVia::UdisksLock,
    }
}

/// The unlocked LUKS containers at or below `node`: they have a mapped
/// child.
fn unlocked_below(node: &BlockNode, out: &mut Vec<UnmountStep>) {
    // Inside out: a container cannot be locked while one is open in it.
    for child in &node.children {
        unlocked_below(child, out);
    }
    if node.fstype.as_deref() == Some(LUKS)
        && node.children.iter().any(|child| child.kind == "crypt")
    {
        out.push(lock_step(&node.path));
    }
}

fn mounted_below(node: &BlockNode, out: &mut Vec<UnmountStep>) {
    if !node.mountpoints.is_empty() {
        out.push(UnmountStep {
            device: node.path.clone(),
            mounts: node.mountpoints.clone(),
            via: UnmountVia::Udisks,
        });
    }
    for child in &node.children {
        mounted_below(child, out);
    }
}

/// Unmounting every mounted filesystem of `disk`, deepest mount first, then
/// powering it off. `None` for anything that is not a removable,
/// non-system, non-virtual whole disk in `nodes` that holds at least one
/// filesystem (an empty card reader has nothing to eject).
pub fn eject_plan(nodes: &[BlockNode], disk: &Path, label: &str) -> Option<DevicePlan> {
    let node = nodes.iter().find(|node| node.path == disk)?;
    if is_optical(node) {
        return disc_eject_plan(node, label);
    }
    if is_virtual(node) || holds_system(node) || !is_ejectable_disk(node) {
        return None;
    }
    let mut filesystems = Vec::new();
    collect_devices(node, node, None, &mut filesystems);
    if filesystems.is_empty() {
        return None;
    }
    let mut unmount = Vec::new();
    mounted_below(node, &mut unmount);
    // Stable: equally deep mounts keep their tree order.
    unmount.sort_by_key(|step| std::cmp::Reverse(step.depth()));
    // Everything is unmounted by then: the containers lock, then the disk
    // powers off.
    unlocked_below(node, &mut unmount);
    Some(DevicePlan {
        label: label.to_owned(),
        unmount,
        power_off: Some(node.path.clone()),
        tray_eject: None,
    })
}

/// Unmounting a disc, then opening the tray. Never a power-off. `None` for
/// an empty drive or one that holds a system mount.
fn disc_eject_plan(node: &BlockNode, label: &str) -> Option<DevicePlan> {
    if !has_media(node) || holds_system(node) {
        return None;
    }
    let mut unmount = Vec::new();
    mounted_below(node, &mut unmount);
    Some(DevicePlan {
        label: label.to_owned(),
        unmount,
        power_off: None,
        tray_eject: Some(node.path.clone()),
    })
}

/// Whether the text of `path` lies below one of the `mounts`. Symlinks are
/// not looked through: [`path_is_on_mounts`] does that.
pub fn path_uses_mounts(path: &Path, mounts: &[PathBuf]) -> bool {
    mounts.iter().any(|mount| path.starts_with(mount))
}

/// Where `path` really is: its nearest existing ancestor with symlinks and
/// `..` resolved, followed by the part that does not exist yet (a paste
/// destination, a folder about to be created). Reads the filesystem, so it
/// may block on a hung device: call it off the UI thread.
pub fn real_location(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        let Ok(mut real) = std::fs::canonicalize(ancestor) else {
            continue;
        };
        let rest = path.strip_prefix(ancestor).unwrap_or(Path::new(""));
        for part in rest.components() {
            match part {
                std::path::Component::ParentDir => {
                    real.pop();
                }
                std::path::Component::Normal(name) => real.push(name),
                _ => {}
            }
        }
        return real;
    }
    path.to_path_buf()
}

/// Whether `path` is on one of the `mounts` by real location: through
/// symlinks, `..` and a destination that does not exist yet, not only by its
/// text. Reads the filesystem: call it off the UI thread.
pub fn path_is_on_mounts(path: &Path, mounts: &[PathBuf]) -> bool {
    if path_uses_mounts(path, mounts) {
        return true;
    }
    let real = real_location(path);
    mounts
        .iter()
        .any(|mount| real.starts_with(real_location(mount)))
}

/// The folder a tab on `location` moves to before `mounts` go away: the
/// nearest existing ancestor outside every mount (by real location, so an
/// ancestor that is a symlink into the device is skipped), else `home`, else
/// `/`. Reads the filesystem: call it off the UI thread.
pub fn fallback_target(
    location: &Path,
    mounts: &[PathBuf],
    home: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
) -> PathBuf {
    location
        .ancestors()
        .find(|ancestor| !path_is_on_mounts(ancestor, mounts) && exists(ancestor))
        .map(|ancestor| {
            // A shown `…/a/..` is the folder above `a`: name it as such.
            if ancestor
                .components()
                .any(|part| part == std::path::Component::ParentDir)
            {
                real_location(ancestor)
            } else {
                ancestor.to_path_buf()
            }
        })
        .or_else(|| home.map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("/"))
}

// --- unlocking ----------------------------------------------------------------

/// Why a device is not offered to `gio mount -d`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnlockError {
    /// Not an absolute path below `/dev` without `..`.
    NotADevicePath,
    /// The name starts with `-`: `gio` could read it as an option.
    LeadingDash,
    /// Not a locked LUKS container of the current listing.
    NotLocked,
}

impl fmt::Display for UnlockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            UnlockError::NotADevicePath => "that is not a device under /dev",
            UnlockError::LeadingDash => "that device name starts with a dash",
            UnlockError::NotLocked => "that is not a locked encrypted volume",
        })
    }
}

impl Error for UnlockError {}

/// A locked LUKS container that passed [`unlock_target`]: the only thing
/// `gio mount -d` is ever started with. Its fields are private, so no typed
/// string reaches `gio`'s argv.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnlockTarget {
    path: PathBuf,
    label: String,
}

impl UnlockTarget {
    /// The device node, e.g. `/dev/sdb1`, as the listing reported it.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The name the user knows the volume by.
    pub fn label(&self) -> &str {
        &self.label
    }
}

/// `requested` as an unlock target: an absolute `/dev/…` path (no `..`, no
/// name starting with `-`) that is a locked LUKS container of `nodes`, the
/// current listing. The path kept is the listing's, never the argument.
///
/// # Errors
///
/// [`UnlockError`] with the reason the device is refused.
pub fn unlock_target(nodes: &[BlockNode], requested: &Path) -> Result<UnlockTarget, UnlockError> {
    use std::path::Component;
    let mut parts = requested.components();
    let shaped = parts.next() == Some(Component::RootDir)
        && parts.next() == Some(Component::Normal(OsStr::new("dev")))
        && parts.clone().next().is_some()
        && parts.all(|part| matches!(part, Component::Normal(_)));
    // `/dev/sde1/`, `//dev/sde1`, `/dev/./sde1` normalize to a listed path
    // by component: only the exact text is accepted.
    if !shaped || requested.components().collect::<PathBuf>().as_os_str() != requested.as_os_str() {
        return Err(UnlockError::NotADevicePath);
    }
    if requested
        .components()
        .any(|part| part.as_os_str().as_encoded_bytes().starts_with(b"-"))
    {
        return Err(UnlockError::LeadingDash);
    }
    let found = devices(nodes)
        .into_iter()
        .find(|device| device.path == requested && device.can_unlock())
        .ok_or(UnlockError::NotLocked)?;
    Ok(UnlockTarget {
        path: found.path,
        label: found.label,
    })
}

/// `gio mount -d <device>`: `-d` takes the device as its value, so it is
/// never read as an option of its own.
pub fn unlock_args(target: &UnlockTarget) -> Vec<OsString> {
    vec![
        OsString::from("mount"),
        OsString::from("-d"),
        target.path.as_os_str().to_owned(),
    ]
}

fn first_mount(node: &BlockNode) -> Option<PathBuf> {
    node.mountpoints
        .first()
        .cloned()
        .or_else(|| node.children.iter().find_map(first_mount))
}

fn find_node<'a>(nodes: &'a [BlockNode], path: &Path) -> Option<&'a BlockNode> {
    nodes.iter().find_map(|node| {
        if node.path == path {
            Some(node)
        } else {
            find_node(&node.children, path)
        }
    })
}

/// Whether `container` shows a mapped (`crypt`) child in `nodes`: it is
/// unlocked, mounted or not.
pub fn is_unlocked(nodes: &[BlockNode], container: &Path) -> bool {
    find_node(nodes, container)
        .is_some_and(|node| node.children.iter().any(|child| child.kind == "crypt"))
}

/// Where the unlocked volume of the container `container` is mounted in
/// `nodes`: the first mount point of the container's children.
pub fn unlocked_mount(nodes: &[BlockNode], container: &Path) -> Option<PathBuf> {
    find_node(nodes, container)?
        .children
        .iter()
        .find_map(first_mount)
}

/// Lists the devices up to `attempts` times, `delay` apart, until the
/// unlocked volume of `container` is mounted: the mapper can trail gio's
/// exit by a moment. Answers that listing and the mount point.
pub fn wait_for_unlocked_mount(
    mut list: impl FnMut() -> Result<Vec<BlockNode>, DeviceError>,
    container: &Path,
    attempts: u32,
    delay: Duration,
) -> Option<(Vec<BlockNode>, PathBuf)> {
    for attempt in 0..attempts {
        if attempt > 0 {
            thread::sleep(delay);
        }
        if let Ok(nodes) = list()
            && let Some(mount) = unlocked_mount(&nodes, container)
        {
            return Some((nodes, mount));
        }
    }
    None
}

// --- udisksctl ----------------------------------------------------------------

/// `<verb> --block-device=<path> --no-user-interaction`: the path is glued
/// to the option, so a name that starts with a dash is never read as one.
fn udisks_args(verb: &str, device: &Path) -> Vec<OsString> {
    let mut block_device = OsString::from("--block-device=");
    block_device.push(device);
    vec![
        OsString::from(verb),
        block_device,
        OsString::from("--no-user-interaction"),
    ]
}

/// `udisksctl mount --block-device=<device> --no-user-interaction`.
pub fn mount_args(device: &Path) -> Vec<OsString> {
    udisks_args("mount", device)
}

/// `udisksctl unmount --block-device=<device> --no-user-interaction`.
pub fn unmount_args(device: &Path) -> Vec<OsString> {
    udisks_args("unmount", device)
}

/// `udisksctl lock --block-device=<container> --no-user-interaction`.
pub fn lock_args(container: &Path) -> Vec<OsString> {
    udisks_args("lock", container)
}

/// `udisksctl power-off --block-device=<disk> --no-user-interaction`.
pub fn power_off_args(disk: &Path) -> Vec<OsString> {
    udisks_args("power-off", disk)
}

/// The tool that opens a drive's tray, and its argv: `eject <device>`
/// (util-linux) when installed, else `gio mount -e <device>`. The device path
/// is absolute, so it is never read as an option.
pub fn tray_eject_command(
    eject: Option<&Path>,
    gio: Option<&Path>,
    device: &Path,
) -> Result<(PathBuf, Vec<OsString>), DeviceError> {
    if let Some(eject) = eject {
        return Ok((eject.to_path_buf(), vec![device.as_os_str().to_owned()]));
    }
    match gio {
        Some(gio) => Ok((
            gio.to_path_buf(),
            vec![
                OsString::from("mount"),
                OsString::from("-e"),
                device.as_os_str().to_owned(),
            ],
        )),
        None => Err(DeviceError::Failed {
            reason: "neither eject nor gio was found".to_owned(),
        }),
    }
}

/// The mount point in a successful `udisksctl mount` output
/// (`Mounted /dev/sdb1 at /run/media/user/LABEL`).
pub fn parse_mount_output(stdout: &str) -> Option<PathBuf> {
    stdout.lines().find_map(|line| {
        let rest = line.strip_prefix("Mounted ")?;
        let (_, at) = rest.split_once(" at ")?;
        let path = PathBuf::from(at.trim_end_matches('\r'));
        path.is_absolute().then_some(path)
    })
}

/// The mount point named by an "already mounted" failure
/// (``is already mounted at `/run/media/user/LABEL'``).
pub fn already_mounted_at(stderr: &str) -> Option<PathBuf> {
    let (_, rest) = stderr.split_once("already mounted at `")?;
    let (path, _) = rest.split_once('\'')?;
    Some(PathBuf::from(path))
}

/// Older `udisksctl` ended the mount line with a period: when `candidate`
/// does not exist but the text without it does, that is the mount point.
pub fn settle_mount_point(candidate: PathBuf, exists: impl Fn(&Path) -> bool) -> PathBuf {
    if exists(&candidate) {
        return candidate;
    }
    match candidate.to_str().and_then(|text| text.strip_suffix('.')) {
        Some(stripped) if exists(Path::new(stripped)) => PathBuf::from(stripped),
        _ => candidate,
    }
}

/// The human reason in a failed `udisksctl` run: the first message line,
/// without the D-Bus error name (`GDBus.Error:org.freedesktop.…:`). A bare
/// error name stands for itself, shortened to its last segment.
pub fn failure_reason(stderr: &str) -> String {
    let Some(line) = stderr.lines().map(str::trim).find(|line| !line.is_empty()) else {
        return "udisksctl failed without a message".to_owned();
    };
    let Some((_, rest)) = line.split_once("GDBus.Error:") else {
        return line.to_owned();
    };
    let (name, message) = rest.split_once(':').unwrap_or((rest, ""));
    let message = message.trim();
    if message.is_empty() {
        name.rsplit('.').next().unwrap_or(name).trim().to_owned()
    } else {
        message.to_owned()
    }
}

// --- running programs ---------------------------------------------------------

/// How long `lsblk` may take.
pub const LSBLK_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one `udisksctl` call may take.
pub const UDISKS_TIMEOUT: Duration = Duration::from_secs(30);

/// What a finished program printed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// The first executable called `name` in `path_var`; relative entries are
/// ignored.
pub fn find_tool(name: &str, path_var: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path_var?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

fn tool_name(program: &Path) -> String {
    program
        .file_name()
        .unwrap_or(program.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn read_all(mut pipe: impl Read) -> String {
    let mut bytes = Vec::new();
    // A pipe that breaks mid-read leaves what was read so far.
    if let Err(error) = pipe.read_to_end(&mut bytes) {
        log::debug!("a tool's output pipe failed: {error}");
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Runs `program` with `args` (no shell, no input, `LC_ALL=C`), killing it
/// after `timeout`.
pub fn run_tool(
    program: &Path,
    args: &[OsString],
    timeout: Duration,
) -> Result<ToolOutput, DeviceError> {
    let tool = tool_name(program);
    let mut child = child::command(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| DeviceError::Spawn {
            tool: tool.clone(),
            reason: error.to_string(),
        })?;
    let stdout = child
        .stdout
        .take()
        .map(|pipe| thread::spawn(move || read_all(pipe)));
    let stderr = child
        .stderr
        .take()
        .map(|pipe| thread::spawn(move || read_all(pipe)));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                // The readers are left to end by themselves: a grandchild
                // may still hold the pipes open.
                child.kill().ok();
                child.wait().ok();
                return Err(DeviceError::TimedOut {
                    tool,
                    seconds: timeout.as_secs().max(1),
                });
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                child.kill().ok();
                return Err(DeviceError::Spawn {
                    tool,
                    reason: error.to_string(),
                });
            }
        }
    };
    let collect = |handle: Option<thread::JoinHandle<String>>| {
        handle
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default()
    };
    Ok(ToolOutput {
        success: status.success(),
        stdout: collect(stdout),
        stderr: collect(stderr),
    })
}

const LSBLK_COLUMNS: &str = "NAME,PATH,LABEL,FSTYPE,SIZE,MOUNTPOINTS,RM,HOTPLUG,TYPE,PKNAME,UUID";
/// Before util-linux 2.37 the column is the singular `MOUNTPOINT`.
const LSBLK_COLUMNS_OLD: &str =
    "NAME,PATH,LABEL,FSTYPE,SIZE,MOUNTPOINT,RM,HOTPLUG,TYPE,PKNAME,UUID";

fn lsblk_args(columns: &str) -> Vec<OsString> {
    ["-J", "-b", "-o", columns]
        .iter()
        .map(OsString::from)
        .collect()
}

/// Lists the block devices with `lsblk` (read-only).
pub fn list_blocks(lsblk: &Path) -> Result<Vec<BlockNode>, DeviceError> {
    let mut output = run_tool(lsblk, &lsblk_args(LSBLK_COLUMNS), LSBLK_TIMEOUT)?;
    if !output.success && output.stderr.contains("unknown column") {
        output = run_tool(lsblk, &lsblk_args(LSBLK_COLUMNS_OLD), LSBLK_TIMEOUT)?;
    }
    if !output.success {
        let reason = failure_reason(&output.stderr);
        return Err(DeviceError::Failed {
            reason: if output.stderr.trim().is_empty() {
                "lsblk failed without a message".to_owned()
            } else {
                reason
            },
        });
    }
    parse_lsblk(&output.stdout)
}

/// Mounts `device` and answers where it is mounted. A device that is
/// already mounted answers its mount point.
pub fn mount(udisksctl: &Path, device: &Path) -> Result<PathBuf, DeviceError> {
    let output = run_tool(udisksctl, &mount_args(device), UDISKS_TIMEOUT)?;
    if output.success {
        return parse_mount_output(&output.stdout)
            .map(|path| settle_mount_point(path, Path::exists))
            .ok_or_else(|| DeviceError::Failed {
                reason: "udisksctl did not say where it mounted the device".to_owned(),
            });
    }
    match already_mounted_at(&output.stderr) {
        Some(path) => Ok(path),
        None => Err(DeviceError::Failed {
            reason: failure_reason(&output.stderr),
        }),
    }
}

/// `gio mount -u <path>`: unmounts a gvfs mount. The path is absolute, so it
/// is never read as an option.
pub fn gio_unmount_args(path: &Path) -> Vec<OsString> {
    vec![
        OsString::from("mount"),
        OsString::from("-u"),
        path.as_os_str().to_owned(),
    ]
}

/// Unmounting the gvfs mount at `mount` through `gio`.
pub fn gio_unmount_plan(label: &str, mount: &Path) -> DevicePlan {
    DevicePlan {
        label: label.to_owned(),
        unmount: vec![UnmountStep {
            device: mount.to_path_buf(),
            mounts: vec![mount.to_path_buf()],
            via: UnmountVia::Gio,
        }],
        power_off: None,
        tray_eject: None,
    }
}

/// The program that runs `step`: `udisksctl` or `gio`, whichever the step
/// goes through; an error when that one is not installed.
pub fn unmount_program<'a>(
    step: &UnmountStep,
    udisksctl: Option<&'a Path>,
    gio: Option<&'a Path>,
) -> Result<&'a Path, DeviceError> {
    let (program, name) = match step.via {
        UnmountVia::Udisks | UnmountVia::UdisksLock => (udisksctl, "udisksctl"),
        UnmountVia::Gio => (gio, "gio"),
    };
    program.ok_or_else(|| DeviceError::Failed {
        reason: format!("{name} was not found"),
    })
}

/// Unmounts one device. A device that is already unmounted counts as done.
/// `program` is the one [`unmount_program`] chose for the step.
pub fn run_unmount(program: &Path, step: &UnmountStep) -> Result<(), DeviceError> {
    let args = match step.via {
        UnmountVia::Udisks => unmount_args(&step.device),
        UnmountVia::UdisksLock => lock_args(&step.device),
        UnmountVia::Gio => gio_unmount_args(&step.device),
    };
    let output = run_tool(program, &args, UDISKS_TIMEOUT)?;
    let already = output.stderr.contains("NotMounted")
        || output.stderr.contains("is not mounted")
        || output
            .stderr
            .contains("Containing mount for file not found")
        || (step.via == UnmountVia::UdisksLock
            && output.stderr.to_lowercase().contains("not unlocked"));
    if output.success || already {
        Ok(())
    } else {
        Err(DeviceError::Failed {
            reason: match step.via {
                UnmountVia::Udisks | UnmountVia::UdisksLock => failure_reason(&output.stderr),
                UnmountVia::Gio => output
                    .stderr
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .unwrap_or("gio failed without a message")
                    .to_owned(),
            },
        })
    }
}

/// Powers `disk` off.
pub fn run_power_off(udisksctl: &Path, disk: &Path) -> Result<(), DeviceError> {
    let output = run_tool(udisksctl, &power_off_args(disk), UDISKS_TIMEOUT)?;
    if output.success {
        Ok(())
    } else {
        Err(DeviceError::Failed {
            reason: failure_reason(&output.stderr),
        })
    }
}

/// Opens the tray of `device` through [`tray_eject_command`].
pub fn run_tray_eject(
    eject: Option<&Path>,
    gio: Option<&Path>,
    device: &Path,
) -> Result<(), DeviceError> {
    let (program, args) = tray_eject_command(eject, gio, device)?;
    let output = run_tool(&program, &args, UDISKS_TIMEOUT)?;
    if output.success {
        return Ok(());
    }
    Err(DeviceError::Failed {
        reason: output
            .stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("the eject tool failed without a message")
            .to_owned(),
    })
}

/// Runs `plan`: its unmounts in order, then the power-off. The first
/// failure stops the plan. A disc's tray eject needs [`run_plan_with`].
pub fn run_plan(udisksctl: &Path, plan: &DevicePlan) -> Result<(), DeviceError> {
    run_plan_with(Some(udisksctl), None, None, plan)
}

/// Runs `plan` with every tool it may need: each unmount step through its
/// own tool, then the power-off or the tray eject. The first failure stops
/// the plan.
pub fn run_plan_with(
    udisksctl: Option<&Path>,
    gio: Option<&Path>,
    eject: Option<&Path>,
    plan: &DevicePlan,
) -> Result<(), DeviceError> {
    for step in &plan.unmount {
        run_unmount(unmount_program(step, udisksctl, gio)?, step)?;
    }
    if let Some(disk) = &plan.power_off {
        let udisksctl = udisksctl.ok_or_else(|| DeviceError::Failed {
            reason: "udisksctl was not found".to_owned(),
        })?;
        run_power_off(udisksctl, disk)?;
    }
    if let Some(device) = &plan.tray_eject {
        run_tray_eject(eject, gio, device)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    type TestResult = Result<(), Box<dyn Error>>;

    /// A USB stick (two partitions, one mounted, one not), an internal NVMe
    /// with its system partitions and an unlocked LUKS root, a locked LUKS
    /// stick, a second internal disk with a swap and an NTFS partition, an
    /// empty card reader, a zram swap and a loop mount.
    const FIXTURE: &str = r#"{
   "blockdevices": [
      {"name":"sda","path":"/dev/sda","label":null,"fstype":null,"size":0,"mountpoints":[null],"rm":true,"hotplug":true,"type":"disk","pkname":null,"uuid":null},
      {"name":"sdb","path":"/dev/sdb","label":null,"fstype":null,"size":31914983424,"mountpoints":[null],"rm":true,"hotplug":true,"type":"disk","pkname":null,"uuid":null,
         "children":[
            {"name":"sdb1","path":"/dev/sdb1","label":"KINGSTON","fstype":"vfat","size":15000000000,"mountpoints":["/run/media/iperez/KINGSTON"],"rm":true,"hotplug":true,"type":"part","pkname":"sdb","uuid":"AAAA-1111"},
            {"name":"sdb2","path":"/dev/sdb2","label":null,"fstype":"ext4","size":16000000000,"mountpoints":[null],"rm":true,"hotplug":true,"type":"part","pkname":"sdb","uuid":"2f4c-ext4"}
         ]},
      {"name":"sdc","path":"/dev/sdc","label":null,"fstype":null,"size":8000000000,"mountpoints":[null],"rm":true,"hotplug":true,"type":"disk","pkname":null,"uuid":null,
         "children":[
            {"name":"sdc1","path":"/dev/sdc1","label":null,"fstype":"crypto_LUKS","size":7999000000,"mountpoints":[null],"rm":true,"hotplug":true,"type":"part","pkname":"sdc","uuid":"luks-uuid"}
         ]},
      {"name":"sdd","path":"/dev/sdd","label":null,"fstype":null,"size":500107862016,"mountpoints":[null],"rm":false,"hotplug":false,"type":"disk","pkname":null,"uuid":null,
         "children":[
            {"name":"sdd1","path":"/dev/sdd1","label":null,"fstype":"swap","size":8000000000,"mountpoints":[null],"rm":false,"hotplug":false,"type":"part","pkname":"sdd","uuid":"swap-uuid"},
            {"name":"sdd2","path":"/dev/sdd2","label":"Data","fstype":"ntfs","size":492000000000,"mountpoints":[null],"rm":false,"hotplug":false,"type":"part","pkname":"sdd","uuid":"ntfs-uuid"}
         ]},
      {"name":"zram0","path":"/dev/zram0","label":"zram0","fstype":"swap","size":6652379136,"mountpoints":["[SWAP]"],"rm":false,"hotplug":false,"type":"disk","pkname":null,"uuid":"zram-uuid"},
      {"name":"loop0","path":"/dev/loop0","label":null,"fstype":"squashfs","size":4096000,"mountpoints":["/snap/core/1"],"rm":false,"hotplug":false,"type":"loop","pkname":null,"uuid":null},
      {"name":"nvme0n1","path":"/dev/nvme0n1","label":null,"fstype":null,"size":1000204886016,"mountpoints":[null],"rm":false,"hotplug":false,"type":"disk","pkname":null,"uuid":null,
         "children":[
            {"name":"nvme0n1p1","path":"/dev/nvme0n1p1","label":null,"fstype":"vfat","size":1073741824,"mountpoints":["/boot"],"rm":false,"hotplug":false,"type":"part","pkname":"nvme0n1","uuid":"5990-8457"},
            {"name":"nvme0n1p2","path":"/dev/nvme0n1p2","label":null,"fstype":"crypto_LUKS","size":999128301568,"mountpoints":[null],"rm":false,"hotplug":false,"type":"part","pkname":"nvme0n1","uuid":"a960b1d8",
               "children":[
                  {"name":"root","path":"/dev/mapper/root","label":null,"fstype":"btrfs","size":999111524352,"mountpoints":["/swap","/home","/nix/store","/"],"rm":false,"hotplug":false,"type":"crypt","pkname":"nvme0n1p2","uuid":"root-uuid"}
               ]},
            {"name":"nvme0n1p3","path":"/dev/nvme0n1p3","label":null,"fstype":"ntfs","size":500000000,"mountpoints":[null],"rm":false,"hotplug":false,"type":"part","pkname":"nvme0n1","uuid":"recovery"}
         ]}
   ]
}"#;

    fn parsed() -> Vec<BlockNode> {
        parse_lsblk(FIXTURE).unwrap_or_default()
    }

    fn paths(devices: &[Device]) -> Vec<&str> {
        devices
            .iter()
            .filter_map(|device| device.path.to_str())
            .collect()
    }

    fn find<'a>(devices: &'a [Device], path: &str) -> Option<&'a Device> {
        devices.iter().find(|device| device.path == Path::new(path))
    }

    // --- lsblk parsing ------------------------------------------------------

    #[test]
    fn lsblk_json_becomes_a_tree_with_real_mount_points_only() -> TestResult {
        let nodes = parse_lsblk(FIXTURE)?;
        assert_eq!(
            nodes.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
            ["sda", "sdb", "sdc", "sdd", "zram0", "loop0", "nvme0n1"]
        );
        let sdb = nodes.get(1).ok_or("sdb missing")?;
        assert_eq!(sdb.size, 31_914_983_424);
        assert!(sdb.removable && sdb.hotplug);
        assert_eq!(sdb.kind, "disk");
        assert!(sdb.mountpoints.is_empty(), "null entries are not mounts");
        let sdb1 = sdb.children.first().ok_or("sdb1 missing")?;
        assert_eq!(sdb1.path, PathBuf::from("/dev/sdb1"));
        assert_eq!(sdb1.label.as_deref(), Some("KINGSTON"));
        assert_eq!(sdb1.fstype.as_deref(), Some("vfat"));
        assert_eq!(sdb1.parent.as_deref(), Some("sdb"));
        assert_eq!(sdb1.uuid.as_deref(), Some("AAAA-1111"));
        assert_eq!(
            sdb1.mountpoints,
            [PathBuf::from("/run/media/iperez/KINGSTON")]
        );
        let zram = nodes.get(4).ok_or("zram missing")?;
        assert!(zram.mountpoints.is_empty(), "[SWAP] is not a mount point");
        assert!(zram.swap_active);
        Ok(())
    }

    /// Older util-linux prints every value as a string and has a single
    /// `mountpoint` key.
    #[test]
    fn the_old_all_strings_format_parses_the_same() -> TestResult {
        let old = r#"{"blockdevices":[{"name":"sdb1","path":"/dev/sdb1","label":"OLD","fstype":"ext4","size":"1000","mountpoint":"/media/x","rm":"1","hotplug":"0","type":"part","pkname":null,"uuid":"u"}]}"#;
        let nodes = parse_lsblk(old)?;
        let node = nodes.first().ok_or("node missing")?;
        assert_eq!(node.size, 1000);
        assert!(node.removable);
        assert!(!node.hotplug);
        assert_eq!(node.mountpoints, [PathBuf::from("/media/x")]);
        Ok(())
    }

    #[test]
    fn broken_json_is_an_error_not_an_empty_list() {
        for bad in [
            "",
            "{",
            "[]",
            r#"{"blockdevices": 3}"#,
            "not json",
            "{} trailing",
        ] {
            assert!(
                matches!(parse_lsblk(bad), Err(DeviceError::Parse(_))),
                "{bad:?}"
            );
        }
        assert_eq!(parse_lsblk(r#"{"blockdevices": []}"#), Ok(Vec::new()));
    }

    #[test]
    fn escapes_and_unicode_in_labels_survive() -> TestResult {
        let json = r#"{"blockdevices":[{"name":"sdb1","path":"/dev/sdb1","label":"A \"q\" \u00e9\u4e2d \ud83d\ude00\\","fstype":"ext4","size":1,"mountpoints":[],"rm":true,"hotplug":true,"type":"part","pkname":null,"uuid":null}]}"#;
        let nodes = parse_lsblk(json)?;
        assert_eq!(
            nodes.first().and_then(|n| n.label.as_deref()),
            Some("A \"q\" é中 😀\\")
        );
        Ok(())
    }

    #[test]
    fn two_high_surrogates_never_panic() {
        let json = r#"{"blockdevices":[{"name":"sdb1","label":"\ud83d\ud83d","type":"part"}]}"#;
        assert!(std::panic::catch_unwind(|| parse_lsblk(json)).is_ok());
    }

    // --- filtering ----------------------------------------------------------

    #[test]
    fn the_usb_stick_shows_both_partitions_mounted_and_not() {
        let found = devices(&parsed());
        let mounted = find(&found, "/dev/sdb1");
        let free = find(&found, "/dev/sdb2");
        assert_eq!(
            mounted.map(|d| (d.label.as_str(), d.state.clone(), d.ejectable)),
            Some((
                "KINGSTON",
                DeviceState::Mounted(PathBuf::from("/run/media/iperez/KINGSTON")),
                true
            ))
        );
        assert_eq!(
            free.map(|d| (d.label.as_str(), d.state.clone(), d.disk.clone())),
            Some((
                "16 GB Volume",
                DeviceState::Unmounted,
                PathBuf::from("/dev/sdb")
            ))
        );
        assert!(free.is_some_and(Device::can_mount));
        assert!(mounted.is_some_and(Device::can_unmount));
        assert!(mounted.is_some_and(Device::can_eject));
    }

    #[test]
    fn system_partitions_swap_loop_zram_and_empty_readers_are_left_out() {
        let found = devices(&parsed());
        // The system partitions of nvme0n1 (/boot, the unlocked root and its
        // LUKS parent) are hidden; its recovery partition is not.
        assert_eq!(
            paths(&found),
            [
                "/dev/sdb1",
                "/dev/sdb2",
                "/dev/sdc1",
                "/dev/sdd2",
                "/dev/nvme0n1p3"
            ]
        );
    }

    #[test]
    fn dual_boot_partitions_of_the_system_disk_mount_but_never_eject() {
        let found = devices(&parsed());
        let windows = find(&found, "/dev/nvme0n1p3");
        assert!(windows.is_some_and(|d| d.can_mount() && !d.can_eject()));
        assert_eq!(
            eject_plan(&parsed(), Path::new("/dev/nvme0n1"), "x"),
            None,
            "the system disk is never powered off"
        );
    }

    #[test]
    fn an_empty_card_reader_has_nothing_to_eject() {
        assert_eq!(eject_plan(&parsed(), Path::new("/dev/sda"), "Reader"), None);
    }

    #[test]
    fn an_unlocked_container_is_locked_and_never_mountable() {
        let found = devices(&parsed());
        let locked = find(&found, "/dev/sdc1");
        assert_eq!(locked.map(|d| d.state.clone()), Some(DeviceState::Locked));
        assert_eq!(locked.map(|d| d.label.as_str()), Some("8 GB Volume"));
        assert!(locked.is_some_and(|d| !d.can_mount() && !d.can_unmount()));
        assert!(locked.is_some_and(Device::can_eject));
    }

    #[test]
    fn an_internal_data_partition_mounts_but_is_not_ejectable() {
        let found = devices(&parsed());
        let data = find(&found, "/dev/sdd2");
        assert_eq!(data.map(|d| d.label.as_str()), Some("Data"));
        assert!(data.is_some_and(|d| d.can_mount() && !d.can_eject()));
    }

    #[test]
    fn a_media_mount_of_an_internal_partition_can_be_unmounted_not_ejected() {
        let mut nodes = parsed();
        if let Some(part) = nodes.get_mut(3).and_then(|disk| disk.children.get_mut(1)) {
            part.mountpoints = vec![PathBuf::from("/run/media/iperez/Data")];
        }
        let found = devices(&nodes);
        let data = find(&found, "/dev/sdd2");
        assert!(data.is_some_and(|d| d.can_unmount() && !d.can_eject()));
        // A fstab-style mount elsewhere is not the session's to unmount.
        if let Some(part) = nodes.get_mut(3).and_then(|disk| disk.children.get_mut(1)) {
            part.mountpoints = vec![PathBuf::from("/mnt/data")];
        }
        let found = devices(&nodes);
        assert!(find(&found, "/dev/sdd2").is_some_and(|d| !d.can_unmount()));
    }

    #[test]
    fn a_system_mount_on_an_external_disk_hides_only_that_partition_and_pins_the_disk() {
        let mut nodes = parsed();
        if let Some(part) = nodes.get_mut(1).and_then(|disk| disk.children.get_mut(0)) {
            part.mountpoints = vec![PathBuf::from("/home")];
        }
        let found = devices(&nodes);
        assert_eq!(find(&found, "/dev/sdb1"), None);
        let other = find(&found, "/dev/sdb2");
        assert!(other.is_some_and(|d| d.can_mount() && !d.can_eject()));
        assert_eq!(eject_plan(&nodes, Path::new("/dev/sdb"), "x"), None);
    }

    #[test]
    fn every_mount_point_of_a_device_is_known() {
        let mut nodes = parsed();
        if let Some(part) = nodes.get_mut(1).and_then(|disk| disk.children.get_mut(0)) {
            part.mountpoints = vec![
                PathBuf::from("/run/media/iperez/KINGSTON"),
                PathBuf::from("/mnt/bind"),
            ];
        }
        let found = devices(&nodes);
        let device = find(&found, "/dev/sdb1");
        assert_eq!(
            device.map(|d| d.mounts.clone()),
            Some(vec![
                PathBuf::from("/run/media/iperez/KINGSTON"),
                PathBuf::from("/mnt/bind")
            ])
        );
        let plan = device.and_then(unmount_plan);
        assert_eq!(
            plan.map(|p| p.mounts()),
            Some(vec![
                PathBuf::from("/run/media/iperez/KINGSTON"),
                PathBuf::from("/mnt/bind")
            ])
        );
        let eject = eject_plan(&nodes, Path::new("/dev/sdb"), "K");
        assert_eq!(eject.map(|p| p.mounts().len()), Some(2));
        let at = device_for_mount(&found, Path::new("/mnt/bind"));
        assert_eq!(at.map(|d| d.path.clone()), Some(PathBuf::from("/dev/sdb1")));
    }

    #[test]
    fn a_label_falls_back_to_the_size() {
        let found = devices(&parsed());
        assert_eq!(
            find(&found, "/dev/sdb2").map(|d| d.label.as_str()),
            Some("16 GB Volume")
        );
        let mut nodes = parsed();
        if let Some(part) = nodes.get_mut(1).and_then(|disk| disk.children.get_mut(1)) {
            part.label = Some(String::new());
            part.size = 0;
        }
        let found = devices(&nodes);
        assert_eq!(
            find(&found, "/dev/sdb2").map(|d| d.label.as_str()),
            Some("Volume")
        );
    }

    #[test]
    fn a_device_is_found_by_its_mount_point() {
        let found = devices(&parsed());
        let at = device_for_mount(&found, Path::new("/run/media/iperez/KINGSTON"));
        assert_eq!(at.map(|d| d.path.clone()), Some(PathBuf::from("/dev/sdb1")));
        assert!(device_for_mount(&found, Path::new("/nope")).is_none());
    }

    // --- plans --------------------------------------------------------------

    #[test]
    fn unmount_plans_name_the_device_and_its_mount() {
        let found = devices(&parsed());
        let plan = find(&found, "/dev/sdb1").and_then(unmount_plan);
        assert_eq!(
            plan,
            Some(DevicePlan {
                label: "KINGSTON".to_owned(),
                unmount: vec![UnmountStep {
                    device: PathBuf::from("/dev/sdb1"),
                    mounts: vec![PathBuf::from("/run/media/iperez/KINGSTON")],
                    via: UnmountVia::Udisks,
                }],
                power_off: None,
                tray_eject: None,
            })
        );
        assert_eq!(
            plan.map(|p| p.mounts()),
            Some(vec![PathBuf::from("/run/media/iperez/KINGSTON")])
        );
        // Nothing to unmount on an unmounted, a locked or a system device.
        assert_eq!(find(&found, "/dev/sdb2").and_then(unmount_plan), None);
        assert_eq!(find(&found, "/dev/sdd2").and_then(unmount_plan), None);
    }

    #[test]
    fn eject_unmounts_every_mounted_partition_then_powers_the_disk_off() {
        let mut nodes = parsed();
        if let Some(part) = nodes.get_mut(1).and_then(|disk| disk.children.get_mut(1)) {
            part.mountpoints = vec![PathBuf::from("/run/media/iperez/second")];
        }
        let plan = eject_plan(&nodes, Path::new("/dev/sdb"), "KINGSTON");
        assert_eq!(
            plan,
            Some(DevicePlan {
                label: "KINGSTON".to_owned(),
                unmount: vec![
                    UnmountStep {
                        device: PathBuf::from("/dev/sdb1"),
                        mounts: vec![PathBuf::from("/run/media/iperez/KINGSTON")],
                        via: UnmountVia::Udisks,
                    },
                    UnmountStep {
                        device: PathBuf::from("/dev/sdb2"),
                        mounts: vec![PathBuf::from("/run/media/iperez/second")],
                        via: UnmountVia::Udisks,
                    },
                ],
                power_off: Some(PathBuf::from("/dev/sdb")),
                tray_eject: None,
            })
        );
    }

    #[test]
    fn eject_of_a_disk_with_nothing_mounted_only_powers_it_off() {
        let plan = eject_plan(&parsed(), Path::new("/dev/sdc"), "Secret");
        assert_eq!(
            plan,
            Some(DevicePlan {
                label: "Secret".to_owned(),
                unmount: Vec::new(),
                power_off: Some(PathBuf::from("/dev/sdc")),
                tray_eject: None,
            })
        );
    }

    #[test]
    fn nested_mounts_unmount_deepest_first() {
        let mut nodes = parsed();
        if let Some(disk) = nodes.get_mut(1)
            && let Some(part) = disk.children.get_mut(1)
        {
            part.mountpoints = vec![PathBuf::from("/run/media/iperez/KINGSTON/inner")];
        }
        let plan = eject_plan(&nodes, Path::new("/dev/sdb"), "K");
        let order: Vec<PathBuf> = plan
            .map(|p| p.unmount.into_iter().map(|s| s.device).collect())
            .unwrap_or_default();
        assert_eq!(
            order,
            [PathBuf::from("/dev/sdb2"), PathBuf::from("/dev/sdb1")]
        );
    }

    #[test]
    fn a_system_disk_internal_disk_or_unknown_disk_is_never_ejected() {
        let nodes = parsed();
        for disk in [
            "/dev/nvme0n1",
            "/dev/sdd",
            "/dev/sdb1",
            "/dev/nope",
            "/dev/zram0",
        ] {
            assert_eq!(eject_plan(&nodes, Path::new(disk), "x"), None, "{disk}");
        }
    }

    // --- busy paths and tab fallback ---------------------------------------

    #[test]
    fn paths_on_a_mount_are_found_component_wise() {
        let mounts = [PathBuf::from("/run/media/u/USB")];
        assert!(path_uses_mounts(Path::new("/run/media/u/USB"), &mounts));
        assert!(path_uses_mounts(Path::new("/run/media/u/USB/a/b"), &mounts));
        assert!(!path_uses_mounts(Path::new("/run/media/u/USB2/a"), &mounts));
        assert!(!path_uses_mounts(Path::new("/run/media/u"), &mounts));
        assert!(!path_uses_mounts(Path::new("/home/u"), &[]));
    }

    #[test]
    fn a_path_is_on_a_mount_through_symlinks_and_not_yet_existing_parts() -> TestResult {
        let fixture = Fixture::new()?;
        let mount = fixture.0.join("media/USB");
        fs::create_dir_all(&mount)?;
        let link = fixture.0.join("usblink");
        std::os::unix::fs::symlink(&mount, &link)?;
        let mounts = [mount.clone()];
        assert!(path_is_on_mounts(&link, &mounts));
        assert!(path_is_on_mounts(&link.join("new/dir"), &mounts));
        assert!(path_is_on_mounts(&mount.join("x/../y"), &mounts));
        assert!(path_is_on_mounts(
            &fixture.0.join("media/other/../USB/a"),
            &mounts
        ));
        assert!(!path_is_on_mounts(&fixture.0.join("media"), &mounts));
        assert!(!path_is_on_mounts(&fixture.0.join("media/USB2/a"), &mounts));
        assert!(!path_is_on_mounts(Path::new("/home/u/src"), &mounts));
        assert_eq!(
            real_location(&link.join("new")),
            real_location(&mount).join("new")
        );
        Ok(())
    }

    #[test]
    fn a_tab_behind_a_symlink_falls_back_past_it() -> TestResult {
        let fixture = Fixture::new()?;
        let mount = fixture.0.join("media/USB");
        fs::create_dir_all(mount.join("sub"))?;
        let link = fixture.0.join("usblink");
        std::os::unix::fs::symlink(&mount, &link)?;
        let target = fallback_target(
            &link.join("sub"),
            std::slice::from_ref(&mount),
            Some(Path::new("/home/u")),
            Path::exists,
        );
        assert_eq!(target, fixture.0);
        Ok(())
    }

    #[test]
    fn a_fallback_path_has_no_dot_dot_left() -> TestResult {
        let fixture = Fixture::new()?;
        let mount = fixture.0.join("media/USB");
        fs::create_dir_all(&mount)?;
        fs::create_dir_all(fixture.0.join("home/a"))?;
        let target = fallback_target(
            &fixture.0.join("home/a/.."),
            std::slice::from_ref(&mount),
            None,
            Path::exists,
        );
        assert_eq!(target, real_location(&fixture.0.join("home")));
        assert!(
            target
                .components()
                .all(|part| part != std::path::Component::ParentDir)
        );
        Ok(())
    }

    #[test]
    fn a_tab_moves_to_the_nearest_existing_ancestor_outside_the_mount() {
        let mounts = [PathBuf::from("/run/media/u/USB")];
        let all = |_: &Path| true;
        assert_eq!(
            fallback_target(
                Path::new("/run/media/u/USB/a/b"),
                &mounts,
                Some(Path::new("/home/u")),
                all
            ),
            PathBuf::from("/run/media/u")
        );
        assert_eq!(
            fallback_target(Path::new("/run/media/u/USB"), &mounts, None, all),
            PathBuf::from("/run/media/u")
        );
        // The ancestor is gone: next one up, then Home, then the root.
        let only_run = |path: &Path| path == Path::new("/run");
        assert_eq!(
            fallback_target(
                Path::new("/run/media/u/USB/a"),
                &mounts,
                Some(Path::new("/home/u")),
                only_run
            ),
            PathBuf::from("/run")
        );
        let none = |_: &Path| false;
        assert_eq!(
            fallback_target(
                Path::new("/run/media/u/USB/a"),
                &mounts,
                Some(Path::new("/home/u")),
                none
            ),
            PathBuf::from("/home/u")
        );
        assert_eq!(
            fallback_target(Path::new("/run/media/u/USB/a"), &mounts, None, none),
            PathBuf::from("/")
        );
    }

    #[test]
    fn a_tab_on_a_nested_mount_skips_every_mount_of_the_plan() {
        let mounts = [PathBuf::from("/mnt/disk"), PathBuf::from("/mnt/disk/inner")];
        assert_eq!(
            fallback_target(Path::new("/mnt/disk/inner/x"), &mounts, None, |_| true),
            PathBuf::from("/mnt")
        );
    }

    // --- udisksctl ----------------------------------------------------------

    #[test]
    fn the_argv_never_goes_through_a_shell_and_never_prompts() {
        let os = |s: &str| OsString::from(s);
        assert_eq!(
            mount_args(Path::new("/dev/sdb2")),
            [
                os("mount"),
                os("--block-device=/dev/sdb2"),
                os("--no-user-interaction")
            ]
        );
        assert_eq!(
            unmount_args(Path::new("/dev/sdb2")),
            [
                os("unmount"),
                os("--block-device=/dev/sdb2"),
                os("--no-user-interaction")
            ]
        );
        assert_eq!(
            power_off_args(Path::new("/dev/sdb")),
            [
                os("power-off"),
                os("--block-device=/dev/sdb"),
                os("--no-user-interaction")
            ]
        );
        // A hostile device name stays one argument, glued to the option so
        // a leading dash is never an option of its own.
        let evil = Path::new("/dev/sdb; rm -rf ~");
        assert_eq!(
            mount_args(evil).get(1),
            Some(&os("--block-device=/dev/sdb; rm -rf ~"))
        );
        assert_eq!(
            unmount_args(Path::new("--help")).get(1),
            Some(&os("--block-device=--help"))
        );
    }

    #[test]
    fn the_mount_point_comes_from_the_mounted_line() {
        assert_eq!(
            parse_mount_output("Mounted /dev/sdb2 at /run/media/iperez/My Disk\n"),
            Some(PathBuf::from("/run/media/iperez/My Disk"))
        );
        assert_eq!(
            parse_mount_output("noise\nMounted /dev/sdb2 at /media/x.\n"),
            Some(PathBuf::from("/media/x."))
        );
        assert_eq!(parse_mount_output(""), None);
        assert_eq!(parse_mount_output("Mounted /dev/sdb2\n"), None);
        assert_eq!(parse_mount_output("Mounted /dev/sdb2 at relative\n"), None);
    }

    #[test]
    fn the_old_trailing_period_is_dropped_only_when_the_path_needs_it() {
        let exists = |path: &Path| path == Path::new("/media/x");
        assert_eq!(
            settle_mount_point(PathBuf::from("/media/x."), exists),
            PathBuf::from("/media/x")
        );
        let exists = |path: &Path| path == Path::new("/media/x.");
        assert_eq!(
            settle_mount_point(PathBuf::from("/media/x."), exists),
            PathBuf::from("/media/x.")
        );
        assert_eq!(
            settle_mount_point(PathBuf::from("/media/y"), |_| false),
            PathBuf::from("/media/y")
        );
    }

    #[test]
    fn an_already_mounted_failure_names_where() {
        let stderr = "Error mounting /dev/sdb1: GDBus.Error:org.freedesktop.UDisks2.Error.AlreadyMounted: Device /dev/sdb1 is already mounted at `/run/media/iperez/KINGSTON'.\n";
        assert_eq!(
            already_mounted_at(stderr),
            Some(PathBuf::from("/run/media/iperez/KINGSTON"))
        );
        assert_eq!(already_mounted_at("Error mounting: target is busy"), None);
    }

    #[test]
    fn failures_keep_the_reason_and_drop_the_noise() {
        assert_eq!(
            failure_reason(
                "Error unmounting /dev/sdb1: GDBus.Error:org.freedesktop.UDisks2.Error.DeviceBusy: target is busy\n"
            ),
            "target is busy"
        );
        assert_eq!(
            failure_reason(
                "Error mounting /dev/sdb1: GDBus.Error:org.freedesktop.UDisks2.Error.NotAuthorizedCanObtain: Not authorized to perform operation\n"
            ),
            "Not authorized to perform operation"
        );
        assert_eq!(
            failure_reason(
                "Error mounting /dev/sdb1: GDBus.Error:org.freedesktop.UDisks2.Error.Failed: Error mounting system-managed device /dev/sdb1: not authorized\nsecond line\n"
            ),
            "Error mounting system-managed device /dev/sdb1: not authorized"
        );
        assert_eq!(failure_reason("plain failure\n"), "plain failure");
        assert_eq!(failure_reason(""), "udisksctl failed without a message");
        assert_eq!(
            failure_reason("GDBus.Error:org.freedesktop.UDisks2.Error.DeviceBusy\n"),
            "DeviceBusy"
        );
    }

    // --- the runner, against fake programs ----------------------------------

    static SEQ: AtomicU64 = AtomicU64::new(0);
    /// A script written while another test forks can be exec'd with
    /// ETXTBSY: the tests that run fake programs take turns.
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A fresh directory under the system temp dir, removed on drop.
    struct Fixture(
        PathBuf,
        #[allow(dead_code)] std::sync::MutexGuard<'static, ()>,
    );

    impl Fixture {
        fn new() -> std::io::Result<Fixture> {
            let dir = std::env::temp_dir().join(format!(
                "fileflux-devices-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir)?;
            let turn = TURN
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Ok(Fixture(dir, turn))
        }

        /// An executable script `name` with `body`.
        fn script(&self, name: &str, body: &str) -> std::io::Result<PathBuf> {
            let path = self.0.join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n"))?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
            Ok(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    /// `run_tool` with the fork-versus-write race of a just-written script
    /// (ETXTBSY) retried.
    fn run(program: &Path, args: &[&str], timeout: Duration) -> Result<ToolOutput, DeviceError> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        for _ in 0..50 {
            match run_tool(program, &args, timeout) {
                Err(DeviceError::Spawn { reason, .. }) if reason.contains("busy") => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                other => return other,
            }
        }
        run_tool(program, &args, timeout)
    }

    #[test]
    fn a_tool_is_found_on_the_path_only_when_executable() -> TestResult {
        let fixture = Fixture::new()?;
        let tool = fixture.script("udisksctl", "exit 0")?;
        fs::write(fixture.0.join("plain"), "data")?;
        let path = std::env::join_paths([fixture.0.as_path()])?;
        assert_eq!(find_tool("udisksctl", Some(&path)), Some(tool));
        assert_eq!(find_tool("plain", Some(&path)), None);
        assert_eq!(find_tool("missing", Some(&path)), None);
        assert_eq!(find_tool("udisksctl", None), None);
        Ok(())
    }

    #[test]
    fn a_tool_run_keeps_argv_stdout_stderr_and_status() -> TestResult {
        let fixture = Fixture::new()?;
        let log = fixture.0.join("argv.log");
        let tool = fixture.script(
            "tool",
            &format!(
                "for a in \"$@\"; do printf '%s\\n' \"$a\" >> '{}'; done\necho out; echo err >&2; exit 3",
                log.display()
            ),
        )?;
        let output = run(
            &tool,
            &["a b", "$(touch /tmp/x)", "--no-user-interaction"],
            Duration::from_secs(5),
        )?;
        assert_eq!(
            output,
            ToolOutput {
                success: false,
                stdout: "out\n".to_owned(),
                stderr: "err\n".to_owned(),
            }
        );
        assert_eq!(
            fs::read_to_string(&log)?,
            "a b\n$(touch /tmp/x)\n--no-user-interaction\n"
        );
        Ok(())
    }

    #[test]
    fn a_tool_past_its_timeout_is_killed() -> TestResult {
        let fixture = Fixture::new()?;
        let tool = fixture.script("slow", "exec sleep 30")?;
        let started = std::time::Instant::now();
        let result = run(&tool, &[], Duration::from_millis(300));
        assert!(
            matches!(result, Err(DeviceError::TimedOut { .. })),
            "{result:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        Ok(())
    }

    #[test]
    fn a_missing_program_is_a_spawn_error() {
        let result = run_tool(Path::new("/nonexistent/tool"), &[], Duration::from_secs(1));
        assert!(matches!(result, Err(DeviceError::Spawn { .. })));
    }

    #[test]
    fn list_blocks_parses_what_lsblk_prints() -> TestResult {
        let fixture = Fixture::new()?;
        let canned = fixture.0.join("lsblk.json");
        fs::write(&canned, FIXTURE)?;
        let lsblk = fixture.script("lsblk", &format!("cat '{}'", canned.display()))?;
        let nodes = list_blocks(&lsblk)?;
        assert_eq!(nodes.len(), 7);
        let failing = fixture.script("lsblk-bad", "echo 'boom' >&2; exit 1")?;
        assert!(matches!(
            list_blocks(&failing),
            Err(DeviceError::Failed { reason }) if reason == "boom"
        ));
        Ok(())
    }

    #[test]
    fn mount_answers_the_mount_point_and_logs_its_argv() -> TestResult {
        let fixture = Fixture::new()?;
        let target = fixture.0.join("mnt");
        fs::create_dir_all(&target)?;
        let log = fixture.0.join("argv.log");
        let ctl = fixture.script(
            "udisksctl",
            &format!(
                "echo \"$@\" >> '{}'\necho \"Mounted /dev/sdb2 at {}\"",
                log.display(),
                target.display()
            ),
        )?;
        assert_eq!(mount(&ctl, Path::new("/dev/sdb2"))?, target);
        assert_eq!(
            fs::read_to_string(&log)?,
            "mount --block-device=/dev/sdb2 --no-user-interaction\n"
        );
        Ok(())
    }

    #[test]
    fn mount_failures_carry_the_reason_and_already_mounted_is_success() -> TestResult {
        let fixture = Fixture::new()?;
        let busy = fixture.script(
            "busy",
            "echo 'Error mounting /dev/sdb2: GDBus.Error:org.freedesktop.UDisks2.Error.NotAuthorizedCanObtain: Not authorized to perform operation' >&2; exit 1",
        )?;
        assert_eq!(
            mount(&busy, Path::new("/dev/sdb2")),
            Err(DeviceError::Failed {
                reason: "Not authorized to perform operation".to_owned()
            })
        );
        let already = fixture.script(
            "already",
            "echo \"Error mounting /dev/sdb2: GDBus.Error:org.freedesktop.UDisks2.Error.AlreadyMounted: Device /dev/sdb2 is already mounted at \\`/run/media/u/X'.\" >&2; exit 1",
        )?;
        assert_eq!(
            mount(&already, Path::new("/dev/sdb2"))?,
            PathBuf::from("/run/media/u/X")
        );
        let silent = fixture.script("silent", "echo 'something else'")?;
        assert!(matches!(
            mount(&silent, Path::new("/dev/sdb2")),
            Err(DeviceError::Failed { .. })
        ));
        Ok(())
    }

    fn plan() -> DevicePlan {
        DevicePlan {
            label: "KINGSTON".to_owned(),
            unmount: vec![
                UnmountStep {
                    device: PathBuf::from("/dev/sdb1"),
                    mounts: vec![PathBuf::from("/run/media/u/A")],
                    via: UnmountVia::Udisks,
                },
                UnmountStep {
                    device: PathBuf::from("/dev/sdb2"),
                    mounts: vec![PathBuf::from("/run/media/u/B")],
                    via: UnmountVia::Udisks,
                },
            ],
            power_off: Some(PathBuf::from("/dev/sdb")),
            tray_eject: None,
        }
    }

    #[test]
    fn a_plan_runs_unmounts_in_order_then_powers_off() -> TestResult {
        let fixture = Fixture::new()?;
        let log = fixture.0.join("argv.log");
        let ctl = fixture.script("udisksctl", &format!("echo \"$@\" >> '{}'", log.display()))?;
        run_plan(&ctl, &plan())?;
        assert_eq!(
            fs::read_to_string(&log)?,
            "unmount --block-device=/dev/sdb1 --no-user-interaction\nunmount --block-device=/dev/sdb2 --no-user-interaction\npower-off --block-device=/dev/sdb --no-user-interaction\n"
        );
        Ok(())
    }

    #[test]
    fn a_failing_unmount_stops_the_plan_before_the_power_off() -> TestResult {
        let fixture = Fixture::new()?;
        let log = fixture.0.join("argv.log");
        let ctl = fixture.script(
            "udisksctl",
            &format!(
                "echo \"$@\" >> '{}'\ncase \"$*\" in *sdb1*) echo 'Error unmounting /dev/sdb1: GDBus.Error:org.freedesktop.UDisks2.Error.DeviceBusy: target is busy' >&2; exit 1;; esac",
                log.display()
            ),
        )?;
        assert_eq!(
            run_plan(&ctl, &plan()),
            Err(DeviceError::Failed {
                reason: "target is busy".to_owned()
            })
        );
        assert_eq!(
            fs::read_to_string(&log)?,
            "unmount --block-device=/dev/sdb1 --no-user-interaction\n",
            "nothing after the failure runs"
        );
        Ok(())
    }

    #[test]
    fn an_already_unmounted_device_does_not_stop_the_plan() -> TestResult {
        let fixture = Fixture::new()?;
        let log = fixture.0.join("argv.log");
        let ctl = fixture.script(
            "udisksctl",
            &format!(
                "echo \"$@\" >> '{}'\ncase \"$*\" in unmount*sdb1*) echo 'Error unmounting /dev/sdb1: GDBus.Error:org.freedesktop.UDisks2.Error.NotMounted: Device /dev/sdb1 is not mounted' >&2; exit 1;; esac",
                log.display()
            ),
        )?;
        run_plan(&ctl, &plan())?;
        assert_eq!(fs::read_to_string(&log)?.lines().count(), 3);
        Ok(())
    }
    fn first_step(plan: &DevicePlan) -> UnmountStep {
        plan.unmount
            .first()
            .cloned()
            .unwrap_or_else(|| UnmountStep {
                device: PathBuf::new(),
                mounts: Vec::new(),
                via: UnmountVia::Udisks,
            })
    }

    #[test]
    fn gio_unmounts_a_gvfs_path_by_argv() {
        assert_eq!(
            gio_unmount_args(Path::new("/run/user/1000/gvfs/mtp:host=P")),
            ["mount", "-u", "/run/user/1000/gvfs/mtp:host=P"].map(OsString::from)
        );
        let plan = gio_unmount_plan("Pixel", Path::new("/run/user/1000/gvfs/mtp:host=P"));
        assert_eq!(plan.label, "Pixel");
        assert_eq!(plan.power_off, None, "a phone is never powered off");
        assert_eq!(
            plan.mounts(),
            [PathBuf::from("/run/user/1000/gvfs/mtp:host=P")]
        );
        assert_eq!(plan.unmount.len(), 1);
        assert_eq!(
            plan.unmount.first().map(|step| step.via),
            Some(UnmountVia::Gio)
        );
    }

    #[test]
    fn each_step_runs_through_its_own_tool_and_needs_it_installed() {
        let udisks = Path::new("/bin/udisksctl");
        let gio = Path::new("/bin/gio");
        let block = first_step(&plan());
        let gvfs = first_step(&gio_unmount_plan("P", Path::new("/g/x")));
        assert_eq!(unmount_program(&block, Some(udisks), Some(gio)), Ok(udisks));
        assert_eq!(unmount_program(&gvfs, Some(udisks), Some(gio)), Ok(gio));
        assert!(matches!(
            unmount_program(&gvfs, Some(udisks), None),
            Err(DeviceError::Failed { reason }) if reason.contains("gio")
        ));
        assert!(unmount_program(&block, None, Some(gio)).is_err());
    }

    #[test]
    fn a_gio_unmount_runs_gio_with_the_path_and_reports_its_message() -> TestResult {
        let fixture = Fixture::new()?;
        let log = fixture.0.join("argv.log");
        let ok = fixture.script("gio", &format!("echo \"$@\" >> '{}'", log.display()))?;
        let step = first_step(&gio_unmount_plan("P", Path::new("/g/mtp:host=P")));
        run_unmount(&ok, &step)?;
        assert_eq!(fs::read_to_string(&log)?, "mount -u /g/mtp:host=P\n");
        let busy = fixture.script(
            "gio-busy",
            "echo 'gio: Unmounting failed: busy' >&2; exit 1",
        )?;
        assert_eq!(
            run_unmount(&busy, &step),
            Err(DeviceError::Failed {
                reason: "gio: Unmounting failed: busy".to_owned()
            })
        );
        let gone = fixture.script(
            "gio-gone",
            "echo 'gio: Containing mount for file not found' >&2; exit 1",
        )?;
        run_unmount(&gone, &step)?;
        Ok(())
    }

    // --- encrypted drives -----------------------------------------------------

    /// A LUKS stick, locked (`sde`), unlocked and mounted (`sdf`), and
    /// unlocked but not mounted (`sdg`).
    const LUKS: &str = r#"{"blockdevices":[
      {"name":"sde","path":"/dev/sde","fstype":null,"size":8000000000,"mountpoints":[null],"rm":true,"hotplug":true,"type":"disk","pkname":null,"uuid":null,
         "children":[{"name":"sde1","path":"/dev/sde1","label":"Vault","fstype":"crypto_LUKS","size":7999000000,"mountpoints":[null],"rm":true,"hotplug":true,"type":"part","pkname":"sde","uuid":"u1"}]},
      {"name":"sdf","path":"/dev/sdf","fstype":null,"size":8000000000,"mountpoints":[null],"rm":true,"hotplug":true,"type":"disk","pkname":null,"uuid":null,
         "children":[{"name":"sdf1","path":"/dev/sdf1","fstype":"crypto_LUKS","size":7999000000,"mountpoints":[null],"rm":true,"hotplug":true,"type":"part","pkname":"sdf","uuid":"u2",
            "children":[{"name":"luks-u2","path":"/dev/mapper/luks-u2","label":"Secrets","fstype":"ext4","size":7900000000,"mountpoints":["/run/media/iperez/Secrets"],"rm":true,"hotplug":true,"type":"crypt","pkname":"sdf1","uuid":"m2"}]}]},
      {"name":"sdg","path":"/dev/sdg","fstype":null,"size":8000000000,"mountpoints":[null],"rm":true,"hotplug":true,"type":"disk","pkname":null,"uuid":null,
         "children":[{"name":"sdg1","path":"/dev/sdg1","fstype":"crypto_LUKS","size":7999000000,"mountpoints":[null],"rm":true,"hotplug":true,"type":"part","pkname":"sdg","uuid":"u3",
            "children":[{"name":"luks-u3","path":"/dev/mapper/luks-u3","fstype":"ext4","size":7900000000,"mountpoints":[null],"rm":true,"hotplug":true,"type":"crypt","pkname":"sdg1","uuid":"m3"}]}]}
    ]}"#;

    fn luks() -> Vec<BlockNode> {
        parse_lsblk(LUKS).unwrap_or_default()
    }

    #[test]
    fn a_luks_container_without_children_is_locked_and_can_be_unlocked() {
        let found = devices(&luks());
        let locked = find(&found, "/dev/sde1");
        assert_eq!(locked.map(|d| d.state.clone()), Some(DeviceState::Locked));
        assert!(locked.is_some_and(Device::can_unlock));
        assert_eq!(locked.and_then(|d| d.container.clone()), None);
    }

    #[test]
    fn a_luks_container_with_a_mapper_child_is_unlocked_and_shows_the_child() {
        let found = devices(&luks());
        assert_eq!(find(&found, "/dev/sdf1"), None, "the container is hidden");
        let mapper = find(&found, "/dev/mapper/luks-u2");
        assert_eq!(
            mapper.map(|d| d.state.clone()),
            Some(DeviceState::Mounted(PathBuf::from(
                "/run/media/iperez/Secrets"
            )))
        );
        assert!(mapper.is_some_and(|d| !d.can_unlock()));
        assert_eq!(
            mapper.and_then(|d| d.container.clone()),
            Some(PathBuf::from("/dev/sdf1"))
        );
        let idle = find(&found, "/dev/mapper/luks-u3");
        assert_eq!(idle.map(|d| d.state.clone()), Some(DeviceState::Unmounted));
        assert!(idle.is_some_and(|d| !d.can_unlock()));
    }

    #[test]
    fn only_a_locked_container_from_the_listing_is_an_unlock_target() {
        let nodes = luks();
        let target = unlock_target(&nodes, Path::new("/dev/sde1"));
        assert_eq!(
            target.as_ref().map(|t| (t.path(), t.label())),
            Ok((Path::new("/dev/sde1"), "Vault"))
        );
        for (request, why) in [
            ("sde1", "relative"),
            ("-d", "an option"),
            ("/dev/-sde1", "leading dash name"),
            ("/dev/../tmp/sde1", "parent component"),
            ("/tmp/sde1", "outside /dev"),
            ("/dev/sde1/", "another spelling"),
            ("/dev/sdz1", "not in the listing"),
            ("/dev/sdf1", "an unlocked container is not listed"),
            ("/dev/mapper/luks-u2", "already unlocked"),
            ("/dev/sde", "a disk"),
            ("", "empty"),
        ] {
            assert!(
                unlock_target(&nodes, Path::new(request)).is_err(),
                "{request:?} ({why}) must be refused"
            );
        }
    }

    #[test]
    fn a_listed_name_starting_with_a_dash_is_refused_too() {
        let json = r#"{"blockdevices":[{"name":"-x","path":"/dev/-x","fstype":"crypto_LUKS","size":1,"mountpoints":[null],"rm":true,"hotplug":true,"type":"part","pkname":null,"uuid":null}]}"#;
        let nodes = parse_lsblk(json).unwrap_or_default();
        assert!(devices(&nodes).iter().any(Device::can_unlock));
        assert_eq!(
            unlock_target(&nodes, Path::new("/dev/-x")),
            Err(UnlockError::LeadingDash)
        );
    }

    #[test]
    fn unlocking_runs_gio_mount_d_with_the_device_alone() {
        let nodes = luks();
        let target = unlock_target(&nodes, Path::new("/dev/sde1"));
        assert_eq!(
            target.map(|t| unlock_args(&t)),
            Ok(vec![
                OsString::from("mount"),
                OsString::from("-d"),
                OsString::from("/dev/sde1")
            ])
        );
    }

    #[test]
    fn the_unlocked_mount_comes_from_the_children_of_the_container() {
        let nodes = luks();
        assert_eq!(
            unlocked_mount(&nodes, Path::new("/dev/sdf1")),
            Some(PathBuf::from("/run/media/iperez/Secrets"))
        );
        assert_eq!(unlocked_mount(&nodes, Path::new("/dev/sdg1")), None);
        assert_eq!(unlocked_mount(&nodes, Path::new("/dev/sde1")), None);
        assert_eq!(unlocked_mount(&nodes, Path::new("/dev/nope")), None);
    }

    #[test]
    fn the_unlocked_mount_is_waited_for_with_fresh_listings() {
        let mut calls = 0;
        let found = wait_for_unlocked_mount(
            || {
                calls += 1;
                let mut nodes = luks();
                if calls >= 3
                    && let Some(node) = nodes
                        .get_mut(2)
                        .and_then(|disk| disk.children.first_mut())
                        .and_then(|part| part.children.first_mut())
                {
                    node.mountpoints = vec![PathBuf::from("/run/media/iperez/late")];
                }
                Ok(nodes)
            },
            Path::new("/dev/sdg1"),
            5,
            Duration::ZERO,
        );
        assert_eq!(
            found.map(|(_, mount)| mount),
            Some(PathBuf::from("/run/media/iperez/late"))
        );
        assert_eq!(calls, 3, "stops at the first listing that has it");
        assert_eq!(
            wait_for_unlocked_mount(|| Ok(luks()), Path::new("/dev/sdg1"), 3, Duration::ZERO),
            None,
            "gives up after the attempts"
        );
        let broken = || {
            Err(DeviceError::Failed {
                reason: "x".to_owned(),
            })
        };
        assert_eq!(
            wait_for_unlocked_mount(broken, Path::new("/dev/sdg1"), 2, Duration::ZERO),
            None
        );
    }

    #[test]
    fn unmounting_an_unlocked_volume_locks_its_container_after_the_unmount() {
        let found = devices(&luks());
        let plan = find(&found, "/dev/mapper/luks-u2").and_then(unmount_plan);
        assert_eq!(
            plan.map(|p| p.unmount),
            Some(vec![
                UnmountStep {
                    device: PathBuf::from("/dev/mapper/luks-u2"),
                    mounts: vec![PathBuf::from("/run/media/iperez/Secrets")],
                    via: UnmountVia::Udisks,
                },
                UnmountStep {
                    device: PathBuf::from("/dev/sdf1"),
                    mounts: Vec::new(),
                    via: UnmountVia::UdisksLock,
                },
            ])
        );
        // A plain volume still unmounts alone.
        let plain = devices(&parsed());
        assert_eq!(
            find(&plain, "/dev/sdb1")
                .and_then(unmount_plan)
                .map(|p| p.unmount.len()),
            Some(1)
        );
    }

    #[test]
    fn ejecting_a_disk_locks_its_unlocked_containers_before_power_off() {
        let plan = eject_plan(&luks(), Path::new("/dev/sdf"), "Secrets");
        assert_eq!(
            plan,
            Some(DevicePlan {
                label: "Secrets".to_owned(),
                unmount: vec![
                    UnmountStep {
                        device: PathBuf::from("/dev/mapper/luks-u2"),
                        mounts: vec![PathBuf::from("/run/media/iperez/Secrets")],
                        via: UnmountVia::Udisks,
                    },
                    UnmountStep {
                        device: PathBuf::from("/dev/sdf1"),
                        mounts: Vec::new(),
                        via: UnmountVia::UdisksLock,
                    },
                ],
                power_off: Some(PathBuf::from("/dev/sdf")),
                tray_eject: None,
            })
        );
        // Unlocked but not mounted: only the lock.
        let idle = eject_plan(&luks(), Path::new("/dev/sdg"), "x").map(|p| p.unmount);
        assert_eq!(
            idle,
            Some(vec![UnmountStep {
                device: PathBuf::from("/dev/sdg1"),
                mounts: Vec::new(),
                via: UnmountVia::UdisksLock,
            }])
        );
        // A locked one has nothing to lock.
        let locked = eject_plan(&luks(), Path::new("/dev/sde"), "x").map(|p| p.unmount);
        assert_eq!(locked, Some(Vec::new()));
    }

    #[test]
    fn a_lock_step_runs_udisksctl_lock_and_tolerates_an_already_locked_device() -> TestResult {
        let fixture = Fixture::new()?;
        let log = fixture.0.join("argv.log");
        let ok = fixture.script("udisksctl", &format!("echo \"$@\" >> '{}'", log.display()))?;
        let step = UnmountStep {
            device: PathBuf::from("/dev/sdf1"),
            mounts: Vec::new(),
            via: UnmountVia::UdisksLock,
        };
        run_unmount(&ok, &step)?;
        assert_eq!(
            fs::read_to_string(&log)?,
            "lock --block-device=/dev/sdf1 --no-user-interaction\n"
        );
        let gone = fixture.script(
            "udisksctl-gone",
            "echo 'Error locking /dev/sdf1: Device /dev/sdf1 is not unlocked' >&2; exit 1",
        )?;
        run_unmount(&gone, &step)?;
        let busy = fixture.script("udisksctl-busy", "echo 'Error locking: busy' >&2; exit 1")?;
        assert!(run_unmount(&busy, &step).is_err());
        let program = unmount_program(&step, Some(Path::new("/bin/udisksctl")), None);
        assert_eq!(program, Ok(Path::new("/bin/udisksctl")));
        Ok(())
    }

    #[test]
    fn an_unlock_target_must_be_the_exact_listing_text() {
        let nodes = luks();
        for request in ["//dev/sde1", "/dev//sde1", "/dev/./sde1", "/dev/sde1/."] {
            assert_eq!(
                unlock_target(&nodes, Path::new(request)).map(|t| t.path().to_path_buf()),
                Err(UnlockError::NotADevicePath),
                "{request}"
            );
        }
        assert!(unlock_target(&nodes, Path::new("/dev/sde1")).is_ok());
    }

    const NESTED: &str = r#"{"blockdevices":[
      {"name":"sde","path":"/dev/sde","fstype":null,"size":8000000000,"mountpoints":[null],"rm":true,"hotplug":true,"type":"disk","pkname":null,"uuid":null,
         "children":[{"name":"sde1","path":"/dev/sde1","fstype":"crypto_LUKS","size":7999000000,"mountpoints":[null],"rm":true,"hotplug":true,"type":"part","pkname":"sde","uuid":"u1",
            "children":[{"name":"outer","path":"/dev/mapper/outer","fstype":"crypto_LUKS","size":7900000000,"mountpoints":[null],"rm":true,"hotplug":true,"type":"crypt","pkname":"sde1","uuid":"m1",
               "children":[{"name":"inner","path":"/dev/mapper/inner","fstype":"ext4","size":7800000000,"mountpoints":["/run/media/iperez/Deep"],"rm":true,"hotplug":true,"type":"crypt","pkname":"outer","uuid":"m2"}]}]}]}
    ]}"#;

    #[test]
    fn nested_luks_is_locked_inside_out_before_power_off() {
        let nodes = parse_lsblk(NESTED).unwrap_or_default();
        let plan = eject_plan(&nodes, Path::new("/dev/sde"), "Deep");
        let steps: Vec<(PathBuf, UnmountVia)> = plan
            .iter()
            .flat_map(|p| p.unmount.iter().map(|s| (s.device.clone(), s.via)))
            .collect();
        assert_eq!(
            steps,
            vec![
                (PathBuf::from("/dev/mapper/inner"), UnmountVia::Udisks),
                (PathBuf::from("/dev/mapper/outer"), UnmountVia::UdisksLock),
                (PathBuf::from("/dev/sde1"), UnmountVia::UdisksLock),
            ]
        );
        assert_eq!(
            plan.and_then(|p| p.power_off),
            Some(PathBuf::from("/dev/sde"))
        );
    }

    #[test]
    fn only_an_unlocked_message_is_tolerated_by_a_lock_step() -> TestResult {
        let fixture = Fixture::new()?;
        let step = UnmountStep {
            device: PathBuf::from("/dev/sdf1"),
            mounts: Vec::new(),
            via: UnmountVia::UdisksLock,
        };
        for (name, message) in [
            ("mounted", "Cleartext device /dev/dm-3 is mounted"),
            ("lookup", "Error looking up object for device"),
        ] {
            let tool = fixture.script(name, &format!("echo '{message}' >&2; exit 1"))?;
            assert!(run_unmount(&tool, &step).is_err(), "{message}");
        }
        Ok(())
    }

    #[test]
    fn a_container_is_unlocked_when_the_listing_shows_a_mapper_child() {
        let nodes = luks();
        assert!(is_unlocked(&nodes, Path::new("/dev/sdf1")));
        assert!(is_unlocked(&nodes, Path::new("/dev/sdg1")));
        assert!(!is_unlocked(&nodes, Path::new("/dev/sde1")));
        assert!(!is_unlocked(&nodes, Path::new("/dev/nope")));
    }

    // --- optical media ------------------------------------------------------

    /// Four drives: a mounted data DVD, an empty drive, an unmounted data
    /// DVD and an audio CD. `lsblk` calls every one of them type `rom`.
    const DISCS: &str = r#"{"blockdevices":[
      {"name":"sr0","path":"/dev/sr0","label":"UBUNTU_24","fstype":"iso9660","size":4700000000,"mountpoints":["/run/media/iperez/UBUNTU_24"],"rm":true,"hotplug":true,"type":"rom","pkname":null,"uuid":"2024-04-01"},
      {"name":"sr1","path":"/dev/sr1","label":null,"fstype":null,"size":0,"mountpoints":[null],"rm":true,"hotplug":true,"type":"rom","pkname":null,"uuid":null},
      {"name":"sr2","path":"/dev/sr2","label":null,"fstype":"udf","size":8500000000,"mountpoints":[null],"rm":true,"hotplug":true,"type":"rom","pkname":null,"uuid":"udf-1"},
      {"name":"sr3","path":"/dev/sr3","label":null,"fstype":null,"size":734003200,"mountpoints":[null],"rm":true,"hotplug":true,"type":"rom","pkname":null,"uuid":null}
    ]}"#;

    fn discs() -> Vec<BlockNode> {
        parse_lsblk(DISCS).unwrap_or_default()
    }

    #[test]
    fn a_data_disc_is_listed_with_its_label_mounted_or_not() -> TestResult {
        let list = devices(&discs());
        let mounted = find(&list, "/dev/sr0").ok_or("device not listed")?;
        assert_eq!(mounted.label, "UBUNTU_24");
        assert_eq!(mounted.fstype.as_deref(), Some("iso9660"));
        assert_eq!(
            mounted.state,
            DeviceState::Mounted(PathBuf::from("/run/media/iperez/UBUNTU_24"))
        );
        assert!(mounted.optical && mounted.ejectable && mounted.can_unmount());
        assert_eq!(mounted.disk, PathBuf::from("/dev/sr0"));
        let unmounted = find(&list, "/dev/sr2").ok_or("device not listed")?;
        assert_eq!(unmounted.label, "Disc");
        assert_eq!(unmounted.state, DeviceState::Unmounted);
        assert!(unmounted.can_mount() && unmounted.can_eject() && unmounted.optical);
        Ok(())
    }

    #[test]
    fn an_empty_drive_is_hidden_and_has_no_eject_plan() {
        let nodes = discs();
        assert!(find(&devices(&nodes), "/dev/sr1").is_none());
        assert_eq!(eject_plan(&nodes, Path::new("/dev/sr1"), "Drive"), None);
    }

    #[test]
    fn an_audio_cd_is_listed_but_cannot_be_mounted() -> TestResult {
        let list = devices(&discs());
        let cd = find(&list, "/dev/sr3").ok_or("device not listed")?;
        assert_eq!(cd.label, "Audio CD");
        assert_eq!(cd.fstype, None);
        assert_eq!(cd.state, DeviceState::Audio);
        assert!(!cd.can_mount() && !cd.can_unlock() && !cd.can_unmount());
        assert!(cd.can_eject());
        Ok(())
    }

    #[test]
    fn a_disc_never_changes_what_the_other_devices_list_or_eject() {
        let base = parsed();
        let mut with_discs = parsed();
        with_discs.extend(discs());
        let before = devices(&base);
        let after: Vec<Device> = devices(&with_discs)
            .into_iter()
            .filter(|device| !device.optical)
            .collect();
        assert_eq!(after, before);
        assert!(before.iter().all(|device| !device.optical));
        for disk in [
            "/dev/sdb",
            "/dev/sdc",
            "/dev/sdd",
            "/dev/loop0",
            "/dev/nvme0n1",
        ] {
            assert_eq!(
                eject_plan(&with_discs, Path::new(disk), "x"),
                eject_plan(&base, Path::new(disk), "x"),
                "{disk}"
            );
        }
        let usb = eject_plan(&base, Path::new("/dev/sdb"), "KINGSTON");
        assert_eq!(
            usb.as_ref()
                .map(|plan| (plan.power_off.clone(), plan.tray_eject.clone())),
            Some((Some(PathBuf::from("/dev/sdb")), None))
        );
        assert_eq!(eject_plan(&base, Path::new("/dev/loop0"), "x"), None);
    }

    #[test]
    fn ejecting_a_mounted_disc_unmounts_it_then_opens_the_tray_without_power_off() {
        assert_eq!(
            eject_plan(&discs(), Path::new("/dev/sr0"), "UBUNTU_24"),
            Some(DevicePlan {
                label: "UBUNTU_24".to_owned(),
                unmount: vec![UnmountStep {
                    device: PathBuf::from("/dev/sr0"),
                    mounts: vec![PathBuf::from("/run/media/iperez/UBUNTU_24")],
                    via: UnmountVia::Udisks,
                }],
                power_off: None,
                tray_eject: Some(PathBuf::from("/dev/sr0")),
            })
        );
        let plan = eject_plan(&discs(), Path::new("/dev/sr3"), "Audio CD");
        assert_eq!(
            plan.map(|plan| (plan.unmount.len(), plan.tray_eject, plan.power_off)),
            Some((0, Some(PathBuf::from("/dev/sr3")), None))
        );
        let plan = eject_plan(&discs(), Path::new("/dev/sr2"), "Disc");
        assert!(plan.is_some_and(|plan| plan.unmount.is_empty() && plan.is_eject()));
    }

    #[test]
    fn the_tray_is_opened_by_eject_when_installed_else_by_gio() {
        let dev = Path::new("/dev/sr0");
        let os = |s: &str| OsString::from(s);
        assert_eq!(
            tray_eject_command(
                Some(Path::new("/bin/eject")),
                Some(Path::new("/bin/gio")),
                dev
            ),
            Ok((PathBuf::from("/bin/eject"), vec![os("/dev/sr0")]))
        );
        assert_eq!(
            tray_eject_command(None, Some(Path::new("/bin/gio")), dev),
            Ok((
                PathBuf::from("/bin/gio"),
                vec![os("mount"), os("-e"), os("/dev/sr0")]
            ))
        );
        assert_eq!(
            tray_eject_command(None, None, dev),
            Err(DeviceError::Failed {
                reason: "neither eject nor gio was found".to_owned()
            })
        );
    }

    fn disc_plan() -> DevicePlan {
        eject_plan(&discs(), Path::new("/dev/sr0"), "UBUNTU_24").unwrap_or(DevicePlan {
            label: String::new(),
            unmount: Vec::new(),
            power_off: None,
            tray_eject: None,
        })
    }

    #[test]
    fn a_disc_plan_runs_udisks_unmount_then_eject_and_never_power_off() -> TestResult {
        let fixture = Fixture::new()?;
        let log = fixture.0.join("argv.log");
        let body = |name: &str| format!("echo \"{name} $@\" >> '{}'", log.display());
        let ctl = fixture.script("udisksctl", &body("udisksctl"))?;
        let eject = fixture.script("eject", &body("eject"))?;
        let gio = fixture.script("gio", &body("gio"))?;
        run_plan_with(Some(&ctl), Some(&gio), Some(&eject), &disc_plan())?;
        assert_eq!(
            fs::read_to_string(&log)?,
            "udisksctl unmount --block-device=/dev/sr0 --no-user-interaction\neject /dev/sr0\n"
        );
        Ok(())
    }

    #[test]
    fn a_disc_plan_without_eject_falls_back_to_gio() -> TestResult {
        let fixture = Fixture::new()?;
        let log = fixture.0.join("argv.log");
        let body = |name: &str| format!("echo \"{name} $@\" >> '{}'", log.display());
        let ctl = fixture.script("udisksctl", &body("udisksctl"))?;
        let gio = fixture.script("gio", &body("gio"))?;
        run_plan_with(Some(&ctl), Some(&gio), None, &disc_plan())?;
        assert_eq!(
            fs::read_to_string(&log)?,
            "udisksctl unmount --block-device=/dev/sr0 --no-user-interaction\ngio mount -e /dev/sr0\n"
        );
        Ok(())
    }

    #[test]
    fn a_failing_unmount_keeps_the_tray_closed_and_a_failing_eject_is_reported() -> TestResult {
        let fixture = Fixture::new()?;
        let log = fixture.0.join("argv.log");
        let busy = fixture.script(
            "busy",
            &format!(
                "echo \"$@\" >> '{}'\necho 'GDBus.Error:org.freedesktop.UDisks2.Error.DeviceBusy: target is busy' >&2; exit 1",
                log.display()
            ),
        )?;
        let eject = fixture.script("eject", &format!("echo eject >> '{}'", log.display()))?;
        assert_eq!(
            run_plan_with(Some(&busy), None, Some(&eject), &disc_plan()),
            Err(DeviceError::Failed {
                reason: "target is busy".to_owned()
            })
        );
        assert_eq!(
            fs::read_to_string(&log)?,
            "unmount --block-device=/dev/sr0 --no-user-interaction\n"
        );
        let ctl = fixture.script("udisksctl", "exit 0")?;
        let stuck = fixture.script("stuck", "echo 'eject: unable to eject' >&2; exit 1")?;
        assert_eq!(
            run_plan_with(Some(&ctl), None, Some(&stuck), &disc_plan()),
            Err(DeviceError::Failed {
                reason: "eject: unable to eject".to_owned()
            })
        );
        assert_eq!(
            run_plan_with(Some(&ctl), None, None, &disc_plan()),
            Err(DeviceError::Failed {
                reason: "neither eject nor gio was found".to_owned()
            })
        );
        Ok(())
    }

    #[test]
    fn the_existing_runner_still_cannot_eject_a_tray() {
        assert!(run_plan(Path::new("/bin/true"), &disc_plan()).is_err());
    }
}
