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
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

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
    /// An encrypted container nobody unlocked: shown, never acted on.
    Locked,
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
    /// Powered off after the last unmount (eject only).
    pub power_off: Option<PathBuf>,
}

impl DevicePlan {
    /// Every mount point the plan takes away.
    pub fn mounts(&self) -> Vec<PathBuf> {
        self.unmount
            .iter()
            .flat_map(|step| step.mounts.iter().cloned())
            .collect()
    }
}

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

fn collect_devices(node: &BlockNode, disk: &BlockNode, out: &mut Vec<Device>) {
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
        for child in &node.children {
            collect_devices(child, disk, out);
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
    let state = if fstype == "crypto_LUKS" {
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
        if is_virtual(disk) {
            continue;
        }
        collect_devices(disk, disk, &mut out);
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
    Some(DevicePlan {
        label: device.label.clone(),
        unmount: vec![UnmountStep {
            device: device.path.clone(),
            mounts: if device.mounts.is_empty() {
                vec![mount.to_path_buf()]
            } else {
                device.mounts.clone()
            },
        }],
        power_off: None,
    })
}

fn mounted_below(node: &BlockNode, out: &mut Vec<UnmountStep>) {
    if !node.mountpoints.is_empty() {
        out.push(UnmountStep {
            device: node.path.clone(),
            mounts: node.mountpoints.clone(),
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
    if is_virtual(node) || holds_system(node) || !is_ejectable_disk(node) {
        return None;
    }
    let mut filesystems = Vec::new();
    collect_devices(node, node, &mut filesystems);
    if filesystems.is_empty() {
        return None;
    }
    let mut unmount = Vec::new();
    mounted_below(node, &mut unmount);
    // Stable: equally deep mounts keep their tree order.
    unmount.sort_by_key(|step| std::cmp::Reverse(step.depth()));
    Some(DevicePlan {
        label: label.to_owned(),
        unmount,
        power_off: Some(node.path.clone()),
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
        .map(Path::to_path_buf)
        .or_else(|| home.map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("/"))
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

/// `udisksctl power-off --block-device=<disk> --no-user-interaction`.
pub fn power_off_args(disk: &Path) -> Vec<OsString> {
    udisks_args("power-off", disk)
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
    let mut child = Command::new(program)
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

/// Unmounts one device. A device that is already unmounted counts as done.
pub fn run_unmount(udisksctl: &Path, step: &UnmountStep) -> Result<(), DeviceError> {
    let output = run_tool(udisksctl, &unmount_args(&step.device), UDISKS_TIMEOUT)?;
    let already = output.stderr.contains("NotMounted") || output.stderr.contains("is not mounted");
    if output.success || already {
        Ok(())
    } else {
        Err(DeviceError::Failed {
            reason: failure_reason(&output.stderr),
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

/// Runs `plan`: its unmounts in order, then the power-off. The first
/// failure stops the plan.
pub fn run_plan(udisksctl: &Path, plan: &DevicePlan) -> Result<(), DeviceError> {
    for step in &plan.unmount {
        run_unmount(udisksctl, step)?;
    }
    if let Some(disk) = &plan.power_off {
        run_power_off(udisksctl, disk)?;
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
                }],
                power_off: None,
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
                    },
                    UnmountStep {
                        device: PathBuf::from("/dev/sdb2"),
                        mounts: vec![PathBuf::from("/run/media/iperez/second")],
                    },
                ],
                power_off: Some(PathBuf::from("/dev/sdb")),
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
                },
                UnmountStep {
                    device: PathBuf::from("/dev/sdb2"),
                    mounts: vec![PathBuf::from("/run/media/u/B")],
                },
            ],
            power_off: Some(PathBuf::from("/dev/sdb")),
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
}
