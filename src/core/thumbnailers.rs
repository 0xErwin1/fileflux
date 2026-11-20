//! The freedesktop thumbnailers: the `*.thumbnailer` files installed under
//! the data directories. This is how pcmanfm, Nautilus and Thunar get a
//! preview of anything they cannot decode themselves — videos through
//! `ffmpegthumbnailer` or `totem-video-thumbnailer`, PDFs through `evince` or
//! `pdftoppm`, office documents through libreoffice — so Fileflux runs the
//! same commands instead of growing a decoder per format.
//!
//! Nothing is trusted blindly: the command is spawned without a shell, its
//! output is read from a private temporary file, killed after
//! [`THUMBNAILER_TIMEOUT`], and decoded here rather than copied into the
//! cache, so a broken thumbnailer cannot write a file the other managers read.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::apps::split_exec;
use super::thumbnails::thumbnail_uri;

/// How long one thumbnailer may run before it is killed. libfm allows 30 s,
/// because it also thumbnails files on network mounts; Fileflux skips those,
/// so a command that has not answered in this long is stuck, and the thread
/// waiting on it is one the folder scans need.
pub const THUMBNAILER_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the running thumbnailer is asked whether it finished.
const POLL: Duration = Duration::from_millis(25);

/// One installed thumbnailer: the `Exec=` line its file declares and the MIME
/// types it claims.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thumbnailer {
    /// The `Exec=` line, with the field codes still in place.
    pub exec: String,
    /// The `MimeType=` list, as written.
    pub mime_types: Vec<String>,
    /// The file it came from, for logs.
    pub path: PathBuf,
}

/// The installed thumbnailers: the user's data directory first, then each
/// system data directory, with the precedence the spec gives `.desktop`
/// files (a file of the same name in a later directory is ignored).
#[must_use]
pub fn installed() -> &'static [Thumbnailer] {
    static INSTALLED: LazyLock<Vec<Thumbnailer>> = LazyLock::new(|| discover_from_env());
    &INSTALLED
}

/// The first installed thumbnailer that claims `mime`, in discovery order.
#[must_use]
pub fn for_mime(mime: &str) -> Option<&'static Thumbnailer> {
    installed().iter().find(|thumbnailer| {
        thumbnailer
            .mime_types
            .iter()
            .any(|claimed| claimed.eq_ignore_ascii_case(mime))
    })
}

/// The data directories to search, in order: `$XDG_DATA_HOME` (fallback
/// `~/.local/share`), then `$XDG_DATA_DIRS` (fallback
/// `/usr/local/share:/usr/share`). Relative entries are ignored, as the XDG
/// specification says.
#[must_use]
pub fn data_directories() -> Vec<PathBuf> {
    let mut directories = Vec::new();
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        let data_home = PathBuf::from(data_home);
        if data_home.is_absolute() {
            directories.push(data_home);
        }
    } else if let Some(home) = std::env::var_os("HOME") {
        directories.push(Path::new(&home).join(".local/share"));
    }
    let data_dirs =
        std::env::var_os("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    directories.extend(
        std::env::split_paths(&data_dirs)
            .filter(|directory| directory.is_absolute())
            .filter(|directory| !directory.as_os_str().is_empty()),
    );
    directories
}

fn discover_from_env() -> Vec<Thumbnailer> {
    discover(&data_directories())
}

/// Every thumbnailer under `directories`' `thumbnailers` subdirectory, in
/// order, shadows included once.
#[must_use]
pub fn discover(directories: &[PathBuf]) -> Vec<Thumbnailer> {
    let mut found = Vec::new();
    let mut seen = BTreeSet::new();
    for directory in directories {
        let thumbnailer_dir = directory.join("thumbnailers");
        let Ok(entries) = std::fs::read_dir(&thumbnailer_dir) else {
            continue;
        };
        let mut names: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "thumbnailer")
            })
            .collect();
        names.sort();
        for path in names {
            let Some(name) = path.file_name().map(PathBuf::from) else {
                continue;
            };
            if !seen.insert(name) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                log::debug!("cannot read the thumbnailer {}", path.display());
                continue;
            };
            match parse(&text) {
                Some((exec, mime_types)) => found.push(Thumbnailer {
                    exec,
                    mime_types,
                    path,
                }),
                None => log::debug!("ignoring the malformed thumbnailer {}", path.display()),
            }
        }
    }
    found
}

/// The `Exec=` and `MimeType=` of a thumbnailer file's
/// `[Thumbnailer Entry]` group. `None` when either is missing: a thumbnailer
/// without a command or without a claim can never run.
#[must_use]
pub fn parse(text: &str) -> Option<(String, Vec<String>)> {
    let mut exec = None;
    let mut mime_types = None;
    let mut in_entry = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_entry = line == "[Thumbnailer Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Exec" if !value.is_empty() => exec = Some(value.to_owned()),
            "MimeType" if !value.is_empty() => {
                mime_types = Some(
                    value
                        .split(';')
                        .map(str::trim)
                        .filter(|mime| !mime.is_empty())
                        .map(str::to_owned)
                        .collect::<Vec<_>>(),
                );
            }
            _ => {}
        }
    }
    Some((exec?, mime_types?))
}

/// The argument list of `exec` for `source`, with `%i` (the path), `%u` (its
/// URI), `%o` (the output file) and `%s` (the asked size) substituted, `%%`
/// becoming a percent sign. The path reaches the command as one argument of
/// its own: no shell, and no quoting to get wrong.
#[must_use]
pub fn arguments(exec: &str, source: &Path, output: &Path, size: u32) -> Option<Vec<String>> {
    let path = source.to_string_lossy();
    let uri = thumbnail_uri(source);
    let output = output.to_string_lossy();
    let size = size.to_string();
    let tokens = split_exec(exec).ok()?;
    let mut arguments = Vec::with_capacity(tokens.len());
    for token in tokens {
        let substituted = substitute(&token, &path, &uri, &output, &size);
        arguments.push(substituted);
    }
    arguments.first()?;
    Some(arguments)
}

/// One `Exec=` argument with its field codes expanded; an argument left empty
/// by a dropped code disappears, as the Desktop Entry spec says.
fn substitute(token: &str, path: &str, uri: &str, output: &str, size: &str) -> String {
    let mut expanded = String::with_capacity(token.len());
    let mut chars = token.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            expanded.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => expanded.push('%'),
            Some('i') => expanded.push_str(path),
            Some('u') => expanded.push_str(uri),
            Some('o') => expanded.push_str(output),
            Some('s') => expanded.push_str(size),
            Some(other) => {
                // Any other code is not a thumbnailer field code.
                expanded.push('%');
                expanded.push(other);
            }
            None => expanded.push('%'),
        }
    }
    expanded
}

/// Runs `thumbnailer` on `source`, asking for `size` pixels, and returns the
/// image it made. `None` when the command cannot start, fails, times out or
/// writes nothing this program can decode.
#[must_use]
pub fn run(thumbnailer: &Thumbnailer, source: &Path, size: u32) -> Option<image::RgbaImage> {
    let output = temporary_output()?;
    let result = run_into(thumbnailer, source, &output, size);
    if let Err(error) = std::fs::remove_file(&output) {
        if error.kind() != std::io::ErrorKind::NotFound {
            log::debug!("cannot remove the thumbnailer output: {error}");
        }
    }
    result
}

fn run_into(
    thumbnailer: &Thumbnailer,
    source: &Path,
    output: &Path,
    size: u32,
) -> Option<image::RgbaImage> {
    let arguments = arguments(&thumbnailer.exec, source, output, size)?;
    let (program, rest) = arguments.split_first()?;
    let mut child = Command::new(program)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            log::debug!(
                "cannot run the thumbnailer {}: {error}",
                thumbnailer.path.display()
            );
        })
        .ok()?;
    let deadline = Instant::now() + THUMBNAILER_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    log::debug!(
                        "the thumbnailer {} failed on {}: {status}",
                        thumbnailer.path.display(),
                        source.display()
                    );
                    return None;
                }
                break;
            }
            Ok(None) if Instant::now() >= deadline => {
                log::info!(
                    "the thumbnailer {} took too long on {}: killed",
                    thumbnailer.path.display(),
                    source.display()
                );
                drop(child.kill());
                drop(child.wait());
                return None;
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(error) => {
                log::debug!(
                    "cannot wait for the thumbnailer {}: {error}",
                    thumbnailer.path.display()
                );
                drop(child.kill());
                return None;
            }
        }
    }
    let made = image::ImageReader::open(output)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .map_err(|error| {
            log::debug!(
                "the thumbnailer {} wrote no image for {}: {error}",
                thumbnailer.path.display(),
                source.display()
            );
        })
        .ok()?;
    let (width, height) = (made.width(), made.height());
    if width == 0 || height == 0 {
        return None;
    }
    let made = if width > size || height > size {
        made.thumbnail(size, size)
    } else {
        made
    };
    Some(made.into_rgba8())
}

/// A private path in the session's temporary directory for one run's output.
fn temporary_output() -> Option<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let directory = std::env::temp_dir();
    Some(directory.join(format!(
        "fileflux-thumbnailer-{}-{}.png",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIDEO: &str = "[Thumbnailer Entry]
TryExec=ffmpegthumbnailer
Exec=/usr/bin/ffmpegthumbnailer -i %i -o %o -s %s -f
MimeType=video/mp4;video/x-matroska;
";

    #[test]
    fn a_thumbnailer_names_its_command_and_its_types() {
        let (exec, mimes) = parse(VIDEO).expect("a valid entry");
        assert_eq!(exec, "/usr/bin/ffmpegthumbnailer -i %i -o %o -s %s -f");
        assert_eq!(mimes, ["video/mp4", "video/x-matroska"]);
        // The wrong group, or a missing key, is not a thumbnailer.
        assert_eq!(parse("[Desktop Entry]\nExec=x\nMimeType=a\n"), None);
        assert_eq!(parse("[Thumbnailer Entry]\nMimeType=video/mp4;\n"), None);
        assert_eq!(parse("[Thumbnailer Entry]\nExec=x\n"), None);
    }

    #[test]
    fn field_codes_become_arguments_without_a_shell() {
        let source = Path::new("/home/u/My Holiday.mp4");
        let output = Path::new("/tmp/out.png");
        let (exec, _) = parse(VIDEO).expect("a valid entry");
        let arguments = arguments(&exec, source, output, 256).expect("an argument list");
        assert_eq!(
            arguments,
            [
                "/usr/bin/ffmpegthumbnailer",
                "-i",
                "/home/u/My Holiday.mp4",
                "-o",
                "/tmp/out.png",
                "-s",
                "256",
                "-f"
            ],
            "the path stays one argument, spaces and all"
        );
    }

    #[test]
    fn a_uri_code_gets_the_thumbnailers_own_escaping() {
        let arguments = arguments(
            "tool %u %o -s %s",
            Path::new("/home/u/a b.png"),
            Path::new("/tmp/o.png"),
            128,
        )
        .expect("an argument list");
        assert_eq!(arguments[1], "file:///home/u/a%20b.png");
        assert_eq!(arguments[2], "/tmp/o.png");
        assert_eq!(arguments[4], "128");
        assert_eq!(
            super::arguments("tool %%", Path::new("/a"), Path::new("/b"), 1)
                .expect("an argument list")[1],
            "%"
        );
    }

    #[test]
    fn user_directories_come_before_system_ones() {
        let scratch = tempdir("order");
        let user = scratch.join("user/thumbnailers");
        let system = scratch.join("system/thumbnailers");
        std::fs::create_dir_all(&user).expect("user directory");
        std::fs::create_dir_all(&system).expect("system directory");
        std::fs::write(user.join("same.thumbnailer"), VIDEO).expect("user file");
        std::fs::write(
            system.join("same.thumbnailer"),
            VIDEO.replace("ffmpegthumbnailer", "shadowed"),
        )
        .expect("system file");
        std::fs::write(
            system.join("other.thumbnailer"),
            VIDEO.replace("video/mp4", "application/pdf"),
        )
        .expect("other file");
        let found = discover(&[scratch.join("user"), scratch.join("system")]);
        assert_eq!(found.len(), 2, "the shadowed file is used once");
        assert!(found[0].exec.contains("ffmpegthumbnailer"));
        assert_eq!(found[1].mime_types, ["application/pdf", "video/x-matroska"]);
        drop(std::fs::remove_dir_all(&scratch));
    }

    #[test]
    fn a_missing_tool_leaves_no_thumbnail_and_no_leftovers() {
        let thumbnailer = Thumbnailer {
            exec: "/nonexistent/thumbnailer %i %o".to_owned(),
            mime_types: vec!["video/mp4".to_owned()],
            path: PathBuf::from("test.thumbnailer"),
        };
        let scratch = tempdir("missing");
        let source = scratch.join("a.mp4");
        std::fs::write(&source, b"not a video").expect("fixture");
        assert!(run(&thumbnailer, &source, 256).is_none());
        let leftovers = std::fs::read_dir(std::env::temp_dir())
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with("fileflux-thumbnailer-")
                    })
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(leftovers, 0, "the temporary output is removed");
        drop(std::fs::remove_dir_all(&scratch));
    }

    fn tempdir(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "fileflux-thumbnailers-test-{tag}-{}",
            std::process::id()
        ));
        drop(std::fs::remove_dir_all(&path));
        std::fs::create_dir_all(&path).expect("scratch directory");
        path
    }
}
