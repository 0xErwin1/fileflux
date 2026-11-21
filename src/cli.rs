//! The command line: `fileflux [PATH]` and `fileflux IMAGE...`.
//!
//! Parsing and start-path resolution live here, free of GPUI, so the binary
//! can reject bad input before any window opens and the rules stay testable.

use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use crate::core::images::is_image_path;

/// The text `--help` prints.
pub const USAGE: &str = "\
Usage: fileflux [OPTIONS] [PATH]
       fileflux [OPTIONS] IMAGE...

Open the Fileflux explorer at PATH, or at your home directory when PATH is
omitted. A file path opens its parent directory, except an image file (png,
jpg, gif, webp, bmp, tiff, ico, svg, qoi, tga, hdr, exr), which opens the
image viewer on it; the viewer browses the other images of the same folder.

Several IMAGE paths open the viewer on exactly that set. Directories and
other files among them are ignored, with a note on stderr.

Options:
  -h, --help       Print this help and exit
  -V, --version    Print the version and exit
";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Open the explorer or the viewer on the paths that were passed (none
    /// opens the explorer at home).
    Open(Vec<PathBuf>),
    Help,
    Version,
}

/// A command line that cannot be honored.
#[derive(Debug, PartialEq, Eq)]
pub enum CliError {
    UnknownOption(String),
    TooManyPaths,
    /// The start path does not exist (shown absolute).
    Missing(PathBuf),
    /// `~` was used but there is no known home to expand it against.
    NoHome,
    /// The start path exists but could not be inspected.
    Unreadable(PathBuf, String),
    /// A relative start path was given but the current directory is gone.
    NoCurrentDir(String),
}

impl CliError {
    /// Whether the user should be pointed at `--help`.
    pub fn suggests_help(&self) -> bool {
        matches!(self, Self::UnknownOption(_) | Self::TooManyPaths)
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOption(option) => write!(f, "unknown option '{option}'"),
            Self::TooManyPaths => f.write_str("expected at most one path"),
            Self::Missing(path) => write!(f, "'{}' does not exist", path.display()),
            Self::NoHome => f.write_str("'~' needs $HOME to be set"),
            Self::Unreadable(path, reason) => {
                write!(f, "cannot open '{}': {reason}", path.display())
            }
            Self::NoCurrentDir(reason) => {
                write!(f, "cannot resolve a relative path: {reason}")
            }
        }
    }
}

impl std::error::Error for CliError {}

/// Parses the arguments after the program name. `--` ends option parsing,
/// so `fileflux -- -odd-name` opens a directory called `-odd-name`.
pub fn parse<I>(args: I) -> Result<Command, CliError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut paths = Vec::new();
    let mut options_done = false;
    for arg in args {
        if !options_done {
            match arg.to_str() {
                Some("-h" | "--help") => return Ok(Command::Help),
                Some("-V" | "--version") => return Ok(Command::Version),
                Some("--") => {
                    options_done = true;
                    continue;
                }
                Some(option) if option.starts_with('-') && option != "-" => {
                    return Err(CliError::UnknownOption(option.to_owned()));
                }
                _ => {}
            }
        }
        paths.push(PathBuf::from(arg));
    }
    // Several paths only make sense as a set of images for the viewer.
    if paths.len() > 1 && !paths.iter().any(|path| is_image_path(path)) {
        return Err(CliError::TooManyPaths);
    }
    Ok(Command::Open(paths))
}

/// What a resolved command line opens.
#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    /// The explorer, at this directory (home when `None`).
    Explorer(Option<PathBuf>),
    Viewer(ViewerLaunch),
}

/// The viewer's starting point.
#[derive(Debug, PartialEq, Eq)]
pub struct ViewerLaunch {
    /// Absolute image files; the first one is shown first.
    pub images: Vec<PathBuf>,
    pub list: ImageList,
}

/// Where the viewer's navigation list comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageList {
    /// The images next to the shown one, in the explorer's name order.
    Siblings,
    /// Exactly the paths that were passed.
    Given,
}

/// A resolved command line plus the notes to print on stderr.
#[derive(Debug, PartialEq, Eq)]
pub struct Resolved {
    pub target: Target,
    pub notes: Vec<String>,
}

/// What an existing path is, for deciding between explorer and viewer.
enum Found {
    Directory,
    Image,
    OtherFile,
}

/// Resolves the parsed paths (see [`resolve_start`] for how each one is
/// made absolute). One path keeps its contract: a directory opens the
/// explorer there, a file opens its parent, an image opens the viewer.
/// Several paths open the viewer on their images and report the rest in
/// [`Resolved::notes`]; without any image they are rejected like before.
pub fn resolve(
    paths: &[PathBuf],
    cwd: impl FnOnce() -> std::io::Result<PathBuf>,
    home: Option<&Path>,
) -> Result<Resolved, CliError> {
    let explorer = |target| Resolved {
        target: Target::Explorer(target),
        notes: Vec::new(),
    };
    let [first, rest @ ..] = paths else {
        return Ok(explorer(None));
    };
    // The current directory is asked for at most once, and only when a
    // relative path needs it.
    let mut ask = Some(cwd);
    let mut known: Option<PathBuf> = None;
    let mut absolute = |path: &Path| {
        absolutize(
            path,
            || {
                if let Some(dir) = &known {
                    return Ok(dir.clone());
                }
                let dir = match ask.take() {
                    Some(ask) => ask()?,
                    None => return Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
                };
                known = Some(dir.clone());
                Ok(dir)
            },
            home,
        )
    };
    if rest.is_empty() {
        let path = absolute(first)?;
        return match inspect(&path)? {
            Found::Image => Ok(Resolved {
                target: Target::Viewer(ViewerLaunch {
                    images: vec![path],
                    list: ImageList::Siblings,
                }),
                notes: Vec::new(),
            }),
            Found::Directory => Ok(explorer(Some(path))),
            Found::OtherFile => Ok(explorer(Some(parent_of(&path)))),
        };
    }

    let mut images: Vec<PathBuf> = Vec::new();
    let mut notes = Vec::new();
    for path in paths {
        let path = absolute(path)?;
        match inspect(&path)? {
            Found::Image if images.contains(&path) => {}
            Found::Image => images.push(path),
            Found::Directory => notes.push(format!("ignoring '{}': a directory", path.display())),
            Found::OtherFile => {
                notes.push(format!("ignoring '{}': not an image file", path.display()));
            }
        }
    }
    if images.is_empty() {
        return Err(CliError::TooManyPaths);
    }
    Ok(Resolved {
        target: Target::Viewer(ViewerLaunch {
            images,
            list: ImageList::Given,
        }),
        notes,
    })
}

fn inspect(absolute: &Path) -> Result<Found, CliError> {
    match std::fs::metadata(absolute) {
        Ok(meta) if meta.is_dir() => Ok(Found::Directory),
        Ok(_) if is_image_path(absolute) => Ok(Found::Image),
        Ok(_) => Ok(Found::OtherFile),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(CliError::Missing(absolute.to_path_buf()))
        }
        Err(error) => Err(CliError::Unreadable(
            absolute.to_path_buf(),
            error.to_string(),
        )),
    }
}

fn parent_of(path: &Path) -> PathBuf {
    path.parent()
        .map_or_else(|| PathBuf::from("/"), Path::to_path_buf)
}

/// Turns the start path into the absolute directory to open: a leading `~`
/// expands against `home` (for quoted arguments the shell left alone),
/// relative paths resolve against `cwd` (asked for only then), and a file
/// opens its parent. `.` and `..` are resolved lexically, like the shell's
/// `cd`, so the path bar and "up" follow what was typed.
pub fn resolve_start(
    path: &Path,
    cwd: impl FnOnce() -> std::io::Result<PathBuf>,
    home: Option<&Path>,
) -> Result<PathBuf, CliError> {
    let absolute = absolutize(path, cwd, home)?;
    match inspect(&absolute)? {
        Found::Directory => Ok(absolute),
        Found::Image | Found::OtherFile => Ok(parent_of(&absolute)),
    }
}

/// The absolute form of a command-line path (`~` expanded, relative paths
/// anchored at `cwd`, lexically), without looking at the filesystem: what a
/// second launch forwards to the running instance.
pub fn absolute_path(
    path: &Path,
    cwd: impl FnOnce() -> std::io::Result<PathBuf>,
    home: Option<&Path>,
) -> Result<PathBuf, CliError> {
    absolutize(path, cwd, home)
}

/// Expands `~` and anchors relative paths, lexically.
fn absolutize(
    path: &Path,
    cwd: impl FnOnce() -> std::io::Result<PathBuf>,
    home: Option<&Path>,
) -> Result<PathBuf, CliError> {
    let expanded = match path.strip_prefix("~") {
        Ok(rest) => home.ok_or(CliError::NoHome)?.join(rest),
        Err(_) => path.to_path_buf(),
    };
    if expanded.is_absolute() {
        Ok(normalize(&expanded))
    } else {
        let cwd = cwd().map_err(|error| CliError::NoCurrentDir(error.to_string()))?;
        Ok(normalize(&cwd.join(expanded)))
    }
}

/// Drops `.` segments and trailing slashes and folds `..` into its parent,
/// without touching the filesystem. `..` above the root stays at the root.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    /// A scratch directory under the system temp dir, removed when dropped
    /// so test runs leave nothing behind.
    struct Scratch(PathBuf);

    impl std::ops::Deref for Scratch {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl AsRef<Path> for Scratch {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if let Err(error) = std::fs::remove_dir_all(&self.0) {
                eprintln!("could not remove {}: {error}", self.0.display());
            }
        }
    }

    fn scratch(name: &str) -> Result<Scratch, std::io::Error> {
        let dir =
            std::env::temp_dir().join(format!("fileflux-cli-unit-{name}-{}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        std::fs::create_dir_all(&dir)?;
        Ok(Scratch(dir))
    }

    #[test]
    fn a_unit_scratch_directory_is_removed_when_dropped() -> Result<(), std::io::Error> {
        let path = {
            let dir = scratch("cleanup")?;
            std::fs::write(dir.join("left-over"), b"x")?;
            dir.to_path_buf()
        };
        assert!(!path.exists(), "{} was left behind", path.display());
        Ok(())
    }

    /// A current directory that resolves to `dir`.
    fn at(dir: &Path) -> impl FnOnce() -> std::io::Result<PathBuf> {
        let dir = dir.to_path_buf();
        move || Ok(dir)
    }

    /// A deleted current directory: absolute and `~` paths must not need it.
    fn no_cwd() -> std::io::Result<PathBuf> {
        Err(std::io::Error::from(std::io::ErrorKind::NotFound))
    }

    #[test]
    fn no_arguments_open_the_default_location() {
        assert_eq!(parse(args(&[])), Ok(Command::Open(Vec::new())));
    }

    #[test]
    fn one_path_is_kept_verbatim() {
        assert_eq!(
            parse(args(&["~/Pictures/"])),
            Ok(Command::Open(vec![PathBuf::from("~/Pictures/")]))
        );
    }

    #[test]
    fn help_and_version_win_over_paths() {
        assert_eq!(parse(args(&["x", "--help"])), Ok(Command::Help));
        assert_eq!(parse(args(&["-V", "x"])), Ok(Command::Version));
    }

    #[test]
    fn double_dash_allows_dash_names() {
        assert_eq!(
            parse(args(&["--", "-odd"])),
            Ok(Command::Open(vec![PathBuf::from("-odd")]))
        );
        assert_eq!(
            parse(args(&["-"])),
            Ok(Command::Open(vec![PathBuf::from("-")]))
        );
    }

    #[test]
    fn rejects_unknown_options_and_extra_paths() {
        assert_eq!(
            parse(args(&["-x"])),
            Err(CliError::UnknownOption("-x".to_owned()))
        );
        assert_eq!(parse(args(&["a", "b"])), Err(CliError::TooManyPaths));
    }

    #[test]
    fn several_paths_are_kept_only_when_an_image_is_among_them() {
        assert_eq!(
            parse(args(&["a.PNG", "notes.txt", "b.jpg"])),
            Ok(Command::Open(vec![
                PathBuf::from("a.PNG"),
                PathBuf::from("notes.txt"),
                PathBuf::from("b.jpg"),
            ]))
        );
        assert_eq!(parse(args(&["a", "b.txt"])), Err(CliError::TooManyPaths));
    }

    fn write(path: &Path) -> TestResult {
        std::fs::write(path, b"x")?;
        Ok(())
    }

    #[test]
    fn no_paths_open_the_explorer_at_home() -> TestResult {
        assert_eq!(
            resolve(&[], no_cwd, None)?,
            Resolved {
                target: Target::Explorer(None),
                notes: Vec::new()
            }
        );
        Ok(())
    }

    #[test]
    fn one_path_keeps_the_explorer_contract_except_for_images() -> TestResult {
        let root = scratch("one")?;
        std::fs::create_dir_all(root.join("Pictures"))?;
        write(&root.join("Pictures/photo.PNG"))?;
        write(&root.join("Pictures/notes.txt"))?;
        let one = |path: &str| resolve(&[PathBuf::from(path)], at(&root), None);

        assert_eq!(
            one("Pictures")?.target,
            Target::Explorer(Some(root.join("Pictures")))
        );
        assert_eq!(
            one("Pictures/notes.txt")?.target,
            Target::Explorer(Some(root.join("Pictures")))
        );
        assert_eq!(
            one("Pictures/./photo.PNG")?,
            Resolved {
                target: Target::Viewer(ViewerLaunch {
                    images: vec![root.join("Pictures/photo.PNG")],
                    list: ImageList::Siblings,
                }),
                notes: Vec::new(),
            }
        );
        assert_eq!(
            one("Pictures/gone.png"),
            Err(CliError::Missing(root.join("Pictures/gone.png")))
        );
        Ok(())
    }

    #[test]
    fn a_directory_named_like_an_image_opens_the_explorer() -> TestResult {
        let root = scratch("dirpng")?;
        std::fs::create_dir_all(root.join("shots.png"))?;
        assert_eq!(
            resolve(&[PathBuf::from("shots.png")], at(&root), None)?.target,
            Target::Explorer(Some(root.join("shots.png")))
        );
        Ok(())
    }

    #[test]
    fn several_paths_open_the_given_images_and_note_the_rest() -> TestResult {
        let root = scratch("many")?;
        std::fs::create_dir_all(root.join("dir"))?;
        for name in ["b.png", "a.jpg", "notes.txt"] {
            write(&root.join(name))?;
        }
        let paths: Vec<PathBuf> = ["b.png", "notes.txt", "dir", "a.jpg", "./b.png"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        let resolved = resolve(&paths, at(&root), None)?;
        assert_eq!(
            resolved.target,
            Target::Viewer(ViewerLaunch {
                images: vec![root.join("b.png"), root.join("a.jpg")],
                list: ImageList::Given,
            })
        );
        assert_eq!(
            resolved.notes,
            vec![
                format!(
                    "ignoring '{}': not an image file",
                    root.join("notes.txt").display()
                ),
                format!("ignoring '{}': a directory", root.join("dir").display()),
            ]
        );
        Ok(())
    }

    #[test]
    fn several_paths_without_an_image_are_rejected() -> TestResult {
        let root = scratch("noimg")?;
        std::fs::create_dir_all(root.join("dir"))?;
        write(&root.join("notes.txt"))?;
        let paths = [PathBuf::from("dir"), PathBuf::from("notes.txt")];
        assert_eq!(
            resolve(&paths, at(&root), None),
            Err(CliError::TooManyPaths)
        );
        Ok(())
    }

    #[test]
    fn resolves_relative_tilde_and_file_paths() -> TestResult {
        let root = scratch("resolve")?;
        std::fs::create_dir_all(root.join("Pictures"))?;
        std::fs::write(root.join("Pictures/photo.png"), b"")?;

        assert_eq!(
            resolve_start(Path::new("Pictures"), at(&root), None)?,
            root.join("Pictures")
        );
        assert_eq!(
            resolve_start(Path::new("~/Pictures/"), no_cwd, Some(&root))?,
            root.join("Pictures")
        );
        assert_eq!(
            resolve_start(Path::new("~"), no_cwd, Some(&root))?.as_os_str(),
            root.as_os_str()
        );
        assert_eq!(
            resolve_start(Path::new("Pictures/photo.png"), at(&root), None)?,
            root.join("Pictures")
        );
        assert_eq!(
            resolve_start(&root.join("Pictures"), no_cwd, None)?,
            root.join("Pictures")
        );
        Ok(())
    }

    #[test]
    fn folds_dot_dot_and_needs_cwd_only_for_relative_paths() -> TestResult {
        let root = scratch("dotdot")?;
        std::fs::create_dir_all(root.join("a/b"))?;
        std::fs::create_dir_all(root.join("sibling"))?;

        assert_eq!(
            resolve_start(Path::new(".."), at(&root.join("a/b")), None)?,
            root.join("a")
        );
        assert_eq!(
            resolve_start(Path::new("../../sibling/."), at(&root.join("a/b")), None)?,
            root.join("sibling")
        );
        assert_eq!(
            resolve_start(Path::new("/.."), no_cwd, None)?,
            PathBuf::from("/")
        );
        assert!(matches!(
            resolve_start(Path::new("a"), no_cwd, None),
            Err(CliError::NoCurrentDir(_))
        ));
        Ok(())
    }

    #[test]
    fn reports_missing_paths_and_missing_home() -> TestResult {
        let root = scratch("missing")?;
        assert_eq!(
            resolve_start(Path::new("nope"), at(&root), None),
            Err(CliError::Missing(root.join("nope")))
        );
        assert_eq!(
            resolve_start(Path::new("~/x"), at(&root), None),
            Err(CliError::NoHome)
        );
        Ok(())
    }
}
