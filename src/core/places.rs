//! Sidebar discovery: standard places, freedesktop user directories and
//! mounted volumes. Reads only location configuration (`user-dirs.dirs`) and
//! the mount table (`/proc/self/mounts`); never scans the filesystem tree,
//! runs shell commands, evaluates shell variables or creates directories.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// One sidebar entry: a stable label plus the exact filesystem path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub label: String,
    pub path: PathBuf,
}

/// Completed discovery. Configuration problems are explicit warnings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Places {
    pub home: PathBuf,
    pub desktop: PathBuf,
    pub places: Vec<Place>,
    pub volumes: Vec<Place>,
    pub trash_root: PathBuf,
    pub warnings: Vec<String>,
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
    Ok(discovered)
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

    let trash_root = match data_home {
        Some(data_home) => data_home.join("Trash"),
        None => home.join(".local").join("share").join("Trash"),
    };

    let mut volumes = Vec::new();
    if let Some(mounts) = mounts {
        let table = parse_mounts(mounts, home.file_name());
        volumes = table.volumes;
        warnings.extend(table.warnings);
    }

    Ok(Places {
        home: home.to_path_buf(),
        desktop,
        places,
        volumes,
        trash_root,
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
}
