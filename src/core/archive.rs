//! Compressing and extracting archives with `bsdtar` (libarchive) as an
//! external process.
//!
//! Safety rules, in the order they bite:
//! - nothing is ever overwritten: the archive or folder gets a unique name
//!   and is renamed into place (`RENAME_NOREPLACE`) only once it is complete;
//! - an archive with an absolute member or one with a `..` component is
//!   refused whole, before anything is extracted;
//! - extraction happens in a fresh hidden folder next to its final place,
//!   with `--no-same-owner --no-same-permissions` and without `-P`, so
//!   bsdtar keeps its own refusals of `..` and absolute names;
//! - after extraction every symlink whose resolved target leaves that
//!   folder is removed;
//! - a cancelled or failed run removes what it made.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use super::ops::{NameError, rename_noreplace, unique_name_cancellable, validate_name};

/// The program every operation here runs.
pub const TOOL: &str = "bsdtar";

/// Most bytes of a member listing read before the archive is refused: a
/// listing that was cut short cannot vouch for the members after the cut.
const LISTING_CAP: usize = 64 << 20;
/// Most bytes of bsdtar's error output kept for the message.
const STDERR_CAP: usize = 16 << 10;
/// Name prefix of the hidden work files; listings hide `.fileflux-partial-*`
/// (see [`super::ops::is_transfer_temp`]).
const TEMP_PREFIX: &str = ".fileflux-partial-";
/// How often the child is checked and the cancel flag polled.
const POLL: Duration = Duration::from_millis(25);
/// How many times a name taken in the meantime is picked again.
const PLACE_ATTEMPTS: u32 = 32;

/// The archive formats "Compress…" offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Zip,
    TarGz,
    TarXz,
}

impl Format {
    pub const ALL: [Format; 3] = [Format::Zip, Format::TarGz, Format::TarXz];

    /// The file-name suffix bsdtar's `-a` picks the format by.
    #[must_use]
    pub fn suffix(self) -> &'static str {
        match self {
            Format::Zip => ".zip",
            Format::TarGz => ".tar.gz",
            Format::TarXz => ".tar.xz",
        }
    }
}

/// Why a member name is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberProblem {
    Absolute,
    ParentDir,
}

/// What can go wrong.
#[derive(Debug)]
pub enum ArchiveError {
    Cancelled,
    Unsafe {
        member: String,
        problem: MemberProblem,
    },
    ListingTooLarge,
    Name(NameError),
    /// bsdtar failed; its own words.
    Tool(String),
    Io(io::Error),
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArchiveError::Cancelled => f.write_str("Cancelled"),
            ArchiveError::Unsafe { member, .. } => write!(
                f,
                "The archive is unsafe: \u{201c}{member}\u{201d} points outside the destination. Nothing was extracted."
            ),
            ArchiveError::ListingTooLarge => f.write_str(
                "The archive lists too many members to check safely. Nothing was extracted.",
            ),
            ArchiveError::Name(error) => write!(f, "{error}"),
            ArchiveError::Tool(text) => f.write_str(text),
            ArchiveError::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ArchiveError {}

impl From<io::Error> for ArchiveError {
    fn from(error: io::Error) -> Self {
        ArchiveError::Io(error)
    }
}

impl From<NameError> for ArchiveError {
    fn from(error: NameError) -> Self {
        ArchiveError::Name(error)
    }
}

/// `bsdtar` on `$PATH`, looked up once per session.
pub fn tool() -> Option<&'static Path> {
    static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOUND
        .get_or_init(|| std::env::var_os("PATH").and_then(|path| find_in_path(TOOL, &path)))
        .as_deref()
}

/// The executable file `name` in the `path` search list.
pub fn find_in_path(name: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

/// The suffixes bsdtar reads as archives, longest first within a family.
const ARCHIVE_SUFFIXES: [&str; 14] = [
    ".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst", ".tgz", ".tbz2", ".tbz", ".txz", ".tzst", ".tar",
    ".zip", ".7z", ".rar", ".tar.lz",
];

/// Length of the archive suffix `name` ends in; `None` when it has none or
/// the suffix would be the whole name.
fn archive_suffix_len(name: &[u8]) -> Option<usize> {
    let lower = name.to_ascii_lowercase();
    ARCHIVE_SUFFIXES
        .iter()
        .find(|suffix| lower.len() > suffix.len() && lower.ends_with(suffix.as_bytes()))
        .map(|suffix| suffix.len())
}

/// Whether `name` ends in an archive suffix bsdtar reads.
pub fn is_archive_name(name: &OsStr) -> bool {
    archive_suffix_len(name.as_bytes()).is_some()
}

/// `name` without its archive suffix: "foo.tar.gz" → "foo". A name that has
/// none is returned as it is.
pub fn strip_archive_suffix(name: &OsStr) -> OsString {
    let bytes = name.as_bytes();
    match archive_suffix_len(bytes) {
        Some(len) => OsString::from_vec(bytes.get(..bytes.len() - len).unwrap_or(bytes).to_vec()),
        None => name.to_owned(),
    }
}

/// The folder name "Extract here" proposes for `archive`.
pub fn extraction_folder_name(archive: &Path) -> OsString {
    let name = archive.file_name().unwrap_or(archive.as_os_str());
    let stripped = strip_archive_suffix(name);
    if stripped == name {
        let mut text = name.to_owned();
        text.push(" extracted");
        text
    } else {
        stripped
    }
}

/// The name "Compress…" proposes: the one entry's name, else "Archive".
pub fn default_archive_base(names: &[OsString]) -> OsString {
    match names {
        [one] => one.clone(),
        _ => OsString::from("Archive"),
    }
}

/// The file name for `base` in `format`: the suffix is added unless `base`
/// already ends in it.
pub fn archive_file_name(base: &OsStr, format: Format) -> Result<OsString, NameError> {
    validate_name(base)?;
    let suffix = format.suffix();
    let lower = base.as_bytes().to_ascii_lowercase();
    let has = lower.len() > suffix.len() && lower.ends_with(suffix.as_bytes());
    let mut name = base.to_owned();
    if !has {
        name.push(suffix);
    }
    validate_name(&name)?;
    Ok(name)
}

/// Why `member` may not be extracted, if so: an absolute name (also in the
/// Windows spellings) or one with a `..` component (either separator).
pub fn member_problem(member: &[u8]) -> Option<MemberProblem> {
    let drive = matches!(
        member,
        [letter, b':', b'/' | b'\\', ..] if letter.is_ascii_alphabetic()
    );
    if matches!(member.first(), Some(b'/' | b'\\')) || drive {
        return Some(MemberProblem::Absolute);
    }
    member
        .split(|byte| matches!(byte, b'/' | b'\\'))
        .any(|component| component == b"..")
        .then_some(MemberProblem::ParentDir)
}

/// Checks a `bsdtar -tf` listing: the number of members when every one is
/// safe, else the first refusal. A member name with a line break splits
/// into several lines, each checked: only ever stricter.
pub fn check_listing(listing: &[u8]) -> Result<usize, ArchiveError> {
    let mut count = 0;
    for line in listing.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        if let Some(problem) = member_problem(line) {
            return Err(ArchiveError::Unsafe {
                member: String::from_utf8_lossy(line).into_owned(),
                problem,
            });
        }
        count += 1;
    }
    Ok(count)
}

/// Whether the symlink `link` (inside `root`) resolves to somewhere outside
/// it. The whole chain is followed; a target that does not exist is judged
/// by where its path leads.
pub fn symlink_escapes(root: &Path, link: &Path) -> io::Result<bool> {
    let root = fs::canonicalize(root)?;
    if let Ok(resolved) = fs::canonicalize(link) {
        return Ok(!resolved.starts_with(&root));
    }
    let target = fs::read_link(link)?;
    let base = match link.parent() {
        Some(parent) => fs::canonicalize(parent)?,
        None => root.clone(),
    };
    let mut resolved = PathBuf::new();
    let joined = if target.is_absolute() {
        target
    } else {
        base.join(target)
    };
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            other => resolved.push(other.as_os_str()),
        }
    }
    Ok(!resolved.starts_with(&root))
}

/// `bsdtar -a -cf <archive> -C <folder> -- <members>`: the format follows
/// the archive's suffix, members are names inside `folder`. A name that
/// starts with `@` would make bsdtar read an archive of that name, and one
/// that starts with `-` can be taken as an option (bsdtar reads `-C<dir>`
/// in the member list as a directory change even after `--`), so both get
/// a `./`.
pub fn compress_args(archive: &Path, folder: &Path, members: &[OsString]) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "-a".into(),
        "-cf".into(),
        archive.as_os_str().to_owned(),
        "-C".into(),
        folder.as_os_str().to_owned(),
        "--".into(),
    ];
    for member in members {
        if matches!(member.as_bytes().first(), Some(b'@' | b'-')) {
            let mut name = OsString::from("./");
            name.push(member);
            args.push(name);
        } else {
            args.push(member.clone());
        }
    }
    args
}

/// `bsdtar -tf <archive>`: one member name per line. `archive` is absolute.
pub fn list_args(archive: &Path) -> Vec<OsString> {
    vec!["-tf".into(), archive.as_os_str().to_owned()]
}

/// `bsdtar -xf <archive> -C <into> --no-same-owner --no-same-permissions`.
/// Never `-P`: bsdtar then refuses `..` and absolute names on its own.
pub fn extract_args(archive: &Path, into: &Path) -> Vec<OsString> {
    vec![
        "-xf".into(),
        archive.as_os_str().to_owned(),
        "-C".into(),
        into.as_os_str().to_owned(),
        "--no-same-owner".into(),
        "--no-same-permissions".into(),
    ]
}

// --- running bsdtar ---------------------------------------------------------------

/// Reads `stream` to its end, keeping at most `cap` bytes; says whether
/// more came.
fn read_capped(mut stream: impl Read, cap: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 16 << 10];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(kept.len());
                if n > room {
                    truncated = true;
                }
                kept.extend_from_slice(chunk.get(..n.min(room)).unwrap_or_default());
            }
        }
    }
    (kept, truncated)
}

struct Output {
    stdout: Vec<u8>,
    stdout_truncated: bool,
}

/// Runs `tool` with `args`, polling `cancel`: a cancel kills the child.
fn run(
    tool: &Path,
    args: &[OsString],
    stdout_cap: usize,
    cancel: &AtomicBool,
) -> Result<Output, ArchiveError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(ArchiveError::Cancelled);
    }
    let mut child = Command::new(tool)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let out = child.stdout.take();
    let err = child.stderr.take();
    let out_reader =
        thread::spawn(move || out.map_or_else(Default::default, |s| read_capped(s, stdout_cap)));
    let err_reader =
        thread::spawn(move || err.map_or_else(Default::default, |s| read_capped(s, STDERR_CAP)));
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            child.kill().ok();
            child.wait().ok();
            out_reader.join().ok();
            err_reader.join().ok();
            return Err(ArchiveError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(POLL),
            Err(error) => {
                child.kill().ok();
                child.wait().ok();
                return Err(error.into());
            }
        }
    };
    let (stdout, stdout_truncated) = out_reader.join().unwrap_or_default();
    let (stderr, _) = err_reader.join().unwrap_or_default();
    if !status.success() {
        let text = String::from_utf8_lossy(&stderr);
        let text = text.trim();
        return Err(ArchiveError::Tool(if text.is_empty() {
            format!("bsdtar failed ({status})")
        } else {
            let shown: String = text.chars().take(500).collect();
            format!("bsdtar: {}", shown.trim_start_matches("bsdtar: "))
        }));
    }
    Ok(Output {
        stdout,
        stdout_truncated,
    })
}

// --- work files -------------------------------------------------------------------

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_name(kind: &str, suffix: &str) -> OsString {
    OsString::from(format!(
        "{TEMP_PREFIX}{kind}-{}-{}{suffix}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

/// A hidden file or folder under construction, removed unless
/// [`Self::keep`] is called: success, failure, cancel and panic all clean up.
struct Work {
    path: PathBuf,
    armed: bool,
}

impl Work {
    /// Reserves a new empty file in `dir`.
    fn file(dir: &Path, suffix: &str) -> io::Result<Work> {
        for _ in 0..PLACE_ATTEMPTS {
            let path = dir.join(temp_name("archive", suffix));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Work { path, armed: true }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::ErrorKind::AlreadyExists.into())
    }

    /// Creates a new empty folder in `dir`.
    fn folder(dir: &Path) -> io::Result<Work> {
        for _ in 0..PLACE_ATTEMPTS {
            let path = dir.join(temp_name("extract", ""));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Work { path, armed: true }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::ErrorKind::AlreadyExists.into())
    }

    /// Renames the work into `dir` under the first free form of `name`,
    /// never replacing anything, and returns where it went.
    fn place(
        mut self,
        dir: &Path,
        name: &OsStr,
        cancel: &AtomicBool,
    ) -> Result<PathBuf, ArchiveError> {
        for _ in 0..PLACE_ATTEMPTS {
            if cancel.load(Ordering::Relaxed) {
                return Err(ArchiveError::Cancelled);
            }
            let free = unique_name_cancellable(dir, name, cancel)?;
            let target = dir.join(free);
            match rename_noreplace(&self.path, &target) {
                Ok(()) => {
                    self.armed = false;
                    return Ok(target);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(io::Error::from(io::ErrorKind::AlreadyExists).into())
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        if self.armed {
            remove_tree(&self.path).ok();
        }
    }
}

/// Removes a file or a folder tree without following symlinks, first
/// giving back the owner access an extracted folder may lack.
fn remove_tree(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return fs::remove_file(path);
    }
    ensure_owner_access(path, &meta)?;
    for entry in fs::read_dir(path)? {
        remove_tree(&entry?.path())?;
    }
    fs::remove_dir(path)
}

fn ensure_owner_access(path: &Path, meta: &fs::Metadata) -> io::Result<()> {
    let mode = meta.permissions().mode();
    if mode & 0o700 != 0o700 {
        fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700))?;
    }
    Ok(())
}

// --- compress ---------------------------------------------------------------------

/// Writes a new archive of the entries `members` of `folder` into `folder`.
/// It is built under a hidden temporary name and renamed into place only
/// when bsdtar succeeded; the final name is `base` plus the format's suffix,
/// made unique ("Archive (2).zip"): nothing is overwritten. Returns the new
/// archive's path.
pub fn compress(
    tool: &Path,
    folder: &Path,
    members: &[OsString],
    base: &OsStr,
    format: Format,
    cancel: &AtomicBool,
) -> Result<PathBuf, ArchiveError> {
    if members.is_empty() {
        return Err(ArchiveError::Tool(
            "There is nothing to compress.".to_owned(),
        ));
    }
    for member in members {
        validate_name(member)?;
    }
    let name = archive_file_name(base, format)?;
    let work = Work::file(folder, format.suffix())?;
    run(tool, &compress_args(&work.path, folder, members), 0, cancel)?;
    work.place(folder, &name, cancel)
}

// --- extract ----------------------------------------------------------------------

/// What an extraction produced.
#[derive(Debug)]
pub struct Extracted {
    /// The new folder.
    pub path: PathBuf,
    pub members: usize,
    /// Symlinks (paths inside the new folder) removed because their target
    /// leaves it.
    pub removed_links: Vec<PathBuf>,
}

/// Extracts `archive` into a new folder inside `dest_dir`, named after the
/// archive and made unique. The members are listed and checked first (an
/// absolute or `..` member refuses the whole archive); the extraction runs
/// in a hidden folder that is renamed into place only at the end; symlinks
/// that point outside it are removed first. Nothing existing is touched.
pub fn extract(
    tool: &Path,
    archive: &Path,
    dest_dir: &Path,
    cancel: &AtomicBool,
) -> Result<Extracted, ArchiveError> {
    let archive = std::path::absolute(archive)?;
    if !fs::metadata(dest_dir)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "the destination is not a folder",
        )
        .into());
    }
    let listing = run(tool, &list_args(&archive), LISTING_CAP, cancel)?;
    if listing.stdout_truncated {
        return Err(ArchiveError::ListingTooLarge);
    }
    let members = check_listing(&listing.stdout)?;

    let work = Work::folder(dest_dir)?;
    run(tool, &extract_args(&archive, &work.path), 0, cancel)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(ArchiveError::Cancelled);
    }
    let removed_links = remove_escaping_links(&work.path)?;
    let name = extraction_folder_name(&archive);
    let path = work.place(dest_dir, &name, cancel)?;
    Ok(Extracted {
        path,
        members,
        removed_links,
    })
}

/// Walks the extracted tree (never into a symlink), then removes every
/// symlink that escapes it. All verdicts are taken before any removal, so a
/// chain through a removed link still counts as escaping. Folders without
/// owner access get it back, so the tree can be read and removed. Returns
/// the removed links relative to `root`, sorted.
fn remove_escaping_links(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut links = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        ensure_owner_access(&dir, &fs::symlink_metadata(&dir)?)?;
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            let file_type = fs::symlink_metadata(&path)?.file_type();
            if file_type.is_symlink() {
                links.push(path);
            } else if file_type.is_dir() {
                pending.push(path);
            }
        }
    }
    let mut escaping = Vec::new();
    for link in links {
        if symlink_escapes(root, &link)? {
            escaping.push(link);
        }
    }
    let mut removed = Vec::new();
    for link in escaping {
        fs::remove_file(&link)?;
        removed.push(
            link.strip_prefix(root)
                .map_or(link.clone(), Path::to_path_buf),
        );
    }
    removed.sort();
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::{fs, process::Command};

    type TestResult = Result<(), Box<dyn std::error::Error>>;
    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> io::Result<Scratch> {
            let dir = std::env::temp_dir().join(format!(
                "fileflux-archive-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir)?;
            Ok(Scratch(dir))
        }
        fn dir(&self, name: &str) -> io::Result<PathBuf> {
            let path = self.0.join(name);
            fs::create_dir_all(&path)?;
            Ok(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            // Extracted trees may hold unreadable folders.
            Command::new("chmod")
                .arg("-R")
                .arg("u+rwx")
                .arg(&self.0)
                .status()
                .ok();
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn os(text: &str) -> OsString {
        OsString::from(text)
    }

    // --- suffixes -------------------------------------------------------

    #[test]
    fn archive_names_are_recognised_by_suffix() {
        for name in [
            "a.zip",
            "a.ZIP",
            "a.tar",
            "a.tar.gz",
            "a.tgz",
            "a.tar.bz2",
            "a.tbz2",
            "a.tar.xz",
            "a.txz",
            "a.tar.zst",
            "a.tzst",
            "a.7z",
            "a.rar",
            "my.project.tar.gz",
        ] {
            assert!(is_archive_name(OsStr::new(name)), "{name}");
        }
        for name in [
            "a.txt",
            "zip",
            "a.gz",
            "a.zip.txt",
            "tar.gz",
            ".zip",
            "a.",
            "a.xz",
        ] {
            assert!(!is_archive_name(OsStr::new(name)), "{name}");
        }
    }

    #[test]
    fn stripping_removes_the_archive_suffixes() {
        for (name, base) in [
            ("foo.tar.gz", "foo"),
            ("archive.zip", "archive"),
            ("pkg.tgz", "pkg"),
            ("a.tar.xz", "a"),
            ("a.TAR.GZ", "a"),
            ("my.project.zip", "my.project"),
            ("x.7z", "x"),
            ("plain.txt", "plain.txt"),
            (".zip", ".zip"),
            ("a.tar.zst", "a"),
        ] {
            assert_eq!(strip_archive_suffix(OsStr::new(name)), os(base), "{name}");
        }
    }

    #[test]
    fn extraction_folder_is_named_after_the_archive() {
        assert_eq!(
            extraction_folder_name(Path::new("/d/foo.tar.gz")),
            os("foo")
        );
        assert_eq!(
            extraction_folder_name(Path::new("/d/.zip")),
            os(".zip extracted")
        );
    }

    #[test]
    fn default_base_is_the_single_name_or_archive() {
        assert_eq!(default_archive_base(&[os("photos")]), os("photos"));
        assert_eq!(default_archive_base(&[os("a"), os("b")]), os("Archive"));
        assert_eq!(default_archive_base(&[]), os("Archive"));
    }

    #[test]
    fn file_name_gets_the_format_suffix_once() -> TestResult {
        assert_eq!(
            archive_file_name(OsStr::new("x"), Format::Zip)?,
            os("x.zip")
        );
        assert_eq!(
            archive_file_name(OsStr::new("x"), Format::TarGz)?,
            os("x.tar.gz")
        );
        assert_eq!(
            archive_file_name(OsStr::new("x"), Format::TarXz)?,
            os("x.tar.xz")
        );
        assert_eq!(
            archive_file_name(OsStr::new("x.zip"), Format::Zip)?,
            os("x.zip")
        );
        assert_eq!(
            archive_file_name(OsStr::new("x.tar.gz"), Format::TarGz)?,
            os("x.tar.gz")
        );
        assert_eq!(
            archive_file_name(OsStr::new("x.zip"), Format::TarGz)?,
            os("x.zip.tar.gz")
        );
        assert!(archive_file_name(OsStr::new(""), Format::Zip).is_err());
        assert!(archive_file_name(OsStr::new("a/b"), Format::Zip).is_err());
        assert!(archive_file_name(OsStr::new(".."), Format::Zip).is_err());
        Ok(())
    }

    #[test]
    fn unique_names_never_take_an_existing_one() -> TestResult {
        let scratch = Scratch::new()?;
        let dir = scratch.dir("d")?;
        fs::write(dir.join("Archive.zip"), b"x")?;
        fs::write(dir.join("Archive (2).zip"), b"x")?;
        let name = archive_file_name(OsStr::new("Archive"), Format::Zip)?;
        assert_eq!(
            crate::core::ops::unique_name(&dir, &name)?,
            os("Archive (3).zip")
        );
        fs::write(dir.join("a.tar.gz"), b"x")?;
        let name = archive_file_name(OsStr::new("a"), Format::TarGz)?;
        assert_eq!(
            crate::core::ops::unique_name(&dir, &name)?,
            os("a (2).tar.gz")
        );
        Ok(())
    }

    // --- member safety ---------------------------------------------------

    #[test]
    fn members_are_checked_for_absolute_and_parent_paths() {
        assert_eq!(
            member_problem(b"/etc/passwd"),
            Some(MemberProblem::Absolute)
        );
        assert_eq!(member_problem(b"\\evil"), Some(MemberProblem::Absolute));
        assert_eq!(member_problem(b"C:\\evil"), Some(MemberProblem::Absolute));
        assert_eq!(member_problem(b"../x"), Some(MemberProblem::ParentDir));
        assert_eq!(member_problem(b"a/../b"), Some(MemberProblem::ParentDir));
        assert_eq!(
            member_problem(b"./a/../../b"),
            Some(MemberProblem::ParentDir)
        );
        assert_eq!(member_problem(b"a/.."), Some(MemberProblem::ParentDir));
        assert_eq!(member_problem(b"a\\..\\b"), Some(MemberProblem::ParentDir));
        for fine in [
            &b"a"[..],
            b"a/b/c.txt",
            b"./a",
            b"dir/",
            b"..a",
            b"a..",
            b"a/..b/c",
            b"...",
            b"a:b",
        ] {
            assert_eq!(
                member_problem(fine),
                None,
                "{}",
                String::from_utf8_lossy(fine)
            );
        }
    }

    #[test]
    fn a_listing_is_refused_whole_when_any_member_is_unsafe() {
        assert_eq!(check_listing(b"a\nb/c\nd/\n").ok(), Some(3));
        assert_eq!(check_listing(b"").ok(), Some(0));
        let refused = check_listing(b"a\nb/../../c\nd\n");
        assert!(matches!(
            refused,
            Err(ArchiveError::Unsafe { ref member, problem: MemberProblem::ParentDir })
                if member == "b/../../c"
        ));
        let refused = check_listing(b"a\n/etc/x\n");
        assert!(matches!(
            refused,
            Err(ArchiveError::Unsafe {
                problem: MemberProblem::Absolute,
                ..
            })
        ));
        assert_eq!(
            check_listing(b"a\n../b\n").err().map(|e| e.to_string()),
            Some("The archive is unsafe: \u{201c}../b\u{201d} points outside the destination. Nothing was extracted.".to_owned())
        );
    }

    // --- symlinks --------------------------------------------------------

    #[test]
    fn symlinks_that_resolve_outside_the_root_escape() -> TestResult {
        let scratch = Scratch::new()?;
        let outside = scratch.dir("outside")?;
        fs::write(outside.join("secret"), b"s")?;
        let root = scratch.dir("root")?;
        fs::create_dir_all(root.join("sub"))?;
        fs::write(root.join("file"), b"f")?;
        symlink("file", root.join("ok"))?;
        symlink("../file", root.join("sub/up-ok"))?;
        symlink("sub", root.join("dirlink"))?;
        symlink("missing", root.join("dangling-in"))?;
        symlink(outside.join("secret"), root.join("abs"))?;
        symlink("../outside/secret", root.join("rel"))?;
        symlink("../../outside", root.join("sub/up2"))?;
        symlink("abs", root.join("chain"))?;
        symlink("../nowhere", root.join("dangling-out"))?;
        symlink("/", root.join("rootlink"))?;
        for (link, escapes) in [
            ("ok", false),
            ("sub/up-ok", false),
            ("dirlink", false),
            ("dangling-in", false),
            ("abs", true),
            ("rel", true),
            ("sub/up2", true),
            ("chain", true),
            ("dangling-out", true),
            ("rootlink", true),
        ] {
            assert_eq!(symlink_escapes(&root, &root.join(link))?, escapes, "{link}");
        }
        Ok(())
    }

    // --- arguments -------------------------------------------------------

    fn strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn compress_arguments_use_auto_format_and_relative_members() {
        let args = strings(compress_args(
            Path::new("/d/.fileflux-partial-x.tar.gz"),
            Path::new("/d"),
            &[os("a b"), os("-rf"), os("@list")],
        ));
        assert_eq!(
            args,
            [
                "-a",
                "-cf",
                "/d/.fileflux-partial-x.tar.gz",
                "-C",
                "/d",
                "--",
                "a b",
                "./-rf",
                "./@list"
            ]
        );
    }

    /// bsdtar reads `-C<dir>` in the member list as "change directory" even
    /// after `--`, so every name that starts like an option or an `@` list
    /// is archived as a file through `./`, and nothing outside the
    /// selection comes in.
    #[test]
    fn option_like_member_names_are_archived_as_files() -> TestResult {
        let Some(tool) = real_tool() else {
            return Ok(());
        };
        let scratch = Scratch::new()?;
        let folder = scratch.dir("src")?;
        fs::write(scratch.0.join("private.txt"), b"outside")?;
        let names = [
            "-",
            "--",
            "-C",
            "-T",
            "-C..",
            "@x",
            "-P",
            "--version",
            "-rf",
            "private.txt",
        ];
        for name in names {
            fs::write(folder.join(name), name)?;
        }
        let members: Vec<OsString> = names.iter().map(|name| os(name)).collect();
        let never = AtomicBool::new(false);
        let made = compress(
            &tool,
            &folder,
            &members,
            OsStr::new("-x"),
            Format::TarGz,
            &never,
        )?;
        let dest = scratch.dir("dest")?;
        let got = extract(&tool, &made, &dest, &never)?;
        let mut want: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
        want.sort();
        assert_eq!(entries(&got.path)?, want);
        for name in names {
            assert_eq!(fs::read(got.path.join(name))?, name.as_bytes());
        }
        Ok(())
    }

    #[test]
    fn listing_and_extraction_arguments_are_conservative() {
        assert_eq!(
            strings(list_args(Path::new("/d/a.zip"))),
            ["-tf", "/d/a.zip"]
        );
        let args = strings(extract_args(Path::new("/d/a.zip"), Path::new("/d/tmp")));
        assert_eq!(
            args,
            [
                "-xf",
                "/d/a.zip",
                "-C",
                "/d/tmp",
                "--no-same-owner",
                "--no-same-permissions"
            ]
        );
        assert!(!args.iter().any(|arg| arg.contains('P')));
    }

    // --- real bsdtar -----------------------------------------------------

    fn real_tool() -> Option<PathBuf> {
        let found = tool().map(Path::to_path_buf);
        if found.is_none() {
            eprintln!("skipped: bsdtar is not on PATH");
        }
        found
    }

    fn entries(dir: &Path) -> io::Result<Vec<String>> {
        let mut names: Vec<String> = fs::read_dir(dir)?
            .map(|entry| entry.map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect::<Result<_, _>>()?;
        names.sort();
        Ok(names)
    }

    /// Writes `bytes` into `header` at `at`.
    fn put(header: &mut [u8; 512], at: usize, bytes: &[u8]) {
        if let Some(field) = header.get_mut(at..at + bytes.len()) {
            field.copy_from_slice(bytes);
        }
    }

    /// One ustar header + data, written by hand so names bsdtar would
    /// refuse to create can be put in an archive.
    fn tar_entry(name: &[u8], kind: u8, link: &[u8], data: &[u8]) -> Vec<u8> {
        tar_entry_mode(name, kind, link, data, b"0000644\0")
    }

    fn tar_entry_mode(name: &[u8], kind: u8, link: &[u8], data: &[u8], mode: &[u8; 8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        put(&mut header, 0, name);
        put(&mut header, 100, mode);
        put(&mut header, 108, b"0000000\0");
        put(&mut header, 116, b"0000000\0");
        put(
            &mut header,
            124,
            format!("{:011o}\0", data.len()).as_bytes(),
        );
        put(&mut header, 136, b"00000000000\0");
        put(&mut header, 148, b"        ");
        put(&mut header, 156, &[kind]);
        put(&mut header, 157, link);
        put(&mut header, 257, b"ustar\x0000");
        let sum: u32 = header.iter().map(|&b| u32::from(b)).sum();
        put(&mut header, 148, format!("{sum:06o}\0 ").as_bytes());
        let mut out = header.to_vec();
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
        out
    }

    fn write_tar(path: &Path, entries: &[Vec<u8>]) -> io::Result<()> {
        let mut bytes: Vec<u8> = entries.concat();
        bytes.extend_from_slice(&[0u8; 1024]);
        fs::write(path, bytes)
    }

    #[test]
    fn compress_then_extract_round_trips_every_format() -> TestResult {
        let Some(tool) = real_tool() else {
            return Ok(());
        };
        for format in Format::ALL {
            let scratch = Scratch::new()?;
            let folder = scratch.dir("src")?;
            fs::create_dir_all(folder.join("photos/deep"))?;
            fs::write(folder.join("photos/deep/a.txt"), b"hello")?;
            fs::write(folder.join("note.txt"), b"note")?;
            fs::write(folder.join("@odd"), b"at")?;
            symlink("note.txt", folder.join("photos/link"))?;
            let members = [os("photos"), os("note.txt"), os("@odd")];
            let never = AtomicBool::new(false);
            let made = compress(
                &tool,
                &folder,
                &members,
                OsStr::new("Archive"),
                format,
                &never,
            )?;
            assert_eq!(made, folder.join(format!("Archive{}", format.suffix())));
            // No temp left, nothing else added.
            assert_eq!(
                entries(&folder)?,
                [
                    "@odd",
                    format!("Archive{}", format.suffix()).as_str(),
                    "note.txt",
                    "photos"
                ]
            );
            // A second run never overwrites.
            let again = compress(
                &tool,
                &folder,
                &members[1..2],
                OsStr::new("Archive"),
                format,
                &never,
            )?;
            assert_eq!(
                again,
                folder.join(format!("Archive (2){}", format.suffix()))
            );
            assert!(!fs::read(&made)?.is_empty());

            let dest = scratch.dir("dest")?;
            let got = extract(&tool, &made, &dest, &never)?;
            assert_eq!(got.path, dest.join("Archive"));
            assert!(got.removed_links.is_empty());
            assert_eq!(fs::read(got.path.join("photos/deep/a.txt"))?, b"hello");
            assert_eq!(fs::read(got.path.join("note.txt"))?, b"note");
            assert_eq!(fs::read(got.path.join("@odd"))?, b"at");
            assert_eq!(
                fs::read_link(got.path.join("photos/link"))?,
                Path::new("note.txt")
            );
            assert_eq!(entries(&dest)?, ["Archive"]);
            // Extracting again picks a new folder.
            let second = extract(&tool, &made, &dest, &never)?;
            assert_eq!(second.path, dest.join("Archive (2)"));
            assert_eq!(fs::read(got.path.join("note.txt"))?, b"note");
        }
        Ok(())
    }

    #[test]
    fn an_archive_with_parent_members_is_refused_and_nothing_is_written() -> TestResult {
        let Some(tool) = real_tool() else {
            return Ok(());
        };
        let scratch = Scratch::new()?;
        let dest = scratch.dir("dest")?;
        let evil = scratch.0.join("evil.tar");
        write_tar(
            &evil,
            &[
                tar_entry(b"fine.txt", b'0', b"", b"ok"),
                tar_entry(b"../escaped.txt", b'0', b"", b"bad"),
            ],
        )?;
        let never = AtomicBool::new(false);
        let refused = extract(&tool, &evil, &dest, &never);
        assert!(matches!(
            refused,
            Err(ArchiveError::Unsafe {
                problem: MemberProblem::ParentDir,
                ..
            })
        ));
        assert!(entries(&dest)?.is_empty());
        assert!(!scratch.0.join("escaped.txt").exists());

        let absolute = scratch.0.join("abs.tar");
        let target = scratch.0.join("abs-escaped.txt");
        write_tar(
            &absolute,
            &[tar_entry(target.as_os_str().as_bytes(), b'0', b"", b"bad")],
        )?;
        assert!(matches!(
            extract(&tool, &absolute, &dest, &never),
            Err(ArchiveError::Unsafe {
                problem: MemberProblem::Absolute,
                ..
            })
        ));
        assert!(entries(&dest)?.is_empty());
        assert!(!target.exists());
        Ok(())
    }

    #[test]
    fn bsdtar_itself_refuses_parent_members_without_our_check() -> TestResult {
        let Some(tool) = real_tool() else {
            return Ok(());
        };
        let scratch = Scratch::new()?;
        let dest = scratch.dir("dest/inner")?;
        let evil = scratch.0.join("evil.tar");
        write_tar(&evil, &[tar_entry(b"../escaped.txt", b'0', b"", b"bad")])?;
        Command::new(&tool)
            .args(extract_args(&evil, &dest))
            .output()?;
        assert!(!scratch.0.join("dest/escaped.txt").exists());
        let absolute = scratch.0.join("abs.tar");
        let target = scratch.0.join("abs-escaped.txt");
        write_tar(
            &absolute,
            &[tar_entry(target.as_os_str().as_bytes(), b'0', b"", b"bad")],
        )?;
        Command::new(&tool)
            .args(extract_args(&absolute, &dest))
            .output()?;
        assert!(!target.exists());
        Ok(())
    }

    #[test]
    fn symlinks_pointing_outside_are_removed_and_reported() -> TestResult {
        let Some(tool) = real_tool() else {
            return Ok(());
        };
        let scratch = Scratch::new()?;
        let dest = scratch.dir("dest")?;
        let tarball = scratch.0.join("links.tar");
        write_tar(
            &tarball,
            &[
                tar_entry(b"file", b'0', b"", b"f"),
                tar_entry(b"good", b'2', b"file", b""),
                tar_entry(b"bad-abs", b'2', b"/etc/passwd", b""),
                tar_entry(b"bad-rel", b'2', b"../../x", b""),
            ],
        )?;
        let never = AtomicBool::new(false);
        let got = extract(&tool, &tarball, &dest, &never)?;
        assert_eq!(entries(&got.path)?, ["file", "good"]);
        let mut removed = got.removed_links.clone();
        removed.sort();
        assert_eq!(
            removed,
            [PathBuf::from("bad-abs"), PathBuf::from("bad-rel")]
        );
        Ok(())
    }

    #[test]
    fn a_hard_link_to_an_outside_file_never_reaches_it() -> TestResult {
        let Some(tool) = real_tool() else {
            return Ok(());
        };
        let scratch = Scratch::new()?;
        let dest = scratch.dir("dest")?;
        let secret = scratch.0.join("secret");
        fs::write(&secret, b"s")?;
        let tarball = scratch.0.join("hard.tar");
        write_tar(
            &tarball,
            &[tar_entry(b"h", b'1', secret.as_os_str().as_bytes(), b"")],
        )?;
        let never = AtomicBool::new(false);
        extract(&tool, &tarball, &dest, &never).ok();
        // Whatever happened, the secret is intact and nothing outside
        // the destination changed.
        assert_eq!(fs::read(&secret)?, b"s");
        assert_eq!(
            std::os::unix::fs::MetadataExt::nlink(&fs::symlink_metadata(&secret)?),
            1
        );
        Ok(())
    }

    #[test]
    fn a_failed_extraction_leaves_no_temp_folder() -> TestResult {
        let Some(tool) = real_tool() else {
            return Ok(());
        };
        let scratch = Scratch::new()?;
        let dest = scratch.dir("dest")?;
        let junk = scratch.0.join("junk.zip");
        fs::write(&junk, b"this is not an archive")?;
        let never = AtomicBool::new(false);
        assert!(extract(&tool, &junk, &dest, &never).is_err());
        assert!(entries(&dest)?.is_empty());
        Ok(())
    }

    #[test]
    fn cancelling_leaves_no_temp_and_no_archive() -> TestResult {
        let Some(tool) = real_tool() else {
            return Ok(());
        };
        let scratch = Scratch::new()?;
        let folder = scratch.dir("src")?;
        fs::write(folder.join("a.txt"), b"hello")?;
        let cancelled = AtomicBool::new(true);
        let result = compress(
            &tool,
            &folder,
            &[os("a.txt")],
            OsStr::new("x"),
            Format::Zip,
            &cancelled,
        );
        assert!(matches!(result, Err(ArchiveError::Cancelled)));
        assert_eq!(entries(&folder)?, ["a.txt"]);

        let made = {
            let never = AtomicBool::new(false);
            compress(
                &tool,
                &folder,
                &[os("a.txt")],
                OsStr::new("x"),
                Format::Zip,
                &never,
            )?
        };
        let dest = scratch.dir("dest")?;
        let result = extract(&tool, &made, &dest, &cancelled);
        assert!(matches!(result, Err(ArchiveError::Cancelled)));
        assert!(entries(&dest)?.is_empty());
        Ok(())
    }

    #[test]
    fn cancelling_a_running_compression_kills_it_and_removes_the_temp() -> TestResult {
        let Some(tool) = real_tool() else {
            return Ok(());
        };
        let scratch = Scratch::new()?;
        let folder = scratch.dir("src")?;
        // Incompressible data keeps xz busy for seconds.
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let noise: Vec<u8> = (0..48 << 20)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect();
        fs::write(folder.join("noise.bin"), &noise)?;
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&cancel);
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            flag.store(true, Ordering::Relaxed);
        });
        let started = std::time::Instant::now();
        let result = compress(
            &tool,
            &folder,
            &[os("noise.bin")],
            OsStr::new("n"),
            Format::TarXz,
            &cancel,
        );
        stopper.join().map_err(|_| "stopper panicked")?;
        assert!(matches!(result, Err(ArchiveError::Cancelled)));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(entries(&folder)?, ["noise.bin"]);
        Ok(())
    }

    #[test]
    fn a_missing_member_fails_without_leaving_a_partial_archive() -> TestResult {
        let Some(tool) = real_tool() else {
            return Ok(());
        };
        let scratch = Scratch::new()?;
        let folder = scratch.dir("src")?;
        let never = AtomicBool::new(false);
        let result = compress(
            &tool,
            &folder,
            &[os("gone")],
            OsStr::new("x"),
            Format::Zip,
            &never,
        );
        assert!(matches!(result, Err(ArchiveError::Tool(_))));
        assert!(entries(&folder)?.is_empty());
        Ok(())
    }

    #[test]
    fn modes_without_owner_access_do_not_block_cleanup() -> TestResult {
        let Some(tool) = real_tool() else {
            return Ok(());
        };
        let scratch = Scratch::new()?;
        let tarball = scratch.0.join("modes.tar");
        let dir = tar_entry_mode(b"locked/", b'5', b"", b"", b"0000000\0");
        write_tar(&tarball, &[dir, tar_entry(b"locked/f", b'0', b"", b"x")])?;
        let dest = scratch.dir("dest")?;
        let never = AtomicBool::new(false);
        let got = extract(&tool, &tarball, &dest, &never);
        // Either it worked or it failed cleanly; never a leftover temp.
        let names = entries(&dest)?;
        assert!(
            names.iter().all(|n| !n.starts_with(".fileflux-")),
            "{names:?}"
        );
        drop(got);
        Ok(())
    }
}
