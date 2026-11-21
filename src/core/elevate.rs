//! Changing an owner needs root, so Fileflux asks polkit for it: it runs
//! `pkexec chown --no-dereference --from=+<shown uid> +<uid> -- <path>` and polkit shows its own
//! authentication dialog. Fileflux never asks for, stores or logs a password;
//! the child has no stdin and only its exit code and `chown`'s own error text
//! come back.
//!
//! Everything here is pure or takes the side effect as a parameter, so the
//! command line, the program lookup, the exit code mapping and the identity
//! re-check are tested without ever running pkexec. The command is an argv
//! vector (never a shell string) and both programs are found in fixed
//! absolute locations, never through `PATH`.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

/// Where `pkexec` may be: NixOS's setuid wrapper first (the store copy is
/// not setuid), then the usual FHS places.
pub const PKEXEC_CANDIDATES: &[&str] =
    &["/run/wrappers/bin/pkexec", "/usr/bin/pkexec", "/bin/pkexec"];

/// Where `chown` may be.
pub const CHOWN_CANDIDATES: &[&str] = &[
    "/run/current-system/sw/bin/chown",
    "/usr/bin/chown",
    "/bin/chown",
];

/// The longest piece of `chown`'s error text a message keeps.
const MAX_REASON: usize = 300;

/// The first candidate that `exists` accepts.
pub fn resolve(candidates: &[&str], exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|candidate| exists(candidate))
}

/// Whether `path` is a regular file somebody can execute.
#[must_use]
pub fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Which program is missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    Pkexec,
    Chown,
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pkexec => f.write_str(
                "Changing the owner needs root: install polkit (the pkexec program) to enable it",
            ),
            Self::Chown => {
                f.write_str("Changing the owner needs the chown program, which was not found")
            }
        }
    }
}

/// Why a command line could not be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelativePath;

impl fmt::Display for RelativePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the path is not absolute")
    }
}

impl std::error::Error for RelativePath {}

/// The two programs an owner change runs, by absolute path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Elevation {
    pub pkexec: PathBuf,
    pub chown: PathBuf,
}

impl Elevation {
    /// Looks for both programs in their fixed locations.
    pub fn detect() -> Result<Self, Unavailable> {
        Self::detect_with(is_executable_file)
    }

    /// [`Self::detect`] with the file test given.
    pub fn detect_with(exists: impl Fn(&Path) -> bool + Copy) -> Result<Self, Unavailable> {
        let pkexec = resolve(PKEXEC_CANDIDATES, exists).ok_or(Unavailable::Pkexec)?;
        let chown = resolve(CHOWN_CANDIDATES, exists).ok_or(Unavailable::Chown)?;
        Ok(Self { pkexec, chown })
    }

    /// The full command line, program first: `pkexec chown --no-dereference
    /// --from=+<from> +<uid> -- <path>`. `from` is the owner the panel
    /// showed: chown acts only if the file still has it. The path is one
    /// argument whatever it holds, and `--` keeps a leading `-` from being
    /// an option. A relative path is refused: pkexec does not run in our
    /// folder.
    pub fn argv(&self, from: u32, uid: u32, path: &Path) -> Result<Vec<OsString>, RelativePath> {
        if !path.is_absolute() {
            return Err(RelativePath);
        }
        Ok(vec![
            self.pkexec.clone().into_os_string(),
            self.chown.clone().into_os_string(),
            OsString::from("--no-dereference"),
            OsString::from(format!("--from=+{from}")),
            // `+` makes chown read the owner as a uid even when a user is
            // literally named like that number.
            OsString::from(format!("+{uid}")),
            OsString::from("--"),
            path.as_os_str().to_owned(),
        ])
    }
}

/// Which file a path was when the user looked at it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub dev: u64,
    pub ino: u64,
    /// The birth time, where the filesystem records it: ext4 reuses the
    /// inode number of a deleted file, so device and inode alone cannot
    /// tell a replaced file from the same one.
    pub born: Option<SystemTime>,
    /// The owner uid the panel showed. Not part of [`Self::check`] (the
    /// owner is what changes); it becomes chown's `--from`.
    pub owner: u32,
    pub symlink: bool,
}

impl Identity {
    /// Reads the identity of `path` without following a symbolic link.
    pub fn of(path: &Path) -> io::Result<Self> {
        let meta = std::fs::symlink_metadata(path)?;
        Ok(Self {
            dev: meta.dev(),
            ino: meta.ino(),
            born: meta.created().ok(),
            owner: meta.uid(),
            symlink: meta.file_type().is_symlink(),
        })
    }

    /// Whether `now` is still the file this identity saw.
    pub fn check(&self, now: &Self) -> Result<(), &'static str> {
        if now.symlink {
            return Err("it is a symbolic link now, left alone");
        }
        if (now.dev, now.ino, now.born) != (self.dev, self.ino, self.born) {
            return Err("it changed since it was shown, left alone");
        }
        Ok(())
    }
}

/// How the elevated command ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finished {
    /// The exit code; `None` when a signal ended it.
    pub code: Option<i32>,
    /// What it wrote on stderr (stdout is discarded).
    pub stderr: String,
}

/// What an owner change came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Changed,
    /// The user did not authorize it (pkexec 126/127); nothing changed.
    Cancelled(String),
    /// Nothing changed; the reason.
    Failed(String),
}

/// Maps the exit of `pkexec chown`: 0 changed; 126 the dialog was dismissed;
/// 127 authorization could not be obtained (no agent, or not authorized);
/// anything else is `chown`'s own failure.
#[must_use]
pub fn outcome_of(code: Option<i32>, stderr: &str) -> Outcome {
    match code {
        Some(0) => Outcome::Changed,
        Some(126) => Outcome::Cancelled(
            "Authentication was cancelled; the owner was not changed".to_owned(),
        ),
        Some(127) => Outcome::Cancelled(
            "Authorization was not granted (no polkit agent, or not allowed); the owner was not changed"
                .to_owned(),
        ),
        Some(code) => Outcome::Failed(with_detail(format!("chown failed (exit {code})"), stderr)),
        None => Outcome::Failed(with_detail(
            "chown was ended by a signal".to_owned(),
            stderr,
        )),
    }
}

fn with_detail(head: String, stderr: &str) -> String {
    let detail: String = stderr.trim().chars().take(MAX_REASON).collect();
    if detail.is_empty() {
        head
    } else {
        format!("{head}: {detail}")
    }
}

/// Re-checks that `path` still is the file `expected` saw, then runs the
/// command line through `run` (blocking). A swapped, replaced, missing or
/// now-symbolic-link path is refused and nothing is run; chown itself only
/// acts if the file still has the owner shown (`--from`). After a success
/// the path is read once more: if it is no longer that file the outcome is a
/// failure that names the path, because the change ran on something else.
///
/// Known debt: a folder *above* the file can still be swapped for a symbolic
/// link during the polkit wait (`chown --no-dereference` only protects the
/// last component). pkexec closes inherited file descriptors and clears the
/// environment, so pinning the file with an fd is impossible through pkexec.
/// A full fix needs a small root helper with its own polkit action that
/// opens each component with `O_NOFOLLOW` / `openat2` `RESOLVE_NO_SYMLINKS`
/// and uses `fchown`. `--from` and the post-change check limit and detect
/// the swap.
pub fn change_owner(
    elevation: &Elevation,
    uid: u32,
    path: &Path,
    expected: &Identity,
    run: impl FnOnce(&[OsString]) -> io::Result<Finished>,
) -> Outcome {
    let now = match Identity::of(path) {
        Ok(now) => now,
        Err(error) => return Outcome::Failed(format!("Could not read the entry: {error}")),
    };
    if let Err(reason) = expected.check(&now) {
        return Outcome::Failed(reason.to_owned());
    }
    let argv = match elevation.argv(expected.owner, uid, path) {
        Ok(argv) => argv,
        Err(error) => return Outcome::Failed(error.to_string()),
    };
    match run(&argv) {
        Ok(finished) => match outcome_of(finished.code, &finished.stderr) {
            Outcome::Changed => verify_after(expected, path),
            other => other,
        },
        Err(error) => Outcome::Failed(format!("Could not start pkexec: {error}")),
    }
}

/// After a successful chown: is `path` still the chosen file?
fn verify_after(expected: &Identity, path: &Path) -> Outcome {
    if Identity::of(path).is_ok_and(|now| expected.check(&now).is_ok()) {
        return Outcome::Changed;
    }
    Outcome::Failed(format!(
        "The owner change ran, but {} no longer points at the file that was chosen \
         (it was replaced while waiting for authorization); check its owner",
        path.display()
    ))
}

/// Runs `argv` (program first) with no stdin and no stdout, waiting for it.
/// This is where polkit's dialog appears; call it off the UI thread.
pub fn spawn_and_wait(argv: &[OsString]) -> io::Result<Finished> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| io::Error::other("empty command line"))?;
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    Ok(Finished {
        code: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}
