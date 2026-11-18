//! "Open with…": finds the installed applications that handle a file's MIME
//! type from the freedesktop `.desktop` files, and turns an `Exec=` line
//! into the argument list to run (no shell involved anywhere).

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::trash::encode_percent;

/// The keys of a `[Desktop Entry]` group Fileflux uses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DesktopEntry {
    pub name: String,
    pub exec: String,
    pub mime_types: Vec<String>,
    pub no_display: bool,
    pub hidden: bool,
    pub terminal: bool,
}

/// An application that can open a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct App {
    /// The desktop file id (`org.gnome.gedit.desktop`).
    pub id: String,
    pub name: String,
    pub exec: String,
}

/// Why an `Exec=` line cannot be run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecError {
    /// A double quote is never closed.
    UnclosedQuote,
    /// Nothing to run.
    Empty,
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ExecError::UnclosedQuote => "the Exec line has an unclosed quote",
            ExecError::Empty => "the Exec line is empty",
        })
    }
}

impl std::error::Error for ExecError {}

/// Applies the Desktop Entry string escapes (`\s \n \t \r \\`).
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Reads the `[Desktop Entry]` group of a `.desktop` file. `None` when the
/// group, its `Name` or its `Exec` is missing, or when it is not an
/// application (`Type=Link`, `Type=Directory`).
pub fn parse_desktop_entry(content: &str) -> Option<DesktopEntry> {
    let mut in_main = false;
    let mut saw_main = false;
    let mut entry = DesktopEntry::default();
    let mut has_name = false;
    let mut has_exec = false;
    let mut is_application = true;
    for line in content.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            saw_main |= in_main;
            continue;
        }
        if !in_main {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Type" => is_application = value == "Application",
            "Name" => {
                entry.name = unescape(value);
                has_name = true;
            }
            "Exec" => {
                entry.exec = unescape(value);
                has_exec = true;
            }
            "MimeType" => {
                entry.mime_types = value
                    .split(';')
                    .map(str::trim)
                    .filter(|mime| !mime.is_empty())
                    .map(str::to_owned)
                    .collect();
            }
            "NoDisplay" => entry.no_display = value == "true",
            "Hidden" => entry.hidden = value == "true",
            "Terminal" => entry.terminal = value == "true",
            _ => {}
        }
    }
    (saw_main && has_name && has_exec && is_application).then_some(entry)
}

/// Splits an `Exec=` value into arguments by the Desktop Entry quoting
/// rules: arguments are separated by spaces, a double-quoted argument may
/// contain spaces, and inside quotes a backslash escapes `"`, `` ` ``, `$`
/// and `\`. Nothing is expanded and no shell is involved.
pub fn split_exec(exec: &str) -> Result<Vec<String>, ExecError> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' => quoted = false,
                '\\' => match chars.next() {
                    Some(next @ ('"' | '`' | '$' | '\\')) => current.push(next),
                    Some(other) => {
                        current.push('\\');
                        current.push(other);
                    }
                    None => current.push('\\'),
                },
                other => current.push(other),
            }
        } else if c == '"' {
            quoted = true;
            started = true;
        } else if c.is_whitespace() {
            if started {
                args.push(std::mem::take(&mut current));
                started = false;
            }
        } else {
            current.push(c);
            started = true;
        }
    }
    if quoted {
        return Err(ExecError::UnclosedQuote);
    }
    if started {
        args.push(current);
    }
    if args.is_empty() {
        return Err(ExecError::Empty);
    }
    Ok(args)
}

/// The argument list to run `exec` for `path`: `%f` and `%F` become the
/// path, `%u` and `%U` its `file://` URI, `%%` a percent sign; the other
/// codes (`%i`, `%c`, `%k` and the deprecated ones) are dropped, and an
/// argument that held only dropped codes disappears. The path keeps its
/// exact bytes.
pub fn expand_exec(exec: &str, path: &Path) -> Result<Vec<OsString>, ExecError> {
    let uri = format!("file://{}", encode_percent(path.as_os_str().as_bytes()));
    let mut args = Vec::new();
    for token in split_exec(exec)? {
        let mut out: Vec<u8> = Vec::new();
        let mut had_code = false;
        let mut chars = token.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                let mut buffer = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
                continue;
            }
            had_code = true;
            match chars.next() {
                Some('f' | 'F') => out.extend_from_slice(path.as_os_str().as_bytes()),
                Some('u' | 'U') => out.extend_from_slice(uri.as_bytes()),
                Some('%') => out.push(b'%'),
                _ => {}
            }
        }
        if out.is_empty() && had_code {
            continue;
        }
        args.push(OsString::from_vec(out));
    }
    if args.first().is_none_or(|program| program.is_empty()) {
        return Err(ExecError::Empty);
    }
    Ok(args)
}

/// Whether `exec` passes the file to the program (`%f`, `%F`, `%u`, `%U`);
/// an application that does not cannot open anything.
pub fn takes_file(exec: &str) -> bool {
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        if c == '%' && matches!(chars.next(), Some('f' | 'F' | 'u' | 'U')) {
            return true;
        }
    }
    false
}

/// Whether an application listing `types` handles `mime`. Types compare
/// case-insensitively; every `text/*` type counts as `text/plain`.
pub fn mime_matches(types: &[String], mime: &str) -> bool {
    let mime = mime.to_ascii_lowercase();
    types.iter().any(|candidate| {
        let candidate = candidate.to_ascii_lowercase();
        candidate == mime || (candidate == "text/plain" && mime.starts_with("text/"))
    })
}

/// A MIME type from the extension alone, for when `xdg-mime` is missing.
pub fn guess_mime(name: &OsStr) -> Option<&'static str> {
    let bytes = name.as_bytes();
    let dot = bytes.iter().rposition(|&byte| byte == b'.')?;
    let extension = String::from_utf8_lossy(bytes.get(dot + 1..)?).to_ascii_lowercase();
    Some(match extension.as_str() {
        "txt" | "text" | "log" | "conf" | "ini" | "cfg" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "csv" => "text/csv",
        "xml" => "application/xml",
        "json" => "application/json",
        "rs" | "c" | "h" | "py" | "sh" | "js" | "ts" | "toml" | "yaml" | "yml" => "text/plain",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "flac" => "audio/flac",
        "wav" => "audio/x-wav",
        "mp4" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        _ => return None,
    })
}

/// The `applications` directories to search, most specific first:
/// `$XDG_DATA_HOME` (else `$HOME/.local/share`), then each entry of
/// `$XDG_DATA_DIRS` (else `/usr/local/share:/usr/share`). Relative, empty
/// and repeated entries are dropped.
pub fn application_dirs(
    home: Option<&OsStr>,
    data_home: Option<&OsStr>,
    data_dirs: Option<&OsStr>,
) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let data_home = data_home
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.filter(|home| !home.is_empty())
                .map(|home| Path::new(home).join(".local/share"))
        });
    roots.extend(data_home);
    let data_dirs = data_dirs
        .filter(|value| !value.is_empty())
        .unwrap_or(OsStr::new("/usr/local/share:/usr/share"));
    roots.extend(std::env::split_paths(data_dirs).filter(|path| path.is_absolute()));
    let mut dirs: Vec<PathBuf> = Vec::new();
    for root in roots {
        let dir = root.join("applications");
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// How deep below `applications` desktop files are looked for.
const MAX_DEPTH: usize = 4;

/// Collects `(id, path)` of the `.desktop` files under `root`; the id is
/// the path below `root` with `/` turned into `-`.
fn desktop_files(root: &Path, dir: &Path, depth: usize, found: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Links to files are common (profiles link into a store); links
        // to folders are not followed.
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if depth < MAX_DEPTH {
                desktop_files(root, &path, depth + 1, found);
            }
        } else if path.extension() == Some(OsStr::new("desktop"))
            && let Ok(relative) = path.strip_prefix(root)
        {
            let id = relative.to_string_lossy().replace('/', "-");
            found.push((id, path));
        }
    }
}

/// The applications under `dirs` that can open `mime`. A file in an earlier
/// directory hides one with the same id in a later one (even when it is
/// itself hidden). Entries that are `NoDisplay`, `Hidden`, run in a
/// terminal, or take no file argument are left out. `default_id` (from
/// `xdg-mime query default`) is listed first, the rest by name.
pub fn find_apps(dirs: &[PathBuf], mime: &str, default_id: Option<&str>) -> Vec<App> {
    let mut seen: Vec<String> = Vec::new();
    let mut apps: Vec<App> = Vec::new();
    for root in dirs {
        let mut files = Vec::new();
        desktop_files(root, root, 0, &mut files);
        for (id, path) in files {
            if seen.contains(&id) {
                continue;
            }
            seen.push(id.clone());
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Some(entry) = parse_desktop_entry(&String::from_utf8_lossy(&bytes)) else {
                continue;
            };
            if entry.hidden
                || entry.no_display
                || entry.terminal
                || !takes_file(&entry.exec)
                || !mime_matches(&entry.mime_types, mime)
            {
                continue;
            }
            apps.push(App {
                id,
                name: entry.name,
                exec: entry.exec,
            });
        }
    }
    apps.sort_by(|a, b| {
        let a_default = default_id == Some(a.id.as_str());
        let b_default = default_id == Some(b.id.as_str());
        b_default
            .cmp(&a_default)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.id.cmp(&b.id))
    });
    apps
}

/// The MIME type of `path` by `xdg-mime query filetype` (no shell), else
/// by extension, else `application/octet-stream`.
pub fn query_mime(path: &Path) -> String {
    let asked = Command::new("xdg-mime")
        .args(["query", "filetype"])
        .arg(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|mime| mime.contains('/'));
    asked
        .or_else(|| path.file_name().and_then(guess_mime).map(str::to_owned))
        .unwrap_or_else(|| "application/octet-stream".to_owned())
}

/// The desktop file id of the default application for `mime`
/// (`xdg-mime query default`), when there is one.
pub fn query_default(mime: &str) -> Option<String> {
    let output = Command::new("xdg-mime")
        .args(["query", "default", mime])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!id.is_empty()).then_some(id)
}

/// What the "Open with…" dialog offers for one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenWith {
    pub mime: String,
    pub apps: Vec<App>,
}

/// The MIME type of `path` and the applications that handle it, from the
/// process environment. Blocking: it runs `xdg-mime` and reads `.desktop`
/// files, so call it off the UI thread.
pub fn open_with_candidates(path: &Path) -> OpenWith {
    let mime = query_mime(path);
    let dirs = application_dirs(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("XDG_DATA_DIRS").as_deref(),
    );
    let default = query_default(&mime);
    let apps = find_apps(&dirs, &mime, default.as_deref());
    OpenWith { mime, apps }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::fs;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::sync::atomic::{AtomicU64, Ordering};

    type TestResult = Result<(), Box<dyn Error>>;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> std::io::Result<Scratch> {
            let dir = std::env::temp_dir().join(format!(
                "fileflux-apps-{tag}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir)?;
            Ok(Scratch(dir))
        }

        fn write(&self, rel: &str, content: &str) -> std::io::Result<()> {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, content)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn argv(exec: &str, path: &str) -> Result<Vec<String>, ExecError> {
        Ok(expand_exec(exec, Path::new(path))?
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect())
    }

    fn entry(name: &str, mime: &str, extra: &str) -> String {
        format!(
            "[Desktop Entry]\nType=Application\nName={name}\nExec=run-{name} %f\nMimeType={mime}\n{extra}"
        )
    }

    #[test]
    fn a_desktop_entry_is_read_from_its_main_group_only() {
        let parsed = parse_desktop_entry(
            "# a comment\n[Desktop Entry]\nType=Application\nName=Editor\nName[es]=Editor ES\n\
             Exec=editor --new %U\nMimeType=text/plain;text/markdown;\nTerminal=false\n\
             \n[Desktop Action new]\nName=New window\nExec=editor --other\n",
        );
        assert_eq!(
            parsed,
            Some(DesktopEntry {
                name: "Editor".to_owned(),
                exec: "editor --new %U".to_owned(),
                mime_types: vec!["text/plain".to_owned(), "text/markdown".to_owned()],
                no_display: false,
                hidden: false,
                terminal: false,
            })
        );
    }

    #[test]
    fn display_flags_and_unusable_entries_are_recognized() {
        let flags = parse_desktop_entry(
            "[Desktop Entry]\nName=A\nExec=a %f\nNoDisplay=true\nHidden=true\nTerminal=true\n",
        );
        assert!(
            flags
                .as_ref()
                .is_some_and(|e| e.no_display && e.hidden && e.terminal)
        );
        assert_eq!(
            parse_desktop_entry("[Desktop Entry]\nName=A\n"),
            None,
            "no Exec"
        );
        assert_eq!(
            parse_desktop_entry("[Desktop Entry]\nExec=a\n"),
            None,
            "no Name"
        );
        assert_eq!(parse_desktop_entry("Name=A\nExec=a\n"), None, "no group");
        assert_eq!(
            parse_desktop_entry("[Desktop Entry]\nType=Link\nName=A\nExec=a\n"),
            None,
            "only applications"
        );
    }

    #[test]
    fn exec_is_split_by_the_desktop_entry_quoting_rules() -> TestResult {
        assert_eq!(
            split_exec("app  --flag   value")?,
            ["app", "--flag", "value"]
        );
        assert_eq!(
            split_exec(r#""/opt/My App/run" "a b" c"#)?,
            ["/opt/My App/run", "a b", "c"]
        );
        assert_eq!(
            split_exec(r#"app "say \"hi\" \$HOME \\ \`x\`""#)?,
            ["app", r#"say "hi" $HOME \ `x`"#]
        );
        assert_eq!(
            split_exec("app 'single quotes are plain'")?,
            ["app", "'single", "quotes", "are", "plain'"]
        );
        assert_eq!(
            split_exec(r#"app "never closed"#),
            Err(ExecError::UnclosedQuote)
        );
        assert_eq!(split_exec("   "), Err(ExecError::Empty));
        Ok(())
    }

    #[test]
    fn field_codes_expand_to_the_file_and_the_ignored_ones_vanish() -> TestResult {
        let path = "/home/u/My Docs/a b.txt";
        assert_eq!(argv("viewer %f", path)?, ["viewer", path]);
        assert_eq!(argv("viewer %F", path)?, ["viewer", path]);
        assert_eq!(
            argv("viewer %u", path)?,
            ["viewer", "file:///home/u/My%20Docs/a%20b.txt"]
        );
        assert_eq!(
            argv("viewer %U", path)?,
            ["viewer", "file:///home/u/My%20Docs/a%20b.txt"]
        );
        assert_eq!(
            argv("viewer %i %c %k --keep %f", path)?,
            ["viewer", "--keep", path],
            "icon, name and location codes are dropped"
        );
        assert_eq!(
            argv("viewer --file=%f", path)?,
            ["viewer", "--file=/home/u/My Docs/a b.txt"]
        );
        assert_eq!(argv("viewer 100%% %f", path)?, ["viewer", "100%", path]);
        assert_eq!(argv("viewer %d %D %n %N %v %m %f", path)?, ["viewer", path]);
        assert_eq!(
            argv(r#""/opt/My App/run" --x %f"#, path)?,
            ["/opt/My App/run", "--x", path]
        );
        Ok(())
    }

    #[test]
    fn a_path_that_is_not_utf8_reaches_the_program_byte_for_byte() -> TestResult {
        let raw = OsString::from_vec(b"/tmp/caf\xe9 \xff.txt".to_vec());
        let args = expand_exec("viewer %f", Path::new(&raw))?;
        assert_eq!(args.get(1).map(|arg| arg.as_bytes()), Some(raw.as_bytes()));
        let uri = expand_exec("viewer %u", Path::new(&raw))?;
        assert_eq!(
            uri.get(1).map(|arg| arg.to_string_lossy().into_owned()),
            Some("file:///tmp/caf%E9%20%FF.txt".to_owned())
        );
        Ok(())
    }

    #[test]
    fn only_file_codes_mean_the_app_takes_a_file() {
        assert!(takes_file("a %f"));
        assert!(takes_file("a %F"));
        assert!(takes_file("a --x=%u"));
        assert!(takes_file("a %U"));
        assert!(!takes_file("a %i %c %k"));
        assert!(!takes_file("a 100%% done"));
        assert!(!takes_file("a"));
    }

    #[test]
    fn mime_types_match_exactly_and_text_files_match_plain_text_apps() {
        let types = |list: &[&str]| list.iter().map(|t| (*t).to_owned()).collect::<Vec<_>>();
        assert!(mime_matches(
            &types(&["image/png", "image/jpeg"]),
            "image/png"
        ));
        assert!(mime_matches(&types(&["IMAGE/PNG"]), "image/png"));
        assert!(!mime_matches(&types(&["image/png"]), "image/jpeg"));
        assert!(!mime_matches(&types(&[]), "image/png"));
        assert!(
            mime_matches(&types(&["text/plain"]), "text/markdown"),
            "every text type is a kind of text/plain"
        );
        assert!(!mime_matches(&types(&["text/plain"]), "image/png"));
    }

    #[test]
    fn the_mime_type_is_guessed_from_the_extension_when_xdg_mime_is_missing() {
        assert_eq!(guess_mime(OsStr::new("notes.txt")), Some("text/plain"));
        assert_eq!(guess_mime(OsStr::new("PHOTO.PNG")), Some("image/png"));
        assert_eq!(guess_mime(OsStr::new("a.tar.gz")), Some("application/gzip"));
        assert_eq!(guess_mime(OsStr::new("Makefile")), None);
        assert_eq!(guess_mime(OsStr::new("weird.zzzz")), None);
    }

    #[test]
    fn application_dirs_follow_the_xdg_data_dirs_rules() {
        let os = OsStr::new;
        let dirs = |home, data_home, data_dirs| application_dirs(home, data_home, data_dirs);
        assert_eq!(
            dirs(Some(os("/h")), None, None),
            [
                PathBuf::from("/h/.local/share/applications"),
                PathBuf::from("/usr/local/share/applications"),
                PathBuf::from("/usr/share/applications"),
            ]
        );
        assert_eq!(
            dirs(
                Some(os("/h")),
                Some(os("/data")),
                Some(os("/a:/b::relative:/a"))
            ),
            [
                PathBuf::from("/data/applications"),
                PathBuf::from("/a/applications"),
                PathBuf::from("/b/applications"),
            ],
            "empty, relative and repeated entries are dropped"
        );
        assert_eq!(
            dirs(None, None, Some(os("/only"))),
            [PathBuf::from("/only/applications")]
        );
    }

    #[test]
    fn find_apps_lists_visible_handlers_with_the_default_first() -> TestResult {
        let first = Scratch::new("first")?;
        let second = Scratch::new("second")?;
        first.write("zed.desktop", &entry("Zed", "text/plain;", ""))?;
        first.write("sub/nested.desktop", &entry("Nested", "text/plain;", ""))?;
        first.write("other.desktop", &entry("Other", "image/png;", ""))?;
        first.write(
            "secret.desktop",
            &entry("Secret", "text/plain;", "NoDisplay=true\n"),
        )?;
        first.write(
            "gone.desktop",
            &entry("Gone", "text/plain;", "Hidden=true\n"),
        )?;
        first.write(
            "term.desktop",
            &entry("Term", "text/plain;", "Terminal=true\n"),
        )?;
        first.write(
            "nofile.desktop",
            "[Desktop Entry]\nName=NoFile\nExec=nofile\nMimeType=text/plain;\n",
        )?;
        first.write("notes.txt", "not a desktop file")?;
        first.write(
            "shadow.desktop",
            &entry("Shadow", "text/plain;", "Hidden=true\n"),
        )?;
        second.write("shadow.desktop", &entry("Shadow", "text/plain;", ""))?;
        second.write("alpha.desktop", &entry("Alpha", "text/plain;", ""))?;
        second.write("zed.desktop", &entry("Zed Old", "text/plain;", ""))?;

        let dirs = [first.0.clone(), second.0.clone()];
        let names = |default: Option<&str>| {
            find_apps(&dirs, "text/plain", default)
                .into_iter()
                .map(|app| app.name)
                .collect::<Vec<_>>()
        };

        assert_eq!(names(None), ["Alpha", "Nested", "Zed"]);
        assert_eq!(
            names(Some("zed.desktop")),
            ["Zed", "Alpha", "Nested"],
            "the default comes first"
        );
        assert_eq!(
            names(Some("sub-nested.desktop")),
            ["Nested", "Alpha", "Zed"]
        );
        assert!(find_apps(&dirs, "video/mp4", None).is_empty());
        let png = find_apps(&dirs, "image/png", None);
        assert_eq!(png.len(), 1);
        assert_eq!(
            png.first().map(|app| app.exec.as_str()),
            Some("run-Other %f")
        );
        assert_eq!(
            png.first().map(|app| app.id.as_str()),
            Some("other.desktop")
        );
        Ok(())
    }
}
