//! "Open as Administrator": a separate Fileflux instance started as root on
//! one folder, the way PCManFM and Nautilus-admin do it.
//!
//! Fileflux asks `pkexec` to run itself with `--new-instance <folder>`.
//! With the polkit action `dev.fileflux.open-as-admin` installed (its
//! `exec.path` is the installed launcher and `exec.allow_gui` keeps
//! `DISPLAY` and `XAUTHORITY`) the command is
//! `pkexec <fileflux> --new-instance -- <folder>`. Without it (`cargo run`,
//! a package that does not ship the policy) it is
//! `pkexec env DISPLAY=… XAUTHORITY=… <fileflux> --new-instance -- <folder>`,
//! passing only those two variables and only when set. Neither path ever
//! carries `DBUS_SESSION_BUS_ADDRESS`, `WAYLAND_DISPLAY` or
//! `XDG_RUNTIME_DIR`: the root instance talks X11 (XWayland), keeps its own
//! state under root's home and never forwards to or claims the user's
//! single-instance name.
//!
//! Everything here is pure or takes its side effect as a parameter, so no
//! test runs pkexec. Commands are argv vectors, never shell strings, and
//! every program is found at a fixed absolute location, never through
//! `PATH`.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use super::Location;
use super::elevate::{PKEXEC_CANDIDATES, is_executable_file, resolve};

/// The command-line flag that starts an instance that never talks to the
/// user's single-instance D-Bus name.
pub const NEW_INSTANCE_FLAG: &str = "--new-instance";

/// The text of the fixed banner a root instance shows.
pub const BANNER: &str = "Running as administrator \u{2014} changes affect the whole system";

/// What a root instance's window titles end with.
pub const ROOT_TITLE_SUFFIX: &str = " (root)";

/// The polkit policy file this module looks for.
pub const POLICY_FILE: &str = "dev.fileflux.open-as-admin.policy";

/// Where `env` may be.
pub const ENV_CANDIDATES: &[&str] = &["/run/current-system/sw/bin/env", "/usr/bin/env", "/bin/env"];

const EXEC_PATH_KEY: &str = "key=\"org.freedesktop.policykit.exec.path\">";
const ALLOW_GUI_KEY: &str = "key=\"org.freedesktop.policykit.exec.allow_gui\">";

/// `title`, with the root suffix when the process runs as root.
#[must_use]
pub fn window_title(title: &str, root: bool) -> String {
    if root {
        format!("{title}{ROOT_TITLE_SUFFIX}")
    } else {
        title.to_owned()
    }
}

/// Whether this instance takes part in the single-instance protocol: a
/// started-by-request instance and any root process stay out of it.
#[must_use]
pub fn claims_session_name(new_instance: bool, root: bool) -> bool {
    !new_instance && !root
}

/// The path pkexec must be given for the running executable `exe`: the Nix
/// wrapper `.name-wrapped` is the real file, but the policy names the
/// wrapper `name` beside it.
#[must_use]
pub fn launcher_of(exe: &Path) -> PathBuf {
    let unwrapped = exe
        .file_name()
        .and_then(OsStr::to_str)
        .and_then(|name| name.strip_prefix('.')?.strip_suffix("-wrapped"))
        .filter(|name| !name.is_empty());
    match unwrapped {
        Some(name) => exe.with_file_name(name),
        None => exe.to_path_buf(),
    }
}

/// The release channel a launcher belongs to, from its file name: `fileflux`
/// is the source package (no channel) and `fileflux-<channel>` (for example
/// `fileflux-nightly`, also as the Nix `.fileflux-nightly-wrapped`) is that
/// channel's binary package. A channel is letters and digits only, so it can
/// never name another directory.
#[must_use]
pub fn channel_of(exe: &Path) -> Option<String> {
    let launcher = launcher_of(exe);
    let channel = launcher
        .file_name()?
        .to_str()?
        .strip_prefix("fileflux-")?
        .to_owned();
    (!channel.is_empty() && channel.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        .then_some(channel)
}

/// The policy file `launcher`'s package installs: `POLICY_FILE` for the
/// source package, `dev.fileflux.open-as-admin-<channel>.policy` for a
/// channel's (the action id carries the same suffix).
#[must_use]
pub fn policy_file_of(launcher: &Path) -> String {
    match channel_of(launcher) {
        Some(channel) => format!("dev.fileflux.open-as-admin-{channel}.policy"),
        None => POLICY_FILE.to_owned(),
    }
}

/// Whether the policy text names `launcher` as the executable and allows
/// the GUI environment through.
#[must_use]
pub fn policy_allows(policy: &str, launcher: &Path) -> bool {
    let annotation = |key: &str| {
        let (_, rest) = policy.split_once(key)?;
        let (text, _) = rest.split_once("</annotate>")?;
        Some(text)
    };
    // polkit compares the annotation with the program path as plain text.
    annotation(EXEC_PATH_KEY).is_some_and(|path| launcher.to_str() == Some(path))
        && annotation(ALLOW_GUI_KEY) == Some("true")
}

/// Where the policy may be installed: only the directories `polkitd` reads
/// actions from. A copy inside the package (`nix run`, a profile, a
/// home-manager link) is never seen by polkit, so it must not count: the
/// generic action would apply and strip `DISPLAY` and `XAUTHORITY`. The
/// launcher only picks the file name (see [`policy_file_of`]); it is the
/// policy's `exec.path` that has to match it.
#[must_use]
pub fn policy_candidates(launcher: &Path) -> Vec<PathBuf> {
    [
        "/etc/polkit-1/actions",
        "/usr/local/share/polkit-1/actions",
        "/usr/share/polkit-1/actions",
        "/run/current-system/sw/share/polkit-1/actions",
    ]
    .iter()
    .map(|dir| Path::new(dir).join(policy_file_of(launcher)))
    .collect()
}

/// Why a launch could not be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchError {
    Pkexec,
    Env,
    /// Our own executable could not be found.
    Launcher,
    /// The folder is not absolute: pkexec does not run in our folder.
    RelativeFolder,
    /// `env` would read a launcher path holding `=` as `NAME=VALUE`.
    LauncherHasEquals,
}

impl fmt::Display for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Pkexec => {
                "Open as Administrator needs root: install polkit (the pkexec program) to enable it"
            }
            Self::Env => "Open as Administrator needs the env program, which was not found",
            Self::Launcher => "Open as Administrator could not find the Fileflux executable",
            Self::RelativeFolder => "Open as Administrator needs an absolute folder path",
            Self::LauncherHasEquals => {
                "Open as Administrator cannot start Fileflux from a path containing '=' without the polkit policy installed"
            }
        })
    }
}

impl std::error::Error for LaunchError {}

/// What a root launch runs, by absolute path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub pkexec: PathBuf,
    /// The Fileflux executable pkexec runs (see [`launcher_of`]).
    pub launcher: PathBuf,
    /// Used only when the policy is not installed.
    pub env: PathBuf,
    /// Whether the polkit action for `launcher` is installed.
    pub policy_installed: bool,
    pub display: Option<OsString>,
    pub xauthority: Option<OsString>,
}

impl Launch {
    /// Looks for pkexec, the launcher and its policy at their fixed places
    /// and reads `DISPLAY` and `XAUTHORITY` from our environment.
    pub fn detect() -> Result<Self, LaunchError> {
        let pkexec = resolve(PKEXEC_CANDIDATES, is_executable_file).ok_or(LaunchError::Pkexec)?;
        let exe = std::env::current_exe().map_err(|_| LaunchError::Launcher)?;
        let launcher = launcher_of(&exe);
        let policy_installed = policy_candidates(&launcher).iter().any(|candidate| {
            std::fs::read_to_string(candidate).is_ok_and(|policy| policy_allows(&policy, &launcher))
        });
        let env = match resolve(ENV_CANDIDATES, is_executable_file) {
            Some(env) => env,
            None if policy_installed => PathBuf::from("/usr/bin/env"),
            None => return Err(LaunchError::Env),
        };
        let var = |name| std::env::var_os(name).filter(|value| !value.is_empty());
        Ok(Self {
            pkexec,
            launcher,
            env,
            policy_installed,
            display: var("DISPLAY"),
            xauthority: var("XAUTHORITY"),
        })
    }

    /// The full command line, program first. `folder` is one argument after
    /// `--`, so it can hold spaces or start with `-`; a relative one is
    /// refused.
    pub fn argv(&self, folder: &Path) -> Result<Vec<OsString>, LaunchError> {
        if !folder.is_absolute() {
            return Err(LaunchError::RelativeFolder);
        }
        if !self.policy_installed && self.launcher.as_os_str().as_encoded_bytes().contains(&b'=') {
            return Err(LaunchError::LauncherHasEquals);
        }
        let mut argv = vec![self.pkexec.clone().into_os_string()];
        if !self.policy_installed {
            argv.push(self.env.clone().into_os_string());
            for (name, value) in [("DISPLAY", &self.display), ("XAUTHORITY", &self.xauthority)] {
                if let Some(value) = value.as_ref().filter(|value| !value.is_empty()) {
                    let mut pair = OsString::from(format!("{name}="));
                    pair.push(value);
                    argv.push(pair);
                }
            }
        }
        argv.push(self.launcher.clone().into_os_string());
        argv.push(OsString::from(NEW_INSTANCE_FLAG));
        argv.push(OsString::from("--"));
        argv.push(folder.as_os_str().to_owned());
        Ok(argv)
    }
}

/// Why a location is not opened as administrator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The Trash, Recent, search results or disk usage: no real folder.
    NotAFolder,
    /// A phone, network share or FUSE mount: root cannot reach the user's.
    Remote,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotAFolder => "Open as Administrator needs a folder. Open one first",
            Self::Remote => {
                "Open as Administrator works on local folders, not on phones or network mounts"
            }
        })
    }
}

/// Why `location` is not opened as administrator, or `None` for a local
/// folder; `remote` says whether a path is on FUSE or gvfs.
pub fn refusal(location: &Location, remote: impl Fn(&Path) -> bool) -> Option<Refusal> {
    match location {
        Location::Directory(path) => remote(path).then_some(Refusal::Remote),
        _ => Some(Refusal::NotAFolder),
    }
}

/// Whether the menu offers the row: never to root, only in a directory, on
/// exactly one selected folder or the empty space. (`selected` holds each
/// selected path and whether it is a folder.)
#[must_use]
pub fn row_offered(root: bool, location: &Location, selected: &[(PathBuf, bool)]) -> bool {
    !root && matches!(location, Location::Directory(_)) && matches!(selected, [] | [(_, true)])
}

/// The notice for how the launch ended; `None` when nothing is worth saying
/// (a clean end, or a signal). Exit codes 126 and 127 are pkexec's (dialog
/// dismissed, not authorized) but also `env`'s and Fileflux's own, so the
/// words stay neutral.
#[must_use]
pub fn exit_notice(code: Option<i32>) -> Option<String> {
    match code? {
        0 => None,
        code => Some(format!(
            "The administrator instance could not start (exit {code}): authentication was cancelled or refused, or the program could not run"
        )),
    }
}

/// Whether `path` (already canonical) is on a remote or FUSE mount, from the
/// mount table's bytes: the deepest mount holding it decides. Types `fuse`
/// and `fuse.*` (sshfs, gvfs, rclone…) are remote; `fuseblk`, which is how
/// ntfs-3g and exfat mount a local disk, is not.
#[must_use]
pub fn remote_in(table: &[u8], path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    let mut best: Option<(usize, bool)> = None;
    for line in table.split(|&byte| byte == b'\n') {
        let mut fields = line
            .split(|&byte| byte == b' ' || byte == b'\t')
            .filter(|field| !field.is_empty());
        let (_, Some(mount), Some(fs_type)) = (fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        let mount = PathBuf::from(OsStr::from_bytes(&unescape_mount(mount)));
        if !path.starts_with(&mount) {
            continue;
        }
        let depth = mount.components().count();
        let remote = fs_type == b"fuse" || fs_type.starts_with(b"fuse.");
        if best.is_none_or(|(deepest, _)| depth >= deepest) {
            best = Some((depth, remote));
        }
    }
    best.is_some_and(|(_, remote)| remote)
}

/// `\040`-style octal escapes of a mount table field.
fn unescape_mount(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut rest = field;
    while let Some((&byte, tail)) = rest.split_first() {
        let octal = (byte == b'\\')
            .then(|| tail.get(..3))
            .flatten()
            .filter(|digits| digits.iter().all(|d| (b'0'..=b'7').contains(d)))
            .map(|digits| digits.iter().fold(0u32, |n, d| n * 8 + u32::from(d - b'0')));
        match octal {
            Some(value) => {
                out.push(u8::try_from(value & 0xff).unwrap_or(b'?'));
                rest = tail.get(3..).unwrap_or_default();
            }
            None => {
                out.push(byte);
                rest = tail;
            }
        }
    }
    out
}

/// Starts `argv` (program first) detached, with no stdin, stdout or stderr,
/// in its own process group. The caller waits for it.
pub fn spawn(argv: &[OsString]) -> io::Result<Child> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| io::Error::other("empty command line"))?;
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
}
