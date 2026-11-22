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

/// Whether the policy text names `launcher` as the executable and allows
/// the GUI environment through.
#[must_use]
pub fn policy_allows(policy: &str, launcher: &Path) -> bool {
    let annotation = |key: &str| {
        let (_, rest) = policy.split_once(key)?;
        let (text, _) = rest.split_once("</annotate>")?;
        Some(text.trim())
    };
    annotation(EXEC_PATH_KEY).is_some_and(|path| Path::new(path) == launcher)
        && annotation(ALLOW_GUI_KEY) == Some("true")
}

/// Where the policy may be installed: beside the launcher's prefix, then the
/// system directories.
#[must_use]
pub fn policy_candidates(launcher: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Some(bin) = launcher.parent() {
        found.push(bin.join("../share/polkit-1/actions").join(POLICY_FILE));
    }
    for dir in [
        "/usr/share/polkit-1/actions",
        "/run/current-system/sw/share/polkit-1/actions",
    ] {
        found.push(Path::new(dir).join(POLICY_FILE));
    }
    found
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

/// The notice for how `pkexec` ended; `None` when nothing is worth saying
/// (a clean end, or a signal).
#[must_use]
pub fn exit_notice(code: Option<i32>) -> Option<String> {
    match code? {
        0 => None,
        126 => Some(
            "Authentication was cancelled; Fileflux was not opened as administrator".to_owned(),
        ),
        127 => Some(
            "Authorization was not granted (no polkit agent, or not allowed); Fileflux was not opened as administrator"
                .to_owned(),
        ),
        code => Some(format!("The administrator instance ended (exit {code})")),
    }
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
