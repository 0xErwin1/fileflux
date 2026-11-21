//! The root owner helper (`fileflux-owner-helper`), shared by the helper
//! binary and Fileflux, which builds the helper's command line and reads its
//! exit codes.
//!
//! This file uses only `std` and `rustix`, because the helper binary
//! includes it by path and must not link the GUI. Fileflux asks polkit
//! (`pkexec`) to run the helper as root with the snapshot of the file the
//! panel showed (device, inode, birth time, owner) and the new uid. The
//! helper opens the path from `/` without following any symbolic link at any
//! level, compares the snapshot with the open descriptor, and only if
//! everything matches changes the owner on that same descriptor
//! (`fchownat(fd, "", uid, -1, AT_EMPTY_PATH)`); otherwise it changes nothing
//! and exits with a distinct code. It reads no stdin, never recurses and runs
//! no other program.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fd::OwnedFd;
use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, ResolveFlags, StatxFlags, chownat, makedev, open, openat,
    openat2, statx,
};
use rustix::io::Errno;
use rustix::process::{Uid, geteuid};

/// The helper's file name, installed next to the Fileflux executable.
pub const HELPER_NAME: &str = "fileflux-owner-helper";

/// The longest path the helper accepts (Linux's `PATH_MAX` less the NUL).
const MAX_PATH: usize = 4095;

/// `(uid_t)-1`: "leave the owner alone" to `chown`, never a valid target.
const NO_UID: u32 = u32::MAX;

/// How the helper ended. The codes avoid 0 (success) and pkexec's own 126 and
/// 127.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// Wrong number or form of arguments.
    Usage = 64,
    /// The path is not absolute or has an empty, `.` or `..` component.
    InvalidPath = 65,
    /// A symbolic link (or magic link) in the path, or something that is not
    /// a regular file or folder: refused.
    Refused = 66,
    /// The path does not exist.
    NotFound = 67,
    /// The device, inode or birth time differ from the snapshot: the file
    /// was replaced.
    IdentityMismatch = 68,
    /// The owner differs from the one shown.
    OwnerMismatch = 69,
    /// The kernel refused the change.
    ChownFailed = 70,
    /// Not running as root, so it can do nothing useful.
    NotRoot = 71,
}

impl Exit {
    #[must_use]
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// The exit a process status code stands for, if it is one of ours.
    #[must_use]
    pub const fn from_code(code: i32) -> Option<Self> {
        Some(match code {
            64 => Self::Usage,
            65 => Self::InvalidPath,
            66 => Self::Refused,
            67 => Self::NotFound,
            68 => Self::IdentityMismatch,
            69 => Self::OwnerMismatch,
            70 => Self::ChownFailed,
            71 => Self::NotRoot,
            _ => return None,
        })
    }
}

/// An exit and the one-line reason for stderr.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub exit: Exit,
    pub reason: String,
}

impl Failure {
    /// Control characters are replaced so the reason stays one line and
    /// cannot drive a terminal.
    pub fn new(exit: Exit, reason: impl AsRef<str>) -> Self {
        let reason = reason
            .as_ref()
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        Self { exit, reason }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for Failure {}

/// A birth time, as `statx` gives it (seconds may be negative).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Born {
    pub secs: i64,
    pub nanos: u32,
}

impl Born {
    /// The birth time `time` stands for.
    #[must_use]
    pub fn from_system_time(time: SystemTime) -> Self {
        match time.duration_since(UNIX_EPOCH) {
            Ok(after) => Self {
                secs: i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
                nanos: after.subsec_nanos(),
            },
            Err(error) => {
                let before = error.duration();
                let secs = i64::try_from(before.as_secs()).unwrap_or(i64::MAX);
                match before.subsec_nanos() {
                    0 => Self {
                        secs: -secs,
                        nanos: 0,
                    },
                    nanos => Self {
                        secs: -secs - 1,
                        nanos: 1_000_000_000 - nanos,
                    },
                }
            }
        }
    }
}

/// What the panel saw of the file: the identity plus the owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub dev: u64,
    pub ino: u64,
    pub born: Option<Born>,
    pub owner: u32,
}

/// What kind of entry an open descriptor is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

/// What `statx` found on the open descriptor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub kind: Kind,
    pub snapshot: Snapshot,
}

impl Snapshot {
    /// Whether `found` is still the file this snapshot saw, with its owner.
    /// A symbolic link or special file is refused whatever it matches;
    /// the identity is compared before the owner.
    pub fn verify(&self, found: &Found) -> Result<(), Failure> {
        if !matches!(found.kind, Kind::File | Kind::Dir) {
            return Err(Failure::new(
                Exit::Refused,
                "not a regular file or folder (a symbolic link or special file)",
            ));
        }
        let now = &found.snapshot;
        if (now.dev, now.ino, now.born) != (self.dev, self.ino, self.born) {
            return Err(Failure::new(
                Exit::IdentityMismatch,
                "the file is not the one that was shown (replaced since)",
            ));
        }
        if now.owner != self.owner {
            return Err(Failure::new(
                Exit::OwnerMismatch,
                format!(
                    "the owner is {} now, not {} as shown",
                    now.owner, self.owner
                ),
            ));
        }
        Ok(())
    }
}

/// One owner change: the path, what it was, and the new owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub path: OsString,
    pub snapshot: Snapshot,
    pub new_uid: u32,
}

fn usage(reason: impl AsRef<str>) -> Failure {
    Failure::new(Exit::Usage, reason)
}

/// An unsigned decimal, digits only (no sign, no spaces).
fn decimal<T: std::str::FromStr>(arg: &OsStr, what: &str) -> Result<T, Failure> {
    let text = arg
        .to_str()
        .filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()));
    text.and_then(|t| t.parse().ok())
        .ok_or_else(|| usage(format!("{what} must be a plain decimal number")))
}

fn uid_arg(arg: &OsStr, what: &str) -> Result<u32, Failure> {
    let uid: u32 = decimal(arg, what)?;
    if uid == NO_UID {
        return Err(usage(format!("{what} {NO_UID} is not a valid uid")));
    }
    Ok(uid)
}

fn parse_born(arg: &OsStr) -> Result<Option<Born>, Failure> {
    let text = arg
        .to_str()
        .ok_or_else(|| usage("birth time is not text"))?;
    if text == "none" {
        return Ok(None);
    }
    let bad = || usage("birth time must be none or SECONDS:NANOSECONDS");
    let (secs, nanos) = text.split_once(':').ok_or_else(bad)?;
    let digits = secs.strip_prefix('-').unwrap_or(secs);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let secs: i64 = secs.parse().map_err(|_| bad())?;
    let nanos: u32 = decimal(OsStr::new(nanos), "birth nanoseconds").map_err(|_| bad())?;
    if nanos >= 1_000_000_000 {
        return Err(bad());
    }
    Ok(Some(Born { secs, nanos }))
}

impl Request {
    /// Parses the arguments after the program name:
    /// `<absolute path> <dev> <ino> <birth|none> <owner uid> <new uid>`.
    /// The birth time is `SECONDS:NANOSECONDS`. Nothing is read from the
    /// system here.
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, Failure> {
        let args: Vec<OsString> = args.into_iter().collect();
        let [path, dev, ino, born, owner, new_uid] = args.as_slice() else {
            return Err(usage(
                "usage: fileflux-owner-helper PATH DEV INO BIRTH|none OWNER_UID NEW_UID",
            ));
        };
        split_path(path)?;
        Ok(Self {
            path: path.clone(),
            snapshot: Snapshot {
                dev: decimal(dev, "dev")?,
                ino: decimal(ino, "ino")?,
                born: parse_born(born)?,
                owner: uid_arg(owner, "owner uid")?,
            },
            new_uid: uid_arg(new_uid, "new uid")?,
        })
    }

    /// The arguments [`Self::parse`] reads back to this request.
    #[must_use]
    pub fn to_args(&self) -> Vec<OsString> {
        let born = self.snapshot.born.map_or_else(
            || "none".to_owned(),
            |born| format!("{}:{}", born.secs, born.nanos),
        );
        vec![
            self.path.clone(),
            OsString::from(self.snapshot.dev.to_string()),
            OsString::from(self.snapshot.ino.to_string()),
            OsString::from(born),
            OsString::from(self.snapshot.owner.to_string()),
            OsString::from(self.new_uid.to_string()),
        ]
    }
}

/// The components of an absolute path. A relative path, `/` alone, and an
/// empty, `.` or `..` component (so also `//` and a trailing `/`) are
/// refused: the path is opened exactly as written.
pub fn split_path(path: &OsStr) -> Result<Vec<&OsStr>, Failure> {
    let bad = |why: &str| Failure::new(Exit::InvalidPath, why);
    let bytes = path.as_bytes();
    if bytes.len() > MAX_PATH {
        return Err(bad("the path is too long"));
    }
    let Some(rest) = bytes.strip_prefix(b"/") else {
        return Err(bad("the path is not absolute"));
    };
    if rest.is_empty() {
        return Err(bad("the path has no component"));
    }
    rest.split(|&b| b == b'/')
        .map(|part| match part {
            b"" | b"." | b".." => Err(bad("the path has an empty, . or .. component")),
            part => Ok(OsStr::from_bytes(part)),
        })
        .collect()
}

/// Why one component could not be opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Symlink,
    Missing,
    Failed(String),
}

/// A directory tree opened one component at a time (injectable, so the walk
/// is tested without a kernel).
pub trait Tree {
    type Node;

    /// The root, `/`.
    fn root(&self) -> Result<Self::Node, String>;

    /// `name` inside `parent`, never following a symbolic link. `last` marks
    /// the final component, which is opened as itself, whatever it is; the
    /// others must be folders.
    fn open(&self, parent: &Self::Node, name: &OsStr, last: bool) -> Result<Self::Node, Step>;
}

/// Opens `parts` from the root one component at a time.
pub fn walk<T: Tree>(tree: &T, parts: &[&OsStr]) -> Result<T::Node, Failure> {
    let mut node = tree
        .root()
        .map_err(|why| Failure::new(Exit::Refused, format!("could not open /: {why}")))?;
    let count = parts.len();
    for (index, part) in parts.iter().enumerate() {
        node = tree
            .open(&node, part, index + 1 == count)
            .map_err(|step| match step {
                Step::Symlink => Failure::new(Exit::Refused, "a symbolic link is in the path"),
                Step::Missing => Failure::new(Exit::NotFound, "the path does not exist"),
                Step::Failed(why) => {
                    Failure::new(Exit::Refused, format!("could not open a component: {why}"))
                }
            })?;
    }
    Ok(node)
}

/// The real kernel, one `openat` per component.
#[derive(Clone, Copy, Debug)]
pub struct Kernel;

impl Tree for Kernel {
    type Node = OwnedFd;

    fn root(&self) -> Result<OwnedFd, String> {
        open(
            "/",
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| e.to_string())
    }

    fn open(&self, parent: &OwnedFd, name: &OsStr, last: bool) -> Result<OwnedFd, Step> {
        let base = OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let flags = if last { base } else { base | OFlags::DIRECTORY };
        match openat(parent, name, flags, Mode::empty()) {
            Ok(fd) => Ok(fd),
            Err(Errno::LOOP) => Err(Step::Symlink),
            Err(Errno::NOENT) => Err(Step::Missing),
            // O_DIRECTORY | O_NOFOLLOW on a link gives ENOTDIR as it does on
            // a file: look at what it is to tell them apart.
            Err(Errno::NOTDIR) => match openat(parent, name, base, Mode::empty()) {
                Ok(fd) => match stat(&fd) {
                    Ok(found) if found.kind == Kind::Symlink => Err(Step::Symlink),
                    _ => Err(Step::Missing),
                },
                Err(_) => Err(Step::Missing),
            },
            Err(e) => Err(Step::Failed(e.to_string())),
        }
    }
}

/// Opens the file `parts` names without following a symbolic link at any
/// level: `openat2` with `RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS` from
/// `/`, or [`walk`] when the kernel has no `openat2`.
pub fn open_without_links(parts: &[&OsStr]) -> Result<OwnedFd, Failure> {
    let root = Kernel
        .root()
        .map_err(|why| Failure::new(Exit::Refused, format!("could not open /: {why}")))?;
    let relative: PathBuf = parts.iter().collect();
    match openat2(
        &root,
        &relative,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    ) {
        Ok(fd) => Ok(fd),
        // No openat2 (old kernel), or a seccomp filter that answers EPERM
        // for it (containers): the component walk refuses links itself.
        Err(Errno::NOSYS | Errno::PERM) => walk(&Kernel, parts),
        Err(Errno::LOOP | Errno::XDEV) => Err(Failure::new(
            Exit::Refused,
            "a symbolic link is in the path",
        )),
        Err(Errno::NOENT | Errno::NOTDIR) => {
            Err(Failure::new(Exit::NotFound, "the path does not exist"))
        }
        Err(e) => Err(Failure::new(
            Exit::Refused,
            format!("could not open the path: {e}"),
        )),
    }
}

/// What `fd` is, by `statx` on the descriptor itself.
pub fn stat(fd: &OwnedFd) -> Result<Found, Failure> {
    let stx = statx(
        fd,
        "",
        AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::BASIC_STATS | StatxFlags::BTIME,
    )
    .map_err(|e| Failure::new(Exit::Refused, format!("could not read the file: {e}")))?;
    let kind = match FileType::from_raw_mode(u32::from(stx.stx_mode)) {
        FileType::RegularFile => Kind::File,
        FileType::Directory => Kind::Dir,
        FileType::Symlink => Kind::Symlink,
        _ => Kind::Other,
    };
    let born = (stx.stx_mask & StatxFlags::BTIME.bits() != 0).then_some(Born {
        secs: stx.stx_btime.tv_sec,
        nanos: stx.stx_btime.tv_nsec,
    });
    Ok(Found {
        kind,
        snapshot: Snapshot {
            dev: makedev(stx.stx_dev_major, stx.stx_dev_minor),
            ino: stx.stx_ino,
            born,
            owner: stx.stx_uid,
        },
    })
}

/// Whether this process runs with effective uid 0.
#[must_use]
pub fn is_root() -> bool {
    geteuid().is_root()
}

/// The whole change: refuses unless `root`, opens the path without links,
/// compares the snapshot with the open descriptor and, only if it all
/// matches, changes the owner (not the group) on that same descriptor.
pub fn run(request: &Request, root: bool) -> Result<(), Failure> {
    if !root {
        return Err(Failure::new(
            Exit::NotRoot,
            "must run as root (it is only meant to be started by pkexec)",
        ));
    }
    let parts = split_path(&request.path)?;
    let fd = open_without_links(&parts)?;
    request.snapshot.verify(&stat(&fd)?)?;
    chownat(
        &fd,
        "",
        Some(Uid::from_raw(request.new_uid)),
        None,
        AtFlags::EMPTY_PATH,
    )
    .map_err(|e| {
        Failure::new(
            Exit::ChownFailed,
            format!("the owner could not be changed: {e}"),
        )
    })
}
