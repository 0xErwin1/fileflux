//! Sidebar discovery: standard places, freedesktop user directories and
//! mounted volumes. Reads only location configuration (`user-dirs.dirs`) and
//! the mount table (`/proc/self/mounts`); never scans the filesystem tree,
//! runs shell commands, evaluates shell variables or creates directories.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

/// One sidebar entry: a stable label plus the exact filesystem path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub label: String,
    pub path: PathBuf,
}

/// A mounted storage volume: its place plus what the mount table and
/// `statvfs` could tell about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    pub label: String,
    pub path: PathBuf,
    /// Mounted under a removable-media root (`/run/media`, `/media`).
    pub removable: bool,
    /// `None` when the filesystem could not be queried: no numbers are made up.
    pub usage: Option<Usage>,
    /// What kind of storage it is: a disk partition or a gvfs mount.
    pub kind: VolumeKind,
}

/// What a sidebar volume is: decides the icon and what can be done with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeKind {
    /// A partition from the mount table.
    Disk,
    /// A phone or camera exposed by gvfs (`mtp:`, `gphoto2:`, `afc:`).
    Phone,
    /// A network share exposed by gvfs (`smb-share:`, `nfs:`, `afp-volume:`).
    Share,
    /// Any other gvfs mount (`sftp:`, `ftp:`, `dav:`, cloud accounts, ...).
    Remote,
}

impl VolumeKind {
    /// Whether gvfs serves it through its FUSE directory: no Trash, no
    /// block-device actions.
    pub fn is_gvfs(self) -> bool {
        self != VolumeKind::Disk
    }
}

impl Volume {
    fn from_place(place: Place) -> Self {
        Self {
            removable: is_removable_mount(&place.path),
            label: place.label,
            path: place.path,
            usage: None,
            kind: VolumeKind::Disk,
        }
    }
}

/// Capacity and free space of a volume, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    pub total: u64,
    pub free: u64,
}

/// Used share above which the meter switches to the warning color.
const HIGH_USAGE: f64 = 0.85;

impl Usage {
    /// Builds usage from `statvfs` counters; `None` for an empty filesystem
    /// or counters that overflow.
    pub fn from_blocks(fragment_size: u64, blocks: u64, available: u64) -> Option<Self> {
        let total = blocks.checked_mul(fragment_size)?;
        let free = available.checked_mul(fragment_size)?.min(total);
        (total > 0).then_some(Self { total, free })
    }

    /// Used share of the capacity in `0.0..=1.0`.
    pub fn used_fraction(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        (1.0 - self.free as f64 / self.total as f64).clamp(0.0, 1.0)
    }

    /// Whether the volume is nearly full (more than 85% used).
    pub fn is_high(&self) -> bool {
        self.used_fraction() > HIGH_USAGE
    }

    /// The sidebar line, e.g. `188 GB free of 498 GB`.
    pub fn label(&self) -> String {
        format!(
            "{} free of {}",
            format_bytes(self.free),
            format_bytes(self.total)
        )
    }
}

/// Queries the filesystem mounted at `path`; any failure is `None`.
pub fn volume_usage(path: &Path) -> Option<Usage> {
    let stat = rustix::fs::statvfs(path).ok()?;
    Usage::from_blocks(stat.f_frsize, stat.f_blocks, stat.f_bavail)
}

/// SI byte formatting: whole bytes below 1 kB, otherwise one decimal under
/// 100 of the unit (trailing `.0` dropped) and none from 100 up.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    value /= 1000.0;
    while (value * 10.0).round() / 10.0 >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    let unit_label = UNITS.get(unit).copied().unwrap_or("PB");
    if value >= 99.95 {
        return format!("{value:.0} {unit_label}");
    }
    let text = format!("{value:.1}");
    let text = text.strip_suffix(".0").unwrap_or(&text);
    format!("{text} {unit_label}")
}

/// Mounts under the removable-media roots are USB-style removable storage.
fn is_removable_mount(path: &Path) -> bool {
    path.starts_with("/run/media") || path.starts_with("/media")
}

// --- gvfs and FUSE ------------------------------------------------------------

/// The directory gvfs exposes its mounts in: `$XDG_RUNTIME_DIR/gvfs`. A
/// missing or relative runtime directory means there is none to look in.
pub fn gvfs_root(runtime_dir: Option<&OsStr>) -> Option<PathBuf> {
    let runtime = Path::new(runtime_dir.filter(|value| !value.is_empty())?);
    runtime.is_absolute().then(|| runtime.join("gvfs"))
}

/// [`gvfs_root`] for the process environment.
pub fn gvfs_root_from_env() -> Option<PathBuf> {
    gvfs_root(std::env::var_os("XDG_RUNTIME_DIR").as_deref())
}

/// How long a gvfs mount may take to answer `statvfs` before its meter is
/// left out: a phone that is locked or asleep can stall it indefinitely.
const GVFS_USAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Looks like the serial number MTP appends to the host name: a long token
/// of letters and digits with at least one digit.
fn is_serial(token: &str) -> bool {
    token.len() >= 6
        && token.chars().all(|c| c.is_ascii_alphanumeric())
        && token.chars().any(|c| c.is_ascii_digit())
}

fn percent_decoded(value: &str) -> String {
    super::trash::decode_percent(value.as_bytes()).map_or_else(
        || value.to_owned(),
        |bytes| String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// The friendly label and kind of a gvfs mount from its directory name
/// (`scheme:key=value,key=value`): `mtp:host=Google_Pixel_8_39060DLJH0031N`
/// is "Google Pixel 8", `smb-share:server=nas,share=media` is "media on
/// nas", `sftp:host=x` is "x". A name that does not parse stays as it is.
pub fn parse_gvfs_name(name: &str) -> (String, VolumeKind) {
    let raw = || (name.to_owned(), VolumeKind::Remote);
    let Some((scheme, fields)) = name.split_once(':') else {
        return raw();
    };
    let field = |key: &str| {
        fields
            .split(',')
            .filter_map(|pair| pair.split_once('='))
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, value)| percent_decoded(value))
            .filter(|value| !value.is_empty())
    };
    match scheme {
        "mtp" | "gphoto2" | "afc" => {
            let Some(host) = field("host") else {
                return raw();
            };
            let mut words: Vec<&str> = host.split('_').filter(|w| !w.is_empty()).collect();
            if words.len() > 1 && words.last().is_some_and(|last| is_serial(last)) {
                words.pop();
            }
            (words.join(" "), VolumeKind::Phone)
        }
        "smb-share" | "afp-volume" | "nfs" => {
            let server = field("server").or_else(|| field("host"));
            let share = field("share").or_else(|| field("volume"));
            match (share, server) {
                (Some(share), Some(server)) => (format!("{share} on {server}"), VolumeKind::Share),
                (None, Some(only)) | (Some(only), None) => (only, VolumeKind::Share),
                (None, None) => raw(),
            }
        }
        _ => match field("host").or_else(|| field("server")) {
            Some(host) => (host, VolumeKind::Remote),
            None => raw(),
        },
    }
}

/// The mounts listed in the gvfs directory `root`. Only the names are read:
/// listing never mounts, stats or opens anything. A missing directory lists
/// nothing, and no usage is filled in.
pub fn gvfs_volumes(root: &Path) -> Vec<Volume> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut names: Vec<OsString> = entries
        .flatten()
        .map(|entry| entry.file_name())
        .filter(|name| !name.as_bytes().starts_with(b"."))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let (label, kind) = parse_gvfs_name(&name.to_string_lossy());
            Volume {
                label,
                path: root.join(&name),
                removable: false,
                usage: None,
                kind,
            }
        })
        .collect()
}

/// Runs `work` on its own thread and gives up on it after `timeout`; a
/// thread stuck in the kernel is left behind instead of stalling the caller.
fn within<T: Send + 'static>(
    timeout: std::time::Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("fileflux-statvfs".to_owned())
        .spawn(move || {
            // The caller may have given up already.
            sender.send(work()).ok();
        })
        .ok()?;
    receiver.recv_timeout(timeout).ok()
}

/// [`volume_usage`] that cannot hang: `None` after [`GVFS_USAGE_TIMEOUT`].
///
/// A thread stuck in the kernel is never joined, so at most one `statvfs` per
/// path is ever in flight: while one still hangs, asking again answers `None`
/// at once instead of leaking another thread on every refresh.
fn bounded_usage(path: &Path) -> Option<Usage> {
    static IN_FLIGHT: LazyLock<Arc<InFlight>> = LazyLock::new(Arc::default);
    bounded_usage_via(&IN_FLIGHT, path, GVFS_USAGE_TIMEOUT, volume_usage)
}

/// [`bounded_usage`] against a given gate and `statvfs`.
fn bounded_usage_via(
    gate: &Arc<InFlight>,
    path: &Path,
    timeout: std::time::Duration,
    query: impl FnOnce(&Path) -> Option<Usage> + Send + 'static,
) -> Option<Usage> {
    let claim = gate.claim(path)?;
    let path = path.to_path_buf();
    within(timeout, move || {
        // Held until the call returns, however late that is.
        let _claim = claim;
        query(&path)
    })
    .flatten()
}

/// The paths with a `statvfs` still running.
#[derive(Debug, Default)]
struct InFlight(Mutex<Vec<PathBuf>>);

/// The right to run a `statvfs` on one path; released on drop.
#[derive(Debug)]
struct Claim {
    gate: Arc<InFlight>,
    path: PathBuf,
}

impl InFlight {
    fn paths(&self) -> std::sync::MutexGuard<'_, Vec<PathBuf>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `None` while another call for `path` has not returned.
    fn claim(self: &Arc<Self>, path: &Path) -> Option<Claim> {
        let mut paths = self.paths();
        if paths.iter().any(|busy| busy == path) {
            return None;
        }
        paths.push(path.to_path_buf());
        Some(Claim {
            gate: Arc::clone(self),
            path: path.to_path_buf(),
        })
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.gate.paths().retain(|busy| *busy != self.path);
    }
}

/// Where FUSE filesystems (and gvfs) are mounted: those paths have weaker
/// semantics than a disk (no atomic no-replace rename, no `fsync`, no
/// timestamps or modes, no Trash), and operations degrade on them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FuseRoots {
    /// Every mount of the table, in table order, with whether it is FUSE or
    /// gvfs: the deepest mount that holds a path decides what the path is.
    mounts: Vec<(PathBuf, bool)>,
    gvfs: Vec<PathBuf>,
}

impl FuseRoots {
    /// From mount table bytes plus the gvfs directory. Mounts of type
    /// `fuse`, `fuse.*` and `fuseblk` count (not `fusectl`); those of type
    /// `fuse.gvfsd-fuse` and the gvfs directory itself are gvfs.
    pub fn from_table(mounts: &[u8], gvfs_root: Option<&Path>) -> Self {
        let mut roots = Self::default();
        for line in mounts.split(|&byte| byte == b'\n') {
            let mut fields = line
                .split(|&byte| byte == b' ' || byte == b'\t')
                .filter(|field| !field.is_empty());
            let (_, Some(mount), Some(fs_type)) = (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let mount = PathBuf::from(OsString::from_vec(decode_fstab_field(mount)));
            let weak = fs_type == b"fuse" || fs_type == b"fuseblk" || fs_type.starts_with(b"fuse.");
            if fs_type == b"fuse.gvfsd-fuse" {
                roots.gvfs.push(mount.clone());
            }
            roots.mounts.push((mount, weak));
        }
        if let Some(root) = gvfs_root {
            let mut known = vec![root.to_path_buf()];
            known.extend(fs::canonicalize(root).ok());
            for root in known {
                roots.gvfs.push(root.clone());
                roots.mounts.push((root, true));
            }
        }
        roots
    }

    /// From the live mount table and `$XDG_RUNTIME_DIR`. An unreadable
    /// table leaves only the gvfs directory.
    pub fn system() -> Self {
        let mounts = fs::read("/proc/self/mounts").unwrap_or_default();
        Self::from_table(&mounts, gvfs_root_from_env().as_deref())
    }

    /// Whether `path` is on a FUSE filesystem or on gvfs: decided by the
    /// deepest mount that holds it, so a disk mounted under a FUSE mount is
    /// still a disk. Of two mounts on the same path the later one (the one
    /// on top) decides.
    pub fn covers(&self, path: &Path) -> bool {
        self.mounts
            .iter()
            .filter(|(mount, _)| path.starts_with(mount))
            .fold((0, false), |best, (mount, weak)| {
                let depth = mount.components().count();
                if depth >= best.0 {
                    (depth, *weak)
                } else {
                    best
                }
            })
            .1
    }

    /// Whether `path` is on gvfs: a phone or a network share that has no
    /// Trash and whose mounts are not block devices.
    pub fn is_gvfs(&self, path: &Path) -> bool {
        self.gvfs.iter().any(|root| path.starts_with(root))
    }
}

/// [`discover_volumes`] against fixed inputs: the mounted partitions of the
/// `mounts` table, then the mounts listed in the `gvfs` directory, each with
/// whatever `usage` answers for its path.
pub fn discover_volumes_with(
    mounts: &[u8],
    user: Option<&OsStr>,
    gvfs: Option<&Path>,
    usage: impl Fn(&Path) -> Option<Usage>,
) -> Vec<Volume> {
    let table = parse_mounts(mounts, user);
    let mut volumes: Vec<Volume> = table.volumes.into_iter().map(Volume::from_place).collect();
    volumes.extend(gvfs.map(gvfs_volumes).unwrap_or_default());
    for volume in &mut volumes {
        volume.usage = usage(&volume.path);
    }
    volumes
}

/// Completed discovery. Configuration problems are explicit warnings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Places {
    pub home: PathBuf,
    pub desktop: PathBuf,
    pub places: Vec<Place>,
    pub volumes: Vec<Volume>,
    pub trash_root: PathBuf,
    /// Where FUSE and gvfs are mounted, read with the volumes (off the UI
    /// thread: resolving the gvfs directory can block on a stuck mount).
    pub fuse: FuseRoots,
    pub warnings: Vec<String>,
}

impl Places {
    /// The volume that holds `path`: the longest mount path that is a
    /// component-wise prefix of it. `None` when no discovered volume
    /// contains the path.
    pub fn volume_containing(&self, path: &Path) -> Option<&Volume> {
        self.volumes
            .iter()
            .filter(|volume| path.starts_with(&volume.path))
            .max_by_key(|volume| volume.path.components().count())
    }
}

/// Enabled entries from `user-dirs.dirs`. Missing entries fall back to the
/// default subdirectory of the home.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserDirs {
    pub desktop: Option<PathBuf>,
    pub documents: Option<PathBuf>,
    pub download: Option<PathBuf>,
    pub music: Option<PathBuf>,
    pub pictures: Option<PathBuf>,
    pub videos: Option<PathBuf>,
    pub warnings: Vec<String>,
}

/// Volumes parsed from the mount table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MountTable {
    pub volumes: Vec<Place>,
    pub warnings: Vec<String>,
}

/// Discovers places and volumes for the current process environment.
///
/// Only environment variables and the mount table are read; no directory is
/// scanned, created or written. A missing `$HOME` is an error.
pub fn discover() -> io::Result<Places> {
    let mut warnings = Vec::new();
    let home = PathBuf::from(OsString::from_vec(
        std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?
            .as_os_str()
            .as_bytes()
            .to_vec(),
    ));
    let data_home = xdg_dir("XDG_DATA_HOME", &mut warnings);
    let config_home = xdg_dir("XDG_CONFIG_HOME", &mut warnings);
    let mounts = match fs::read("/proc/self/mounts") {
        Ok(bytes) => Some(bytes),
        Err(error) => {
            warnings.push(format!("could not read /proc/self/mounts: {error}"));
            None
        }
    };
    let mut discovered = discover_with(
        &home,
        data_home.as_deref(),
        config_home.as_deref(),
        mounts.as_deref(),
    )?;
    warnings.extend(discovered.warnings);
    discovered.warnings = warnings;
    discovered.fuse = FuseRoots::system();
    if let Some(root) = gvfs_root_from_env() {
        discovered.volumes.extend(gvfs_volumes(&root));
    }
    // Discovery already runs off the UI thread, so statvfs may block here.
    for volume in &mut discovered.volumes {
        volume.usage = usage_of_volume(&volume.path);
    }
    Ok(discovered)
}

/// Re-reads the mounted volumes alone: the mount table, the gvfs directory
/// and `statvfs`, for the sidebar's live refresh. `home` only decides which
/// `/run/media/<user>` root counts as the user's. Blocking: call it off the
/// UI thread.
pub fn discover_volumes(home: Option<&Path>) -> io::Result<Vec<Volume>> {
    let mounts = fs::read("/proc/self/mounts")?;
    let gvfs = gvfs_root_from_env();
    Ok(discover_volumes_with(
        &mounts,
        home.and_then(Path::file_name),
        gvfs.as_deref(),
        usage_of_volume,
    ))
}

/// `statvfs` for a listed volume: a gvfs mount's answer is bounded in time.
fn usage_of_volume(path: &Path) -> Option<Usage> {
    match gvfs_root_from_env() {
        Some(root) if path.starts_with(&root) => bounded_usage(path),
        _ => volume_usage(path),
    }
}

/// Environment-aware variable read shared by `discover`: an empty value is
/// unset, a relative value is ignored with a warning.
fn xdg_dir(variable: &str, warnings: &mut Vec<String>) -> Option<PathBuf> {
    let value = std::env::var_os(variable)?;
    if value.is_empty() {
        return None;
    }
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Some(path)
    } else {
        warnings.push(format!(
            "{variable} is relative and was ignored; using the default location"
        ));
        None
    }
}

/// The home Trash from the process environment alone (`$XDG_DATA_HOME`, an
/// absolute value, else `$HOME/.local/share`, then `/Trash`), without any
/// filesystem access, so it is known before places discovery finishes.
/// `None` when `$HOME` is unset or empty.
pub fn startup_trash_root(home: Option<&OsStr>, data_home: Option<&OsStr>) -> Option<PathBuf> {
    let home = Path::new(home.filter(|value| !value.is_empty())?);
    let data_home = data_home.map(Path::new).filter(|path| path.is_absolute());
    Some(home_trash_root(home, data_home))
}

/// `$XDG_DATA_HOME/Trash`, or `home/.local/share/Trash` when it is unset.
fn home_trash_root(home: &Path, data_home: Option<&Path>) -> PathBuf {
    match data_home {
        Some(data_home) => data_home.join("Trash"),
        None => home.join(".local").join("share").join("Trash"),
    }
}

/// Deterministic discovery against fixed fixtures: `data_home` and
/// `config_home` stand in for `$XDG_DATA_HOME` and `$XDG_CONFIG_HOME`
/// (`None` means unset), and `mounts` for `/proc/self/mounts` bytes
/// (`None` means the table could not be read). No filesystem directory is
/// touched except reading `config_home/user-dirs.dirs`.
pub fn discover_with(
    home: &Path,
    data_home: Option<&Path>,
    config_home: Option<&Path>,
    mounts: Option<&[u8]>,
) -> io::Result<Places> {
    if home.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the home location is empty",
        ));
    }
    let config_home = config_home.map_or_else(|| home.join(".config"), Path::to_path_buf);
    let mut warnings = Vec::new();
    let user_dirs = read_user_dirs(&config_home, home, &mut warnings);
    let desktop = user_dirs
        .desktop
        .clone()
        .unwrap_or_else(|| home.join("Desktop"));
    let documents = user_dirs
        .documents
        .clone()
        .unwrap_or_else(|| home.join("Documents"));
    let download = user_dirs
        .download
        .clone()
        .unwrap_or_else(|| home.join("Downloads"));
    let music = user_dirs
        .music
        .clone()
        .unwrap_or_else(|| home.join("Music"));
    let pictures = user_dirs
        .pictures
        .clone()
        .unwrap_or_else(|| home.join("Pictures"));
    let videos = user_dirs
        .videos
        .clone()
        .unwrap_or_else(|| home.join("Videos"));

    // Unavailable default places are labeled normally so navigation can
    // surface the filesystem error itself; nothing is created.
    let places = vec![
        Place {
            label: "Home".to_owned(),
            path: home.to_path_buf(),
        },
        Place {
            label: "Desktop".to_owned(),
            path: desktop.clone(),
        },
        Place {
            label: "Documents".to_owned(),
            path: documents,
        },
        Place {
            label: "Downloads".to_owned(),
            path: download,
        },
        Place {
            label: "Music".to_owned(),
            path: music,
        },
        Place {
            label: "Pictures".to_owned(),
            path: pictures,
        },
        Place {
            label: "Videos".to_owned(),
            path: videos,
        },
    ];

    let trash_root = home_trash_root(home, data_home);

    let mut volumes = Vec::new();
    if let Some(mounts) = mounts {
        let table = parse_mounts(mounts, home.file_name());
        volumes = table.volumes.into_iter().map(Volume::from_place).collect();
        warnings.extend(table.warnings);
    }

    Ok(Places {
        home: home.to_path_buf(),
        desktop,
        places,
        volumes,
        trash_root,
        fuse: FuseRoots::default(),
        warnings,
    })
}

/// Reads and parses `user-dirs.dirs` under the configuration home. A missing
/// file is normal (all defaults apply); other read failures warn.
fn read_user_dirs(config_home: &Path, home: &Path, warnings: &mut Vec<String>) -> UserDirs {
    let config_file = config_home.join("user-dirs.dirs");
    match fs::read(&config_file) {
        Ok(bytes) => {
            let parsed = parse_user_dirs(&String::from_utf8_lossy(&bytes), home);
            warnings.extend(parsed.warnings.clone());
            parsed
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => UserDirs::default(),
        Err(error) => {
            warnings.push(format!(
                "user-dirs configuration {} could not be read: {error}",
                config_file.to_string_lossy()
            ));
            UserDirs::default()
        }
    }
}

/// Parses `user-dirs.dirs` content. Values may be quoted, may start with
/// `$HOME` (the only expanded variable, taken from the fixture home) and may
/// be relative to the home; no shell is involved.
pub fn parse_user_dirs(content: &str, home: &Path) -> UserDirs {
    let mut dirs = UserDirs::default();
    let mut warnings = Vec::new();
    for (position, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            warnings.push(format!(
                "user-dirs entry on line {} was ignored: no '=' separator",
                position + 1
            ));
            continue;
        };
        let key = key.trim();
        let is_key = !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !is_key {
            warnings.push(format!(
                "user-dirs entry on line {} was ignored: invalid key {key:?}",
                position + 1
            ));
            continue;
        }
        let mut value = value.trim();
        for quote in ['"', '\''] {
            value = strip_surrounding(value, quote);
        }
        let path = if value == "$HOME" {
            home.to_path_buf()
        } else if let Some(rest) = value.strip_prefix("$HOME/") {
            home.join(rest)
        } else if value.starts_with('$') {
            warnings.push(format!(
                "user-dirs entry {key} on line {} uses an unsupported variable and was ignored",
                position + 1
            ));
            continue;
        } else {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                path
            } else {
                home.join(path)
            }
        };
        match key {
            "XDG_DESKTOP_DIR" => dirs.desktop = Some(path),
            "XDG_DOCUMENTS_DIR" => dirs.documents = Some(path),
            "XDG_DOWNLOAD_DIR" => dirs.download = Some(path),
            "XDG_MUSIC_DIR" => dirs.music = Some(path),
            "XDG_PICTURES_DIR" => dirs.pictures = Some(path),
            "XDG_VIDEOS_DIR" => dirs.videos = Some(path),
            _ => {}
        }
    }
    dirs.warnings = warnings;
    dirs
}

fn strip_surrounding(value: &str, quote: char) -> &str {
    let inner = value.strip_prefix(quote).unwrap_or(value);
    inner.strip_suffix(quote).unwrap_or(inner)
}

/// Lists mounted disk partitions, including removable USB storage. Device
/// sources, not individual bind/subvolume mountpoints, identify volumes;
/// the filesystem root wins over internal mounts of the same partition.
/// Virtual/network sources and loop/RAM images are not disk partitions.
/// Escaped device and mount paths retain their original bytes.
pub fn parse_mounts(mounts: &[u8], user: Option<&OsStr>) -> MountTable {
    let mut volumes: Vec<Place> = Vec::new();
    let mut warnings = Vec::new();
    let mut seen: Vec<OsString> = Vec::new();
    let mut devices: Vec<OsString> = Vec::new();
    for (position, line) in mounts.split(|&byte| byte == b'\n').enumerate() {
        let line = line.strip_suffix(&b"\r"[..]).unwrap_or(line);
        if line.is_empty() || line.first() == Some(&b'#') {
            continue;
        }
        let mut fields = line
            .split(|&byte| byte == b' ' || byte == b'\t')
            .filter(|field| !field.is_empty());
        let device_field = fields.next();
        let Some(mount_field) = fields.next() else {
            warnings.push(format!(
                "malformed /proc/self/mounts entry on line {} was skipped",
                position + 1
            ));
            continue;
        };
        let Some(fs_type_field) = fields.next() else {
            warnings.push(format!(
                "malformed /proc/self/mounts entry on line {} was skipped",
                position + 1
            ));
            continue;
        };
        let Some(device) = device_field
            .map(decode_fstab_field)
            .map(OsString::from_vec)
            .filter(|source| is_partition_source(source))
        else {
            continue;
        };
        let path = PathBuf::from(OsString::from_vec(decode_fstab_field(mount_field)));
        if !is_visible_mount(&path, OsStr::from_bytes(fs_type_field), user) {
            continue;
        }
        let mount_point = path.as_os_str().to_os_string();
        if seen.contains(&mount_point) {
            continue;
        }
        seen.push(mount_point);
        let label = match path.file_name() {
            Some(file_name) => file_name.to_string_lossy().into_owned(),
            None if path == Path::new("/") => "System".to_owned(),
            None => path.to_string_lossy().into_owned(),
        };
        if let Some(index) = devices.iter().position(|known| known == &device) {
            if path == Path::new("/")
                && let Some(volume) = volumes.get_mut(index)
            {
                *volume = Place { label, path };
            }
            continue;
        }
        devices.push(device);
        volumes.push(Place { label, path });
    }
    volumes.sort_by(|a, b| {
        a.path
            .as_os_str()
            .as_bytes()
            .cmp(b.path.as_os_str().as_bytes())
    });
    MountTable { volumes, warnings }
}

fn is_partition_source(source: &OsStr) -> bool {
    let bytes = source.as_bytes();
    bytes.starts_with(b"/dev/")
        && !["/dev/loop", "/dev/ram", "/dev/zram", "/dev/fuse"]
            .iter()
            .any(|prefix| bytes.starts_with(prefix.as_bytes()))
}

/// Known pseudo and system filesystem types that are never user-visible
/// storage volumes; only these types are excluded, never real storage.
const PSEUDO_FILESYSTEMS: &[&str] = &[
    "proc",
    "sysfs",
    "devtmpfs",
    "devpts",
    "tmpfs",
    "ramfs",
    "cgroup",
    "cgroup2",
    "cgroupfs",
    "mqueue",
    "hugetlbfs",
    "bpf",
    "debugfs",
    "tracefs",
    "configfs",
    "securityfs",
    "pstore",
    "efivarfs",
    "fusectl",
    "squashfs",
    "overlay",
    "autofs",
];

/// Eligible device-backed storage mountpoints: pseudo/system
/// filesystem types are excluded, virtual mounts are excluded by path
/// (`/proc`, `/sys`, `/dev`), `/run` is treated as internal runtime except
/// the user's media roots, and any other absolute mount point (for example
/// `/srv/data`) counts as storage.
fn is_visible_mount(path: &Path, fs_type: &OsStr, user: Option<&OsStr>) -> bool {
    if !path.is_absolute() {
        return false;
    }
    if path == Path::new("/") {
        return true;
    }
    if PSEUDO_FILESYSTEMS
        .iter()
        .any(|pseudo| OsStr::new(pseudo) == fs_type)
    {
        return false;
    }
    if path.starts_with("/proc") || path.starts_with("/sys") || path.starts_with("/dev") {
        return false;
    }
    if path == Path::new("/run") {
        return false;
    }
    if path.starts_with("/run/") {
        let user_media = user.map(|user| Path::new("/run/media").join(user));
        if !user_media.is_some_and(|media| path.starts_with(media)) {
            return false;
        }
    }
    true
}

/// Decodes fstab-style escapes: a backslash followed by exactly three octal
/// digits becomes the byte it names; anything else stays literal.
fn decode_fstab_field(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut rest = field;
    while let Some(first) = rest.first().copied() {
        let digits = rest.get(1..4);
        if first == b'\\'
            && digits.is_some_and(|d| d.len() == 3 && d.iter().all(|b| (b'0'..=b'7').contains(b)))
        {
            let digits = digits.unwrap_or_default();
            let value = digits
                .iter()
                .fold(0u32, |acc, digit| acc * 8 + u32::from(digit - b'0'));
            out.push(value as u8);
            rest = rest.get(4..).unwrap_or_default();
        } else {
            out.push(first);
            rest = rest.get(1..).unwrap_or_default();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

    /// Unique scratch directory outside the repository, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(prefix: &str) -> io::Result<Scratch> {
            let tick = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
                + SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed) as u128;
            let dir = std::env::temp_dir().join(format!(
                "fileflux-places-{prefix}-{}-{tick}",
                std::process::id()
            ));
            fs::create_dir_all(&dir)?;
            Ok(Scratch(dir))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn place(label: &str, path: &Path) -> (String, PathBuf) {
        (label.to_owned(), path.to_path_buf())
    }

    fn labeled(places: &[Place]) -> Vec<(String, PathBuf)> {
        places
            .iter()
            .map(|p| (p.label.clone(), p.path.clone()))
            .collect()
    }

    /// The labeled place with that exact label, for assertions.
    fn find<'a>(places: &'a [Place], label: &str) -> Result<&'a Place, Box<dyn std::error::Error>> {
        places
            .iter()
            .find(|place| place.label == label)
            .ok_or_else(|| Box::<dyn std::error::Error>::from(format!("{label} place missing")))
    }

    #[test]
    fn the_startup_trash_root_needs_only_the_environment() {
        let os = OsStr::new;
        assert_eq!(
            startup_trash_root(Some(os("/h")), None),
            Some(PathBuf::from("/h/.local/share/Trash"))
        );
        assert_eq!(
            startup_trash_root(Some(os("/h")), Some(os("/data"))),
            Some(PathBuf::from("/data/Trash"))
        );
        for ignored in ["", "relative/data"] {
            assert_eq!(
                startup_trash_root(Some(os("/h")), Some(os(ignored))),
                Some(PathBuf::from("/h/.local/share/Trash")),
                "{ignored:?}"
            );
        }
        assert_eq!(startup_trash_root(None, Some(os("/data"))), None);
        assert_eq!(startup_trash_root(Some(os("")), None), None);
    }

    /// Default places, default Trash and no volumes from an isolated home.
    #[test]
    fn default_places_and_trash_come_from_isolated_home() -> TestResult {
        let home = Scratch::new("home")?;

        let discovered = discover_with(home.path(), None, None, None)?;

        let expected = vec![
            place("Home", home.path()),
            place("Desktop", &home.path().join("Desktop")),
            place("Documents", &home.path().join("Documents")),
            place("Downloads", &home.path().join("Downloads")),
            place("Music", &home.path().join("Music")),
            place("Pictures", &home.path().join("Pictures")),
            place("Videos", &home.path().join("Videos")),
        ];
        assert_eq!(labeled(&discovered.places), expected);
        assert_eq!(discovered.home, home.path());
        assert_eq!(discovered.desktop, home.path().join("Desktop"));
        assert_eq!(
            discovered.trash_root,
            home.path().join(".local/share/Trash")
        );
        assert!(discovered.volumes.is_empty());
        assert!(discovered.warnings.is_empty());
        Ok(())
    }

    /// Without `$XDG_CONFIG_HOME` the deterministic helper still reads the
    /// conventional `HOME/.config/user-dirs.dirs`, as `discover()` sees it.
    #[test]
    fn default_config_home_is_home_dot_config() -> TestResult {
        let home = Scratch::new("default-config")?;
        fs::create_dir_all(home.path().join(".config"))?;
        fs::write(
            home.path().join(".config/user-dirs.dirs"),
            "XDG_DESKTOP_DIR=\"$HOME/Lapdesk\"\n",
        )?;

        let discovered = discover_with(home.path(), None, None, None)?;

        assert_eq!(discovered.desktop, home.path().join("Lapdesk"));
        assert_eq!(
            find(&discovered.places, "Desktop")?.path,
            home.path().join("Lapdesk")
        );
        assert!(discovered.warnings.is_empty());
        Ok(())
    }

    /// Configured user directories are honored verbatim (raw bytes kept),
    /// `$HOME` is expanded, relative values resolve against the home and
    /// absolute values are kept as written.
    #[test]
    fn configured_user_dirs_are_honored_verbatim() -> TestResult {
        let home = Scratch::new("udirs-home")?;
        let config = Scratch::new("udirs-config")?;
        fs::create_dir_all(config.path().join("xdg"))?;
        fs::write(
            config.path().join("xdg/user-dirs.dirs"),
            "# a comment\n\
             XDG_DESKTOP_DIR=\"$HOME/Writing Desk\"\n\
             XDG_DOCUMENTS_DIR=\"Docs\"\n\
             XDG_DOWNLOAD_DIR=\"/srv/fixture-downloads\"\n",
        )?;

        let discovered = discover_with(home.path(), None, Some(&config.path().join("xdg")), None)?;

        assert_eq!(discovered.desktop, home.path().join("Writing Desk"));
        assert_eq!(
            find(&discovered.places, "Desktop")?.path,
            home.path().join("Writing Desk")
        );
        assert_eq!(
            find(&discovered.places, "Documents")?.path,
            home.path().join("Docs")
        );
        assert_eq!(
            find(&discovered.places, "Downloads")?.path,
            PathBuf::from("/srv/fixture-downloads")
        );
        assert!(discovered.warnings.is_empty());
        Ok(())
    }

    /// A malformed line and an unsupported shell-style variable produce
    /// explicit warnings; every parseable entry is still honored.
    #[test]
    fn malformed_user_dirs_warn_and_keep_parseable_entries() -> TestResult {
        let home = Scratch::new("badudirs-home")?;
        let config = Scratch::new("badudirs-config")?;
        fs::create_dir_all(config.path())?;
        fs::write(
            config.path().join("user-dirs.dirs"),
            "XDG_DESKTOP_DIR=\"$HOME/Desktop\"\n\
             totally not a valid line\n\
             XDG_DOWNLOAD_DIR=\"$XDG_UNKNOWN_VAR/download\"\n",
        )?;

        let discovered = discover_with(home.path(), None, Some(config.path()), None)?;

        assert_eq!(discovered.warnings.len(), 2);
        assert!(discovered.warnings.iter().any(|w| w.contains("line 2")));
        assert!(
            discovered
                .warnings
                .iter()
                .any(|w| w.contains("XDG_DOWNLOAD_DIR"))
        );
        assert_eq!(discovered.desktop, home.path().join("Desktop"));
        assert_eq!(
            find(&discovered.places, "Downloads")?.path,
            home.path().join("Downloads")
        );
        Ok(())
    }

    /// fstab-style octal escapes in mount fields are decoded, so a mount
    /// point containing a space keeps its exact bytes.
    #[test]
    fn mount_paths_decode_fstab_octal_escapes() -> TestResult {
        let table = parse_mounts(b"/dev/sda1 /media/fix\\040disk ext4 rw 0 0\n", None);

        assert!(table.warnings.is_empty());
        let first = table.volumes.first().ok_or("expected one volume")?;
        assert_eq!(
            first.path,
            PathBuf::from(OsString::from_vec(b"/media/fix disk".to_vec()))
        );
        Ok(())
    }

    /// A device-backed root is shown; mounted partitions remain visible at arbitrary
    /// absolute paths, while pseudo and internal runtime mounts are excluded
    /// by path and filesystem type instead of a storage allowlist.
    #[test]
    fn volumes_keep_user_storage_and_exclude_virtual_roots() -> TestResult {
        let mounts = b"\
/dev/root / ext4 rw 0 0\n\
procfs /proc proc rw 0 0\n\
sysfs /sys sysfs rw 0 0\n\
devtmpfs /dev devtmpfs rw 0 0\n\
tmpfs /run/user/1000 tmpfs rw 0 0\n\
gvfsd-fuse /run/user/1000/gvfs fuse.gvfsd-fuse rw 0 0\n\
efivarfs /sys/firmware/efi/efivars efivarfs rw 0 0\n\
tmpmount /run/tmpfile tmpfs rw 0 0\n\
/dev/sdb1 /media/USBDRIVE vfat rw 0 0\n\
/dev/sdc1 /mnt/data ext4 rw 0 0\n\
/dev/sdd1 /run/media/iperez/SSD ext4 rw 0 0\n\
/dev/sde1 /run/media/otheruser/nousb ext4 rw 0 0\n\
/dev/sdf1 /srv/data ext4 rw 0 0\n\
/dev/sdg1 /var/backup ext4 rw 0 0\n\
snapfuse /snap/core22/1 squashfs ro 0 0\n\
/dev/sdh1 /boot/efi vfat rw 0 0\n";

        let table = parse_mounts(mounts, Some(OsStr::from_bytes(b"iperez")));

        assert!(table.warnings.is_empty());
        let paths: Vec<String> = table
            .volumes
            .iter()
            .map(|v| v.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            paths,
            vec![
                "/",
                "/boot/efi",
                "/media/USBDRIVE",
                "/mnt/data",
                "/run/media/iperez/SSD",
                "/srv/data",
                "/var/backup",
            ]
        );
        Ok(())
    }

    /// Free space belongs to the volume with the longest mount-path prefix.
    #[test]
    fn the_containing_volume_is_the_longest_mount_prefix() {
        let volume = |label: &str, path: &str| Volume {
            label: label.to_owned(),
            path: PathBuf::from(path),
            removable: false,
            usage: None,
            kind: VolumeKind::Disk,
        };
        let places = Places {
            home: PathBuf::from("/home/me"),
            desktop: PathBuf::from("/home/me/Desktop"),
            places: Vec::new(),
            volumes: vec![
                volume("System", "/"),
                volume("home", "/home"),
                volume("data", "/mnt/data"),
            ],
            trash_root: PathBuf::from("/home/me/.local/share/Trash"),
            fuse: FuseRoots::default(),
            warnings: Vec::new(),
        };
        let label = |path: &str| {
            places
                .volume_containing(Path::new(path))
                .map(|v| v.label.as_str())
        };
        assert_eq!(label("/home/me/Pictures"), Some("home"));
        assert_eq!(label("/mnt/data/x/y"), Some("data"));
        assert_eq!(label("/mnt/database"), Some("System"), "not a prefix");
        assert_eq!(label("/etc"), Some("System"));
        assert_eq!(label("relative"), None);
    }

    /// The filesystem root reads "System" in the sidebar, not a bare "/".
    #[test]
    fn the_root_mount_is_labeled_system() {
        let table = parse_mounts(
            b"/dev/sda2 / ext4 rw 0 0\n/dev/sdb1 /mnt/data ext4 rw 0 0\n",
            None,
        );
        let labels: Vec<_> = table.volumes.iter().map(|v| v.label.as_str()).collect();
        assert_eq!(labels, ["System", "data"]);
    }

    /// Real storage mounted outside `/media`, `/mnt` and `/run/media` is a
    /// mounted volume, not hidden; tmpfs, proc and sysfs stay excluded.
    #[test]
    fn srv_data_storage_included_and_runtime_excluded() -> TestResult {
        let mounts = b"\
/dev/sda1 /srv/data ext4 rw 0 0\n\
procfs /proc proc rw 0 0\n\
sysfs /sys/kernel sysfs rw 0 0\n\
tmpmount /run/temp tmpfs rw 0 0\n";

        let table = parse_mounts(mounts, Some(OsStr::from_bytes(b"iperez")));

        let paths: Vec<String> = table
            .volumes
            .iter()
            .map(|v| v.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, vec!["/srv/data"]);
        Ok(())
    }

    #[test]
    fn usage_fraction_threshold_and_label() -> TestResult {
        let usage = Usage::from_blocks(4096, 1_000, 250).ok_or("non-empty filesystem")?;
        assert_eq!(usage.total, 4_096_000);
        assert_eq!(usage.free, 1_024_000);
        assert!((usage.used_fraction() - 0.75).abs() < 1e-9);
        assert!(!usage.is_high());
        // Exactly 85% used is not yet high; just above is.
        assert!(
            !Usage {
                total: 100,
                free: 15
            }
            .is_high()
        );
        assert!(
            Usage {
                total: 100,
                free: 14
            }
            .is_high()
        );
        assert_eq!(
            Usage {
                total: 498_000_000_000,
                free: 188_000_000_000
            }
            .label(),
            "188 GB free of 498 GB"
        );
        assert_eq!(
            Usage {
                total: 32_000_000_000,
                free: 19_800_000_000
            }
            .label(),
            "19.8 GB free of 32 GB"
        );
        Ok(())
    }

    #[test]
    fn usage_rejects_empty_or_overflowing_counters() -> TestResult {
        assert_eq!(Usage::from_blocks(4096, 0, 0), None);
        assert_eq!(Usage::from_blocks(u64::MAX, 2, 1), None);
        // More available than total is clamped, never negative usage.
        let clamped = Usage::from_blocks(1, 10, 20).ok_or("non-empty")?;
        assert_eq!(clamped.free, 10);
        assert_eq!(clamped.used_fraction(), 0.0);
        Ok(())
    }

    #[test]
    fn format_bytes_uses_si_units_with_artboard_precision() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_000), "1 KB");
        assert_eq!(format_bytes(1_500_000), "1.5 MB");
        assert_eq!(format_bytes(19_800_000_000), "19.8 GB");
        assert_eq!(format_bytes(32_000_000_000), "32 GB");
        assert_eq!(format_bytes(188_000_000_000), "188 GB");
        assert_eq!(format_bytes(931_000_000_000), "931 GB");
        assert_eq!(format_bytes(99_960_000_000), "100 GB");
        assert_eq!(format_bytes(999_950_000_000), "1 TB");
        assert_eq!(format_bytes(2_000_000_000_000), "2 TB");
    }

    #[test]
    fn statvfs_failure_leaves_no_usage() {
        assert_eq!(
            volume_usage(Path::new("/nonexistent/fileflux-volume")),
            None
        );
        assert!(volume_usage(Path::new("/")).is_some());
    }

    #[test]
    fn removable_media_roots_are_flagged() -> TestResult {
        let mounts = b"/dev/sda1 /run/media/me/USB vfat rw 0 0\n/dev/sdb1 /mnt/data ext4 rw 0 0\n";
        let discovered = discover_with(Path::new("/home/me"), None, None, Some(mounts))?;
        let flags: Vec<(String, bool, bool)> = discovered
            .volumes
            .iter()
            .map(|v| (v.label.clone(), v.removable, v.usage.is_none()))
            .collect();
        assert_eq!(
            flags,
            vec![
                ("data".to_owned(), false, true),
                ("USB".to_owned(), true, true)
            ]
        );
        Ok(())
    }

    #[test]
    fn volumes_are_mounted_partitions_and_usb_not_internal_bind_mounts() {
        let mounts = b"\
/dev/nvme0n1p2 /nix/store btrfs rw 0 0\n\
/dev/nvme0n1p2 / btrfs rw 0 0\n\
/dev/nvme0n1p2 /home btrfs rw 0 0\n\
/dev/nvme0n1p2 /swap btrfs rw 0 0\n\
/dev/nvme0n1p1 /boot vfat rw 0 0\n\
/dev/mapper/data /mnt/data ext4 rw 0 0\n\
/dev/sdb1 /run/media/fixture/USB\\040Disk exfat rw 0 0\n\
/dev/sdb1 /mnt/usb-bind exfat rw 0 0\n\
/dev/loop0 /mnt/image ext4 rw 0 0\n\
/dev/fuse /mnt/fuse fuseblk rw 0 0\n\
server:/share /mnt/network nfs rw 0 0\n\
tmpfs /run tmpfs rw 0 0\n";
        let table = parse_mounts(mounts, Some(OsStr::new("fixture")));
        let paths: Vec<_> = table
            .volumes
            .iter()
            .map(|place| place.path.as_path())
            .collect();
        assert_eq!(
            paths,
            [
                Path::new("/"),
                Path::new("/boot"),
                Path::new("/mnt/data"),
                Path::new("/run/media/fixture/USB Disk")
            ]
        );
        assert!(table.warnings.is_empty());
        assert!(
            parse_mounts(b"overlay / overlay rw 0 0\n", None)
                .volumes
                .is_empty()
        );
    }

    /// A mount point listed twice is deduplicated.
    #[test]
    fn repeated_mount_points_are_deduplicated() -> TestResult {
        let mounts = b"\
/dev/sdb1 /mnt/data ext4 rw 0 0\n\
/dev/sdb2 /mnt/data ext4 rw 0 0\n";

        let table = parse_mounts(mounts, Some(OsStr::from_bytes(b"iperez")));

        assert_eq!(table.volumes.len(), 1);
        let first = table.volumes.first().ok_or("expected one volume")?;
        assert_eq!(first.path, PathBuf::from("/mnt/data"));
        Ok(())
    }

    /// The environment entry point runs against a fully isolated child
    /// process environment: `$HOME` from the fixture, no personal user
    /// configuration is read, the conventional config home applies, and the
    /// collected environment warnings survive into `Places.warnings`.
    #[test]
    fn discover_child_environment_isolation() -> TestResult {
        const REPORT_VAR: &str = "FILEFLUX_DISCOVERY_REPORT";
        if let Some(report_path) = std::env::var_os(REPORT_VAR) {
            let discovered = discover()?;
            let mut report = String::new();
            report.push_str(&format!("home={}\n", discovered.home.display()));
            report.push_str(&format!("desktop={}\n", discovered.desktop.display()));
            report.push_str(&format!("trash={}\n", discovered.trash_root.display()));
            for warning in &discovered.warnings {
                report.push_str(&format!("warning={warning}\n"));
            }
            for volume in &discovered.volumes {
                report.push_str(&format!("volume={}\n", volume.path.display()));
            }
            fs::write(report_path, report)?;
            return Ok(());
        }

        let home = Scratch::new("child")?;
        let report_dir = Scratch::new("child-report")?;
        fs::create_dir_all(home.path().join(".config"))?;
        fs::write(
            home.path().join(".config/user-dirs.dirs"),
            "XDG_DESKTOP_DIR=\"$HOME/Lapdesk\"\n",
        )?;
        let report_path = report_dir.path().join("report.txt");

        let exe = std::env::current_exe()?;
        let output = Command::new(exe)
            .arg("core::places::tests::discover_child_environment_isolation")
            .arg("--exact")
            .env(REPORT_VAR, &report_path)
            .env("HOME", home.path())
            .env("XDG_DATA_HOME", "relative/junk")
            .env_remove("XDG_CONFIG_HOME")
            .output()?;
        assert!(
            output.status.success(),
            "child test failed: {}",
            String::from_utf8_lossy(&output.stdout)
        );

        let report_text = fs::read_to_string(&report_path)?;
        assert_eq!(
            report_text
                .lines()
                .filter(|line| line.starts_with("warning="))
                .count(),
            1,
            "environment warnings must survive into the report"
        );
        let contains = |needle: &str| report_text.lines().any(|line| line.contains(needle));
        assert!(contains(&format!("home={}", home.path().display())));
        assert!(contains(&format!(
            "desktop={}",
            home.path().join("Lapdesk").display()
        )));
        assert!(contains(&format!(
            "trash={}",
            home.path().join(".local/share/Trash").display()
        )));
        assert!(contains("XDG_DATA_HOME"), "relative-variable warning kept");
        assert!(!contains("could not read /proc/self/mounts"));
        assert!(
            report_text
                .lines()
                .filter_map(|line| line.strip_prefix("volume="))
                .all(|path| Path::new(path).is_absolute()),
            "reported partition mountpoints must be absolute; a container root need not be a disk"
        );
        Ok(())
    }
    // --- gvfs and FUSE -------------------------------------------------------

    #[test]
    fn the_gvfs_root_is_below_the_runtime_directory() {
        let os = OsStr::new;
        assert_eq!(
            gvfs_root(Some(os("/run/user/1000"))),
            Some(PathBuf::from("/run/user/1000/gvfs"))
        );
        assert_eq!(gvfs_root(None), None);
        assert_eq!(gvfs_root(Some(os(""))), None);
        assert_eq!(gvfs_root(Some(os("relative"))), None, "never relative");
    }

    #[test]
    fn gvfs_names_become_friendly_labels() {
        use VolumeKind::{Phone, Remote, Share};
        let cases = [
            (
                "mtp:host=Google_Pixel_8_39060DLJH0031N",
                "Google Pixel 8",
                Phone,
            ),
            ("mtp:host=Fake_Phone_123", "Fake Phone 123", Phone),
            ("mtp:host=Solo", "Solo", Phone),
            ("smb-share:server=nas,share=media", "media on nas", Share),
            ("smb-share:share=media,server=nas", "media on nas", Share),
            (
                "smb-share:server=my%20nas,share=a%2Fb",
                "a/b on my nas",
                Share,
            ),
            ("sftp:host=x", "x", Remote),
            ("sftp:host=x,user=bob", "x", Remote),
            ("ftp:host=files.example.org", "files.example.org", Remote),
            ("something", "something", Remote),
            ("weird:nothing=1", "weird:nothing=1", Remote),
            ("sftp:host=", "sftp:host=", Remote),
        ];
        for (name, label, kind) in cases {
            assert_eq!(
                parse_gvfs_name(name),
                (label.to_owned(), kind),
                "gvfs name {name:?}"
            );
        }
    }

    #[test]
    fn the_gvfs_directory_lists_its_mounts_by_name_only() -> TestResult {
        let scratch = Scratch::new("gvfs-list")?;
        let root = scratch.path().join("run").join("gvfs");
        fs::create_dir_all(
            root.join("mtp:host=Fake_Phone_123")
                .join("Internal shared storage"),
        )?;
        fs::create_dir_all(root.join("sftp:host=x"))?;
        fs::write(root.join(".hidden"), b"")?;

        let volumes = gvfs_volumes(&root);

        let listed: Vec<_> = volumes
            .iter()
            .map(|v| (v.label.as_str(), v.kind, v.removable, v.usage))
            .collect();
        assert_eq!(
            listed,
            [
                ("Fake Phone 123", VolumeKind::Phone, false, None),
                ("x", VolumeKind::Remote, false, None),
            ]
        );
        assert_eq!(
            volumes.first().map(|v| v.path.clone()),
            Some(root.join("mtp:host=Fake_Phone_123"))
        );
        assert!(gvfs_volumes(&scratch.path().join("absent")).is_empty());
        Ok(())
    }

    #[test]
    fn discovery_lists_disks_then_gvfs_with_usage_only_where_it_answers() -> TestResult {
        let scratch = Scratch::new("gvfs-discover")?;
        let root = scratch.path().join("gvfs");
        fs::create_dir_all(root.join("mtp:host=Fake_Phone_123"))?;
        fs::create_dir_all(root.join("smb-share:server=nas,share=media"))?;
        let mounts = b"/dev/sda2 / ext4 rw 0 0\n\
gvfsd-fuse /run/user/1000/gvfs fuse.gvfsd-fuse rw 0 0\n";
        let phone = root.join("mtp:host=Fake_Phone_123");
        let answer = |path: &Path| {
            if path == Path::new("/") || path == phone {
                Usage::from_blocks(4096, 1000, 250)
            } else {
                // statvfs failed, or reported zero blocks.
                Usage::from_blocks(4096, 0, 0)
            }
        };

        let volumes = discover_volumes_with(mounts, None, Some(&root), answer);

        let kinds: Vec<_> = volumes.iter().map(|v| (v.label.as_str(), v.kind)).collect();
        assert_eq!(
            kinds,
            [
                ("System", VolumeKind::Disk),
                ("Fake Phone 123", VolumeKind::Phone),
                ("media on nas", VolumeKind::Share),
            ]
        );
        assert!(
            volumes.get(1).is_some_and(|v| v.usage.is_some()),
            "the phone answered statvfs"
        );
        assert_eq!(
            volumes.get(2).map(|v| v.usage),
            Some(None),
            "zero blocks: no meter"
        );
        // Without a gvfs directory only the disks are listed.
        let plain = discover_volumes_with(mounts, None, None, answer);
        assert_eq!(plain.len(), 1);
        Ok(())
    }

    #[test]
    fn a_statvfs_that_hangs_is_given_up_on() {
        use std::time::Duration;
        let slow = within(Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_millis(600));
            7
        });
        assert_eq!(slow, None);
        assert_eq!(within(Duration::from_secs(5), || 7), Some(7));
    }

    #[test]
    fn fuse_roots_cover_fuse_mounts_and_gvfs_only() -> TestResult {
        let scratch = Scratch::new("fuse-roots")?;
        let gvfs = scratch.path().join("run").join("gvfs");
        let mounts = b"/dev/sda2 / ext4 rw 0 0\n\
gvfsd-fuse /run/user/1000/gvfs fuse.gvfsd-fuse rw 0 0\n\
me@host:/ /mnt/remote fuse.sshfs rw 0 0\n\
/dev/sdb1 /mnt/ntfs fuseblk rw 0 0\n\
fusectl /sys/fs/fuse/connections fusectl rw 0 0\n\
/dev/sdc1 /mnt/plain ext4 rw 0 0\n";

        let roots = FuseRoots::from_table(mounts, Some(&gvfs));

        for path in [
            "/mnt/remote/dir/file",
            "/mnt/ntfs",
            "/run/user/1000/gvfs/mtp:host=P/Internal/DCIM",
        ] {
            assert!(roots.covers(Path::new(path)), "{path} is FUSE");
        }
        for path in ["/mnt/plain/x", "/", "/home/me", "/mnt/remote2"] {
            assert!(!roots.covers(Path::new(path)), "{path} is not FUSE");
        }
        assert!(roots.covers(&gvfs.join("mtp:host=Fake_Phone_123")));
        assert!(roots.is_gvfs(&gvfs.join("mtp:host=Fake_Phone_123/DCIM")));
        assert!(roots.is_gvfs(Path::new("/run/user/1000/gvfs/smb-share:server=a,share=b")));
        assert!(
            !roots.is_gvfs(Path::new("/mnt/remote/x")),
            "sshfs is FUSE, not gvfs"
        );
        assert!(!FuseRoots::default().covers(Path::new("/mnt/remote")));
        Ok(())
    }

    #[test]
    fn the_deepest_mount_decides_whether_a_path_is_on_fuse() {
        let mounts = b"/dev/sda2 / ext4 rw 0 0\n\
me@host:/ /mnt/remote fuse.sshfs rw 0 0\n\
/dev/sdc1 /mnt/remote/usb ext4 rw 0 0\n\
/dev/sdd1 /mnt/remote/usb/ntfs fuseblk rw 0 0\n\
/dev/sde1 /mnt/stack ext4 rw 0 0\n\
other:/ /mnt/stack fuse.sshfs rw 0 0\n\
other:/ /mnt/stack2 fuse.sshfs rw 0 0\n\
/dev/sde2 /mnt/stack2 ext4 rw 0 0\n";

        let roots = FuseRoots::from_table(mounts, None);

        for (path, fuse) in [
            ("/mnt/remote/file", true),
            ("/mnt/remote/usb", false),
            ("/mnt/remote/usb/photos/a.jpg", false),
            ("/mnt/remote/usb/ntfs/doc", true),
            ("/mnt/remote/usb2/x", true),
            ("/home/me", false),
            ("/mnt/stack/x", true),
            ("/mnt/stack2/x", false),
        ] {
            assert_eq!(roots.covers(Path::new(path)), fuse, "{path}");
        }
    }

    #[test]
    fn a_disk_mounted_inside_the_gvfs_directory_is_a_disk() {
        let gvfs = Path::new("/run/user/1000/gvfs");
        let mounts = b"gvfsd-fuse /run/user/1000/gvfs fuse.gvfsd-fuse rw 0 0\n\
/dev/sdb1 /run/user/1000/gvfs/mtp:host=P/disk ext4 rw 0 0\n";

        let roots = FuseRoots::from_table(mounts, Some(gvfs));

        assert!(roots.covers(Path::new("/run/user/1000/gvfs/mtp:host=P/DCIM")));
        assert!(!roots.covers(Path::new("/run/user/1000/gvfs/mtp:host=P/disk/x")));
    }

    #[test]
    fn a_hung_statvfs_is_not_asked_again_until_it_returns() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, Instant};
        let gate = Arc::new(InFlight::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let usage = || Usage::from_blocks(1, 10, 5);
        let path = Path::new("/run/user/1000/gvfs/mtp:host=P");
        let (release, hung) = std::sync::mpsc::channel::<()>();
        let counted = Arc::clone(&calls);

        let first = bounded_usage_via(&gate, path, Duration::from_millis(50), move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            hung.recv().ok();
            None
        });
        assert_eq!(first, None, "the hung call is given up on");

        for _ in 0..5 {
            let counted = Arc::clone(&calls);
            let again = bounded_usage_via(&gate, path, Duration::from_millis(50), move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                usage()
            });
            assert_eq!(again, None);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no thread per refresh");
        let other = Path::new("/run/user/1000/gvfs/mtp:host=Q");
        assert_eq!(
            bounded_usage_via(&gate, other, Duration::from_secs(5), move |_| usage()),
            usage(),
            "another mount is independent"
        );

        release.send(()).ok();
        let started = Instant::now();
        while gate.claim(path).is_none() {
            assert!(started.elapsed() < Duration::from_secs(10), "never freed");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            bounded_usage_via(&gate, path, Duration::from_secs(5), move |_| usage()),
            usage(),
            "asked again once the hung call returned"
        );
    }
}
