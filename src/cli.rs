//! The command line: `fileflux [PATH]`.
//!
//! Parsing and start-path resolution live here, free of GPUI, so the binary
//! can reject bad input before any window opens and the rules stay testable.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

/// The text `--help` prints.
pub const USAGE: &str = "\
Usage: fileflux [OPTIONS] [PATH]

Open the Fileflux explorer at PATH, or at your home directory when PATH is
omitted. A file path opens its parent directory.

Options:
  -h, --help       Print this help and exit
  -V, --version    Print the version and exit
";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Open the explorer, at the given path when one was passed.
    Open(Option<PathBuf>),
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
    let mut path = None;
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
        if path.replace(PathBuf::from(arg)).is_some() {
            return Err(CliError::TooManyPaths);
        }
    }
    Ok(Command::Open(path))
}

/// Turns the start path into the absolute directory to open: a leading `~`
/// expands against `home` (for quoted arguments the shell left alone),
/// relative paths resolve against `cwd`, and a file opens its parent.
/// Symlinks are kept as written, so the path bar shows what was typed.
pub fn resolve_start(path: &Path, cwd: &Path, home: Option<&Path>) -> Result<PathBuf, CliError> {
    let expanded = match path.strip_prefix("~") {
        Ok(rest) => home.ok_or(CliError::NoHome)?.join(rest),
        Err(_) => path.to_path_buf(),
    };
    // Collecting the components drops trailing slashes and `.` segments, so
    // `~/Pictures/` shows as `/home/me/Pictures`.
    let absolute: PathBuf = cwd.join(expanded).components().collect();
    match std::fs::metadata(&absolute) {
        Ok(meta) if meta.is_dir() => Ok(absolute),
        Ok(_) => Ok(absolute
            .parent()
            .map_or_else(|| PathBuf::from("/"), Path::to_path_buf)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(CliError::Missing(absolute))
        }
        Err(error) => Err(CliError::Unreadable(absolute, error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn scratch(name: &str) -> Result<PathBuf, std::io::Error> {
        let dir =
            std::env::temp_dir().join(format!("fileflux-cli-unit-{name}-{}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    #[test]
    fn no_arguments_open_the_default_location() {
        assert_eq!(parse(args(&[])), Ok(Command::Open(None)));
    }

    #[test]
    fn one_path_is_kept_verbatim() {
        assert_eq!(
            parse(args(&["~/Pictures/"])),
            Ok(Command::Open(Some(PathBuf::from("~/Pictures/"))))
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
            Ok(Command::Open(Some(PathBuf::from("-odd"))))
        );
        assert_eq!(
            parse(args(&["-"])),
            Ok(Command::Open(Some(PathBuf::from("-"))))
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
    fn resolves_relative_tilde_and_file_paths() -> TestResult {
        let root = scratch("resolve")?;
        std::fs::create_dir_all(root.join("Pictures"))?;
        std::fs::write(root.join("Pictures/photo.png"), b"")?;

        assert_eq!(
            resolve_start(Path::new("Pictures"), &root, None)?,
            root.join("Pictures")
        );
        assert_eq!(
            resolve_start(Path::new("~/Pictures/"), Path::new("/"), Some(&root))?,
            root.join("Pictures")
        );
        assert_eq!(
            resolve_start(Path::new("~"), Path::new("/"), Some(&root))?.as_os_str(),
            root.as_os_str()
        );
        assert_eq!(
            resolve_start(Path::new("Pictures/photo.png"), &root, None)?,
            root.join("Pictures")
        );
        assert_eq!(
            resolve_start(&root.join("Pictures"), Path::new("/elsewhere"), None)?,
            root.join("Pictures")
        );
        Ok(())
    }

    #[test]
    fn reports_missing_paths_and_missing_home() -> TestResult {
        let root = scratch("missing")?;
        assert_eq!(
            resolve_start(Path::new("nope"), &root, None),
            Err(CliError::Missing(root.join("nope")))
        );
        assert_eq!(
            resolve_start(Path::new("~/x"), &root, None),
            Err(CliError::NoHome)
        );
        Ok(())
    }
}
