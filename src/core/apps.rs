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
    /// The `Categories` list (`TerminalEmulator`, `Utility`, ...).
    pub categories: Vec<String>,
    /// `X-TerminalArgDir`, the xdg-terminal-exec key naming the argument that
    /// sets a terminal's working directory (`--working-directory=`).
    pub terminal_arg_dir: Option<String>,
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
    /// A field code sits inside a quoted argument. The Desktop Entry spec
    /// forbids it, and expanding it would let a file name reach a shell
    /// (`sh -c "echo %f"`), so such entries are refused.
    QuotedFieldCode,
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ExecError::UnclosedQuote => "the Exec line has an unclosed quote",
            ExecError::Empty => "the Exec line is empty",
            ExecError::QuotedFieldCode => "the Exec line uses a field code inside quotes",
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

/// The most of a `.desktop` file that is read: a few lines, never large.
const DESKTOP_FILE_LIMIT: u64 = 1 << 20;

/// The text of the `.desktop` file at `path` (a link is followed, as opening
/// it would). Opened non-blocking and checked on the descriptor, so a FIFO
/// swapped in cannot stall it; blocking on a slow mount, so call it off the
/// UI thread.
pub fn read_desktop_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits().cast_signed())
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("it is not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(DESKTOP_FILE_LIMIT).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// What "Launch" runs for a Desktop Entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopLaunch {
    /// The program and its arguments (see [`expand_exec_bare`]).
    pub argv: Vec<OsString>,
    /// `Terminal=true`: the entry wants a terminal around it.
    pub terminal: bool,
}

/// The command "Launch" runs for the `.desktop` file `content`: its `Exec`
/// expanded with no files. The error is a plain sentence for a notice.
pub fn desktop_launch(content: &str) -> Result<DesktopLaunch, String> {
    let entry = parse_desktop_entry(content)
        .ok_or_else(|| "it is not a launcher of an application".to_owned())?;
    let argv = expand_exec_bare(&entry.exec).map_err(|error| error.to_string())?;
    Ok(DesktopLaunch {
        argv,
        terminal: entry.terminal,
    })
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
            "Categories" => {
                entry.categories = value
                    .split(';')
                    .map(str::trim)
                    .filter(|category| !category.is_empty())
                    .map(str::to_owned)
                    .collect();
            }
            "X-TerminalArgDir" => {
                entry.terminal_arg_dir = (!value.is_empty()).then(|| value.to_owned());
            }
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
    Ok(split_exec_tokens(exec)?
        .into_iter()
        .map(|token| token.text)
        .collect())
}

/// One `Exec=` argument and whether any part of it was double-quoted.
struct ExecToken {
    text: String,
    quoted: bool,
}

fn split_exec_tokens(exec: &str) -> Result<Vec<ExecToken>, ExecError> {
    let mut args = Vec::new();
    let mut was_quoted = false;
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
            was_quoted = true;
            started = true;
        } else if c.is_whitespace() {
            if started {
                args.push(ExecToken {
                    text: std::mem::take(&mut current),
                    quoted: std::mem::take(&mut was_quoted),
                });
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
        args.push(ExecToken {
            text: current,
            quoted: was_quoted,
        });
    }
    if args.is_empty() {
        return Err(ExecError::Empty);
    }
    Ok(args)
}

/// Whether `text` holds a field code other than the literal `%%`.
fn has_field_code(text: &str) -> bool {
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '%' && chars.next() != Some('%') {
            return true;
        }
    }
    false
}

/// The argument list to run `exec` for `path`: `%f` and `%F` become the
/// path, `%u` and `%U` its `file://` URI, `%%` a percent sign; the other
/// codes (`%i`, `%c`, `%k` and the deprecated ones) are dropped, and an
/// argument that held only dropped codes disappears. The path keeps its
/// exact bytes.
pub fn expand_exec(exec: &str, path: &Path) -> Result<Vec<OsString>, ExecError> {
    let mut invocations = expand_exec_many(exec, &[path])?;
    if invocations.is_empty() {
        return Err(ExecError::Empty);
    }
    Ok(invocations.swap_remove(0))
}

/// One argument of an `Exec=` line with its field codes found.
struct ExecArg {
    text: String,
    /// The argument holds `%F` or `%U`: it stands for every file.
    list: bool,
}

/// `token` for one file (`None`: no file, so its codes are dropped too): the
/// codes replaced as in [`expand_exec`]; `None` when the argument held only
/// dropped codes.
fn render_arg(token: &str, file: Option<(&Path, &[u8])>) -> Option<Vec<u8>> {
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
            Some('f' | 'F') => {
                if let Some((path, _)) = file {
                    out.extend_from_slice(path.as_os_str().as_bytes());
                }
            }
            Some('u' | 'U') => {
                if let Some((_, uri)) = file {
                    out.extend_from_slice(uri);
                }
            }
            Some('%') => out.push(b'%'),
            _ => {}
        }
    }
    (!(out.is_empty() && had_code)).then_some(out)
}

/// The arguments of `exec` with their field codes found; a field code inside
/// quotes is refused (see [`ExecError::QuotedFieldCode`]).
fn exec_args(exec: &str) -> Result<Vec<ExecArg>, ExecError> {
    let mut tokens = Vec::new();
    for ExecToken { text, quoted } in split_exec_tokens(exec)? {
        if quoted && has_field_code(&text) {
            return Err(ExecError::QuotedFieldCode);
        }
        tokens.push(ExecArg {
            list: has_list_code(&text),
            text,
        });
    }
    Ok(tokens)
}

/// The argument list to run `exec` with **no files**, as "Launch" does: the
/// file codes (`%f %F %u %U`) and the other codes are dropped, `%%` is a
/// percent sign, and an argument that held only dropped codes disappears. No
/// shell is involved.
pub fn expand_exec_bare(exec: &str) -> Result<Vec<OsString>, ExecError> {
    let args: Vec<OsString> = exec_args(exec)?
        .iter()
        .filter_map(|token| render_arg(&token.text, None).map(OsString::from_vec))
        .collect();
    if args.first().is_none_or(|program| program.is_empty()) {
        return Err(ExecError::Empty);
    }
    Ok(args)
}

/// The processes to start to run `exec` on `paths`, as argument lists (the
/// program first), by the Desktop Entry rules. An `Exec=` with `%F` or `%U`
/// takes every file in **one** process: such an argument becomes one
/// argument per file (`%U` as percent-encoded `file://` URIs). One with only
/// `%f` or `%u` gets **one process per file**, in the order of `paths`. The
/// other codes behave as for one file ([`expand_exec`]); a single code next
/// to a list code takes the first file. No shell is involved. No paths, no
/// processes.
pub fn expand_exec_many(exec: &str, paths: &[&Path]) -> Result<Vec<Vec<OsString>>, ExecError> {
    let tokens = exec_args(exec)?;
    let Some(first) = paths.first() else {
        return Ok(Vec::new());
    };
    let uri_of = |path: &Path| {
        format!("file://{}", encode_percent(path.as_os_str().as_bytes())).into_bytes()
    };
    let one_process = tokens.iter().any(|token| token.list);
    let single_files: &[&Path] = if one_process { &[first] } else { paths };
    let mut invocations = Vec::new();
    for path in single_files {
        let mut args: Vec<OsString> = Vec::new();
        for token in &tokens {
            if token.list {
                for each in paths {
                    args.extend(
                        render_arg(&token.text, Some((each, &uri_of(each))))
                            .map(OsString::from_vec),
                    );
                }
            } else {
                args.extend(
                    render_arg(&token.text, Some((path, &uri_of(path)))).map(OsString::from_vec),
                );
            }
        }
        if args.first().is_none_or(|program| program.is_empty()) {
            return Err(ExecError::Empty);
        }
        invocations.push(args);
    }
    Ok(invocations)
}

/// Whether `text` holds `%F` or `%U`.
fn has_list_code(text: &str) -> bool {
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '%' && matches!(chars.next(), Some('F' | 'U')) {
            return true;
        }
    }
    false
}

/// Whether `exec` passes the file to the program (`%f`, `%F`, `%u`, `%U`)
/// in a form it can be run with; an application that does not cannot open
/// anything, and one with a field code inside quotes is refused (see
/// [`ExecError::QuotedFieldCode`]).
pub fn takes_file(exec: &str) -> bool {
    let Ok(tokens) = split_exec_tokens(exec) else {
        return false;
    };
    if tokens
        .iter()
        .any(|token| token.quoted && has_field_code(&token.text))
    {
        return false;
    }
    tokens.iter().any(|token| {
        let mut chars = token.text.chars();
        while let Some(c) = chars.next() {
            if c == '%' && matches!(chars.next(), Some('f' | 'F' | 'u' | 'U')) {
                return true;
            }
        }
        false
    })
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

/// Every desktop entry under `dirs` with its id, in precedence order: a file
/// in an earlier directory hides one with the same id in a later one (even
/// when it is itself hidden or unreadable as an application, which then
/// leaves no entry at all). Nothing is filtered by `NoDisplay` or `Hidden`;
/// the caller decides.
pub fn desktop_entries(dirs: &[PathBuf]) -> Vec<(String, DesktopEntry)> {
    let mut seen: Vec<String> = Vec::new();
    let mut entries = Vec::new();
    for root in dirs {
        let mut files = Vec::new();
        desktop_files(root, root, 0, &mut files);
        files.sort();
        for (id, path) in files {
            if seen.contains(&id) {
                continue;
            }
            seen.push(id.clone());
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if let Some(entry) = parse_desktop_entry(&String::from_utf8_lossy(&bytes)) {
                entries.push((id, entry));
            }
        }
    }
    entries
}

/// An application that can open a file, with the MIME types it declares.
struct Candidate {
    app: App,
    mime_types: Vec<String>,
}

/// Every application under `dirs` that a file manager may offer: one that
/// is not `NoDisplay`, `Hidden` or run in a terminal, and whose `Exec=`
/// takes a file in a form that can be run (see [`takes_file`]). A file in
/// an earlier directory hides one with the same id in a later one (even
/// when it is itself hidden).
fn candidates(dirs: &[PathBuf]) -> Vec<Candidate> {
    let mut seen: Vec<String> = Vec::new();
    let mut found: Vec<Candidate> = Vec::new();
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
            if entry.hidden || entry.no_display || entry.terminal || !takes_file(&entry.exec) {
                continue;
            }
            found.push(Candidate {
                app: App {
                    id,
                    name: entry.name,
                    exec: entry.exec,
                },
                mime_types: entry.mime_types,
            });
        }
    }
    found
}

/// Name order, ignoring case; the id breaks ties.
fn by_name(a: &App, b: &App) -> std::cmp::Ordering {
    a.name
        .to_lowercase()
        .cmp(&b.name.to_lowercase())
        .then_with(|| a.id.cmp(&b.id))
}

fn sorted_apps(mut apps: Vec<App>, default_id: Option<&str>) -> Vec<App> {
    apps.sort_by(|a, b| {
        let a_default = default_id == Some(a.id.as_str());
        let b_default = default_id == Some(b.id.as_str());
        b_default.cmp(&a_default).then_with(|| by_name(a, b))
    });
    apps
}

fn matching_apps(found: &[Candidate], mime: &str, default_id: Option<&str>) -> Vec<App> {
    let apps = found
        .iter()
        .filter(|candidate| {
            // A default the user chose is offered for the type even when
            // the application does not declare it ("Other application…").
            default_id == Some(candidate.app.id.as_str())
                || mime_matches(&candidate.mime_types, mime)
        })
        .map(|candidate| candidate.app.clone())
        .collect();
    sorted_apps(apps, default_id)
}

/// The applications that handle **every** type of `mimes` (each by the rules
/// of [`mime_matches`]). One type behaves as [`matching_apps`], default
/// included; several types list no default.
fn matching_apps_all(found: &[Candidate], mimes: &[String], default_id: Option<&str>) -> Vec<App> {
    let apps = found
        .iter()
        .filter(|candidate| {
            default_id == Some(candidate.app.id.as_str())
                || mimes
                    .iter()
                    .all(|mime| mime_matches(&candidate.mime_types, mime))
        })
        .map(|candidate| candidate.app.clone())
        .collect();
    sorted_apps(apps, default_id)
}

fn all_apps(found: &[Candidate]) -> Vec<App> {
    sorted_apps(found.iter().map(|c| c.app.clone()).collect(), None)
}

/// The applications under `dirs` that can open `mime`. A file in an earlier
/// directory hides one with the same id in a later one (even when it is
/// itself hidden). Entries that are `NoDisplay`, `Hidden`, run in a
/// terminal, or take no file argument are left out. `default_id` (the
/// remembered or `xdg-mime query default` choice) is listed first, even
/// when it does not declare `mime`; the rest by name.
pub fn find_apps(dirs: &[PathBuf], mime: &str, default_id: Option<&str>) -> Vec<App> {
    matching_apps(&candidates(dirs), mime, default_id)
}

/// The applications under `dirs` that can open every type of `mimes` (the
/// intersection of their associations, see [`find_apps`] for the entries
/// left out). `default_id` is listed first.
pub fn find_apps_for_all(dirs: &[PathBuf], mimes: &[String], default_id: Option<&str>) -> Vec<App> {
    matching_apps_all(&candidates(dirs), mimes, default_id)
}

/// Every application under `dirs` that can open a file, whatever types it
/// declares ("Other application…"), by name. The same entries are left out
/// as in [`find_apps`].
pub fn find_all_apps(dirs: &[PathBuf]) -> Vec<App> {
    all_apps(&candidates(dirs))
}

/// The applications whose name contains `query`, ignoring case and
/// surrounding blanks, in the order given. An empty query keeps them all.
pub fn filter_apps(apps: &[App], query: &str) -> Vec<App> {
    let query = query.trim().to_lowercase();
    apps.iter()
        .filter(|app| app.name.to_lowercase().contains(&query))
        .cloned()
        .collect()
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
    /// The type of the file; of several files, their common type, or empty
    /// when they differ (`apps` then holds the applications that handle
    /// every one of the types).
    pub mime: String,
    /// The applications that handle `mime`, the default first.
    pub apps: Vec<App>,
    /// Every application that takes a file, by name ("Other application…").
    pub all_apps: Vec<App>,
}

/// `$XDG_CONFIG_HOME/mimeapps.list` (else `$HOME/.config/mimeapps.list`);
/// a relative `XDG_CONFIG_HOME` is ignored, as the spec says.
pub fn mimeapps_path(home: Option<&OsStr>, config_home: Option<&OsStr>) -> Option<PathBuf> {
    let base = config_home
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.filter(|home| !home.is_empty())
                .map(|home| Path::new(home).join(".config"))
        })?;
    Some(base.join("mimeapps.list"))
}

fn mimeapps_path_from_env() -> Option<PathBuf> {
    mimeapps_path(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
    )
}

const DEFAULT_GROUP: &str = "[Default Applications]";

/// The line without its `\n` or `\r\n` ending.
fn line_body(line: &str) -> &str {
    line.strip_suffix('\n')
        .map_or(line, |rest| rest.strip_suffix('\r').unwrap_or(rest))
}

/// The first desktop id `file` lists for `mime` in its
/// `[Default Applications]` group.
pub fn default_application(file: &Path, mime: &str) -> Option<String> {
    let content = std::fs::read(file).ok()?;
    let content = String::from_utf8_lossy(&content);
    let mut in_defaults = false;
    for line in content.split_inclusive('\n').map(line_body) {
        let line = line.trim();
        if line.starts_with('[') {
            in_defaults = line == DEFAULT_GROUP;
        } else if in_defaults
            && let Some((key, value)) = line.split_once('=')
            && key.trim() == mime
        {
            return value
                .split(';')
                .map(str::trim)
                .find(|id| !id.is_empty())
                .map(str::to_owned);
        }
    }
    None
}

/// `content` with `mime=id;` as the default for `mime`: the first line of
/// the `[Default Applications]` group for that key is replaced, else the
/// line is added at the end of the group, else the group is appended. Every
/// other byte stays, and new lines use the file's own line ending.
fn with_default(content: &str, mime: &str, id: &str) -> String {
    let eol = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let new_line = format!("{mime}={id};");
    let lines: Vec<&str> = content.split_inclusive('\n').collect();

    let mut in_defaults = false;
    let mut group_seen = false;
    // Index just after the last key line of the group.
    let mut insert_at = 0;
    let mut replace: Option<usize> = None;
    for (index, raw) in lines.iter().enumerate() {
        let line = line_body(raw).trim();
        if line.starts_with('[') {
            in_defaults = line == DEFAULT_GROUP;
            if in_defaults {
                group_seen = true;
                insert_at = index + 1;
            }
        } else if in_defaults && !line.is_empty() && !line.starts_with('#') {
            insert_at = index + 1;
            if replace.is_none()
                && line
                    .split_once('=')
                    .is_some_and(|(key, _)| key.trim() == mime)
            {
                replace = Some(index);
            }
        }
    }

    let mut out = String::with_capacity(content.len() + new_line.len() + 2 * eol.len());
    if group_seen {
        for (index, raw) in lines.iter().enumerate() {
            if replace == Some(index) {
                let ending = &raw[line_body(raw).len()..];
                out.push_str(&new_line);
                out.push_str(ending);
            } else {
                out.push_str(raw);
            }
            if replace.is_none() && index + 1 == insert_at {
                if !raw.ends_with('\n') {
                    out.push_str(eol);
                }
                out.push_str(&new_line);
                out.push_str(eol);
            }
        }
    } else {
        out.push_str(content);
        if !content.is_empty() {
            if !content.ends_with('\n') {
                out.push_str(eol);
            }
            if lines
                .last()
                .is_some_and(|last| !line_body(last).trim().is_empty())
            {
                out.push_str(eol);
            }
        }
        out.push_str(DEFAULT_GROUP);
        out.push_str(eol);
        out.push_str(&new_line);
        out.push_str(eol);
    }
    out
}

/// Makes `id` the default application for `mime` in the
/// `[Default Applications]` group of the `mimeapps.list` at `file`,
/// creating the file, its folder and the group when missing. The new
/// content goes to a temporary file in the same folder that then replaces
/// `file`, so a reader never sees a half-written list.
pub fn set_default_application(file: &Path, mime: &str, id: &str) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};

    let valid = |text: &str, forbidden: &[char]| {
        !text.is_empty()
            && !text
                .chars()
                .any(|c| c.is_control() || forbidden.contains(&c))
    };
    if !valid(mime, &['=', ';', '[', ']']) || !valid(id, &['=', ';']) {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "the MIME type or the desktop id cannot be written to mimeapps.list",
        ));
    }
    let current = match std::fs::read(file) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let text = current.as_deref().map(String::from_utf8_lossy);
    let updated = with_default(text.as_deref().unwrap_or(""), mime, id);
    // Lossy decoding must not rewrite bytes the user had: a list that is
    // not UTF-8 is left alone.
    if text
        .as_ref()
        .is_some_and(|text| matches!(text, std::borrow::Cow::Owned(_)))
    {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "mimeapps.list is not valid UTF-8",
        ));
    }
    // The shared atomic writer: a unique temporary name created with
    // O_EXCL and O_NOFOLLOW, fsync and rename, written through a symlinked
    // file (home-manager) instead of replacing the link. An existing file
    // keeps its mode; a new one is 0644 like the desktop tools write it.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(file).map_or(0o644, |meta| meta.permissions().mode() & 0o7777);
    crate::core::settings::write_atomic_with_mode(file, updated.as_bytes(), mode)
}

/// Remembers `id` as the default for `mime` in the user's `mimeapps.list`
/// (see [`set_default_application`]).
pub fn remember_default(mime: &str, id: &str) -> std::io::Result<()> {
    let file = mimeapps_path_from_env().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no HOME or XDG_CONFIG_HOME")
    })?;
    set_default_application(&file, mime, id)
}

/// The MIME type of `path` and the applications that handle it, from the
/// process environment. Blocking: it runs `xdg-mime` and reads `.desktop`
/// files, so call it off the UI thread. The default is the one remembered in
/// the user's `mimeapps.list`, else the one `xdg-mime` reports.
pub fn open_with_candidates(path: &Path) -> OpenWith {
    open_with_for_type(&query_mime(path))
}

/// Distinct MIME types of `mimes`, ignoring case, in first-seen order.
fn distinct_mimes(mimes: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut distinct: Vec<String> = Vec::new();
    for mime in mimes {
        if !distinct.iter().any(|seen| seen.eq_ignore_ascii_case(&mime)) {
            distinct.push(mime);
        }
    }
    distinct
}

/// What the "Open with…" dialog offers for several files: the applications
/// that handle every selected type. Blocking, like [`open_with_candidates`].
/// With one shared type the answer is that of one file of the type.
pub fn open_with_candidates_many(paths: &[PathBuf]) -> OpenWith {
    let mimes = distinct_mimes(paths.iter().map(|path| query_mime(path)));
    if let [mime] = mimes.as_slice() {
        return open_with_for_type(mime);
    }
    let dirs = application_dirs_from_env();
    let found = candidates(&dirs);
    OpenWith {
        apps: matching_apps_all(&found, &mimes, None),
        all_apps: all_apps(&found),
        mime: String::new(),
    }
}

fn application_dirs_from_env() -> Vec<PathBuf> {
    application_dirs(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("XDG_DATA_DIRS").as_deref(),
    )
}

fn default_id_for(mime: &str) -> Option<String> {
    mimeapps_path_from_env()
        .and_then(|file| default_application(&file, mime))
        .or_else(|| query_default(mime))
}

fn open_with_for_type(mime: &str) -> OpenWith {
    let found = candidates(&application_dirs_from_env());
    let default = default_id_for(mime);
    OpenWith {
        apps: matching_apps(&found, mime, default.as_deref()),
        all_apps: all_apps(&found),
        mime: mime.to_owned(),
    }
}

/// The default application for `text/plain` among the ones installed under
/// `dirs`, as `default_of` names it (see [`default_apps`]). `None` when none
/// is set or the one set is not installed or cannot take a file.
pub fn default_text_app_in(
    dirs: &[PathBuf],
    default_of: &dyn Fn(&str) -> Option<String>,
) -> Option<App> {
    default_apps(
        dirs,
        &[(PathBuf::new(), "text/plain".to_owned())],
        default_of,
    )
    .into_iter()
    .find_map(|(_, app)| app)
}

/// The default application for `text/plain`, from the process environment
/// (the user's `mimeapps.list`, else `xdg-mime query default`). Blocking:
/// off the UI thread.
pub fn default_text_app() -> Option<App> {
    default_text_app_in(&application_dirs_from_env(), &|mime| default_id_for(mime))
}

/// Files with the application that opens them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppGroup {
    /// `None`: no application of ours can open them (the system opener does).
    pub app: Option<App>,
    pub paths: Vec<PathBuf>,
}

/// Groups files by application (by id), the groups in the order their
/// first file appears and each group's files in the given order. Files
/// without an application form the last-seen `None` group.
pub fn group_by_app(files: Vec<(PathBuf, Option<App>)>) -> Vec<AppGroup> {
    let mut groups: Vec<AppGroup> = Vec::new();
    for (path, app) in files {
        let existing = groups
            .iter_mut()
            .find(|group| group.app.as_ref().map(|a| &a.id) == app.as_ref().map(|a| &a.id));
        match existing {
            Some(group) => group.paths.push(path),
            None => groups.push(AppGroup {
                app,
                paths: vec![path],
            }),
        }
    }
    groups
}

/// The default application of each file: the application `default_of`
/// names for the file's type, when it is installed under `dirs` and can be
/// offered (see [`find_apps`]).
pub fn default_apps(
    dirs: &[PathBuf],
    files: &[(PathBuf, String)],
    default_of: &dyn Fn(&str) -> Option<String>,
) -> Vec<(PathBuf, Option<App>)> {
    let found = candidates(dirs);
    files
        .iter()
        .map(|(path, mime)| {
            let app = default_of(mime).and_then(|id| {
                found
                    .iter()
                    .find(|candidate| candidate.app.id == id)
                    .map(|candidate| candidate.app.clone())
            });
            (path.clone(), app)
        })
        .collect()
}

/// `paths` grouped by their default application, from the process
/// environment. Blocking (`xdg-mime`, `.desktop` files): off the UI thread.
pub fn default_app_groups(paths: &[PathBuf]) -> Vec<AppGroup> {
    let mut by_mime: Vec<(String, Option<String>)> = Vec::new();
    let files: Vec<(PathBuf, String)> = paths
        .iter()
        .map(|path| (path.clone(), query_mime(path)))
        .collect();
    for (_, mime) in &files {
        if !by_mime.iter().any(|(seen, _)| seen == mime) {
            by_mime.push((mime.clone(), default_id_for(mime)));
        }
    }
    let default_of = |mime: &str| -> Option<String> {
        by_mime
            .iter()
            .find(|(seen, _)| seen == mime)
            .and_then(|(_, id)| id.clone())
    };
    group_by_app(default_apps(
        &application_dirs_from_env(),
        &files,
        &default_of,
    ))
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
                categories: Vec::new(),
                terminal_arg_dir: None,
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
        // A file code only inside a quoted argument is not usable.
        assert!(!takes_file(r#"sh -c "echo \"%f\"""#));
    }

    #[test]
    fn a_field_code_inside_a_quoted_argument_is_refused() {
        let path = Path::new("/tmp/$(touch pwned);.txt");
        assert_eq!(
            expand_exec(r#"sh -c "echo \"%f\"""#, path),
            Err(ExecError::QuotedFieldCode)
        );
        assert_eq!(
            expand_exec(r#"app "--name=%u""#, path),
            Err(ExecError::QuotedFieldCode)
        );
        // A quoted argument without codes stays fine, and %% is literal.
        assert!(expand_exec(r#"app "two words" "100%%" %f"#, path).is_ok());
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

    #[test]
    fn desktop_entries_keeps_the_earlier_directory_even_when_its_file_is_unusable() -> TestResult {
        let scratch = Scratch::new("entries-precedence")?;
        // Earlier directory: `broken` has no Exec (unparsable as an
        // application), `good` is fine. Later directory: both have valid
        // entries under the same ids, plus one id only it has.
        scratch.write(
            "first/applications/broken.desktop",
            "[Desktop Entry]\nName=Broken\n",
        )?;
        scratch.write(
            "first/applications/good.desktop",
            "[Desktop Entry]\nName=First Good\nExec=first-good\n",
        )?;
        scratch.write(
            "second/applications/broken.desktop",
            "[Desktop Entry]\nName=Second Broken\nExec=second-broken\n",
        )?;
        scratch.write(
            "second/applications/good.desktop",
            "[Desktop Entry]\nName=Second Good\nExec=second-good\n",
        )?;
        scratch.write(
            "second/applications/only.desktop",
            "[Desktop Entry]\nName=Only\nExec=only\n",
        )?;
        let dirs = [
            scratch.0.join("first/applications"),
            scratch.0.join("second/applications"),
        ];

        let entries: Vec<(String, String)> = desktop_entries(&dirs)
            .into_iter()
            .map(|(id, entry)| (id, entry.exec))
            .collect();
        assert_eq!(
            entries,
            [
                ("good.desktop".to_owned(), "first-good".to_owned()),
                ("only.desktop".to_owned(), "only".to_owned()),
            ],
            "the unparsable earlier `broken` still hides the later one"
        );
        Ok(())
    }

    #[test]
    fn find_all_apps_lists_every_visible_file_app_by_name_whatever_its_mime() -> TestResult {
        let first = Scratch::new("all-first")?;
        let second = Scratch::new("all-second")?;
        first.write("zed.desktop", &entry("Zed", "text/plain;", ""))?;
        first.write("img.desktop", &entry("Gimp", "image/png;", ""))?;
        first.write("nomime.desktop", &entry("beta", "", ""))?;
        first.write("sub/nested.desktop", &entry("Nested", "video/mp4;", ""))?;
        first.write("a.desktop", &entry("Secret", "", "NoDisplay=true\n"))?;
        first.write("b.desktop", &entry("Gone", "", "Hidden=true\n"))?;
        first.write("c.desktop", &entry("Term", "", "Terminal=true\n"))?;
        first.write(
            "d.desktop",
            "[Desktop Entry]\nName=NoFile\nExec=nofile\nMimeType=text/plain;\n",
        )?;
        first.write(
            "e.desktop",
            "[Desktop Entry]\nName=Quoted\nExec=sh -c \"echo %f\"\n",
        )?;
        first.write("shadow.desktop", &entry("Shadow", "", "Hidden=true\n"))?;
        second.write("shadow.desktop", &entry("Shadow", "", ""))?;
        second.write("zed.desktop", &entry("Zed Old", "", ""))?;
        second.write("alpha.desktop", &entry("alpha", "", ""))?;

        let dirs = [first.0.clone(), second.0.clone()];
        let names: Vec<String> = find_all_apps(&dirs)
            .into_iter()
            .map(|app| app.name)
            .collect();
        assert_eq!(names, ["alpha", "beta", "Gimp", "Nested", "Zed"]);
        Ok(())
    }

    #[test]
    fn the_filter_matches_names_case_insensitively_and_keeps_the_order() {
        let app = |id: &str, name: &str| App {
            id: id.to_owned(),
            name: name.to_owned(),
            exec: "x %f".to_owned(),
        };
        let apps = [
            app("a.desktop", "Alpha Editor"),
            app("b.desktop", "Beta"),
            app("c.desktop", "Text EDITOR"),
        ];
        let ids = |query: &str| {
            filter_apps(&apps, query)
                .into_iter()
                .map(|app| app.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(""), ["a.desktop", "b.desktop", "c.desktop"]);
        assert_eq!(ids("  "), ["a.desktop", "b.desktop", "c.desktop"]);
        assert_eq!(ids("editor"), ["a.desktop", "c.desktop"]);
        assert_eq!(ids(" BETA "), ["b.desktop"]);
        assert!(ids("zzz").is_empty());
    }

    #[test]
    fn a_remembered_default_is_listed_first_even_when_it_does_not_declare_the_type() -> TestResult {
        let scratch = Scratch::new("default-any")?;
        scratch.write("alpha.desktop", &entry("Alpha", "text/plain;", ""))?;
        scratch.write("other.desktop", &entry("Other", "image/png;", ""))?;
        let dirs = [scratch.0.clone()];
        let names: Vec<String> = find_apps(&dirs, "text/plain", Some("other.desktop"))
            .into_iter()
            .map(|app| app.name)
            .collect();
        assert_eq!(names, ["Other", "Alpha"]);
        let unset: Vec<String> = find_apps(&dirs, "text/plain", None)
            .into_iter()
            .map(|app| app.name)
            .collect();
        assert_eq!(unset, ["Alpha"]);
        Ok(())
    }

    fn mimeapps(scratch: &Scratch) -> PathBuf {
        scratch.0.join("config/mimeapps.list")
    }

    fn only_file_in_config(scratch: &Scratch) -> std::io::Result<Vec<String>> {
        let mut names: Vec<String> = fs::read_dir(scratch.0.join("config"))?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<Result<_, _>>()?;
        names.sort();
        Ok(names)
    }

    #[test]
    fn remembering_creates_the_file_and_the_group_when_missing() -> TestResult {
        let scratch = Scratch::new("mime-create")?;
        let file = mimeapps(&scratch);
        set_default_application(&file, "text/plain", "gedit.desktop")?;
        assert_eq!(
            fs::read_to_string(&file)?,
            "[Default Applications]\ntext/plain=gedit.desktop;\n"
        );
        assert_eq!(only_file_in_config(&scratch)?, ["mimeapps.list"]);

        let second = Scratch::new("mime-create-2")?;
        let other = mimeapps(&second);
        fs::create_dir_all(other.parent().ok_or("no parent")?)?;
        fs::write(&other, "[Added Associations]\nimage/png=a.desktop;\n")?;
        set_default_application(&other, "text/plain", "gedit.desktop")?;
        assert_eq!(
            fs::read_to_string(&other)?,
            "[Added Associations]\nimage/png=a.desktop;\n\n\
             [Default Applications]\ntext/plain=gedit.desktop;\n",
            "a new group is appended after a blank line"
        );
        Ok(())
    }

    #[test]
    fn remembering_changes_one_line_and_keeps_every_other_byte() -> TestResult {
        let scratch = Scratch::new("mime-keep")?;
        let file = mimeapps(&scratch);
        fs::create_dir_all(file.parent().ok_or("no parent")?)?;
        let before = "# my apps\n[Added Associations]\ntext/plain=old.desktop;\n\n\
                      [Default Applications]\n# keep me\nimage/png=viewer.desktop;\n\
                      text/plain=old.desktop;gedit.desktop;\n  \nvideo/mp4=mpv.desktop;\n\n\
                      [Removed Associations]\ntext/plain=bad.desktop;";
        fs::write(&file, before)?;
        set_default_application(&file, "text/plain", "new app.desktop")?;
        assert_eq!(
            fs::read_to_string(&file)?,
            before.replace(
                "text/plain=old.desktop;gedit.desktop;",
                "text/plain=new app.desktop;"
            ),
            "only the existing default line changed (no trailing newline stays absent)"
        );

        // A key that is missing is added at the end of its group.
        set_default_application(&file, "audio/ogg", "player.desktop")?;
        assert_eq!(
            fs::read_to_string(&file)?,
            before
                .replace(
                    "text/plain=old.desktop;gedit.desktop;",
                    "text/plain=new app.desktop;"
                )
                .replace(
                    "video/mp4=mpv.desktop;\n",
                    "video/mp4=mpv.desktop;\naudio/ogg=player.desktop;\n"
                )
        );
        assert_eq!(only_file_in_config(&scratch)?, ["mimeapps.list"]);
        Ok(())
    }

    #[test]
    fn remembering_keeps_a_file_without_a_final_newline_valid() -> TestResult {
        let scratch = Scratch::new("mime-eof")?;
        let file = mimeapps(&scratch);
        fs::create_dir_all(file.parent().ok_or("no parent")?)?;
        fs::write(&file, "[Default Applications]\nimage/png=v.desktop;")?;
        set_default_application(&file, "text/plain", "g.desktop")?;
        assert_eq!(
            fs::read_to_string(&file)?,
            "[Default Applications]\nimage/png=v.desktop;\ntext/plain=g.desktop;\n"
        );
        Ok(())
    }

    #[test]
    fn remembering_tolerates_crlf_files_and_keeps_their_line_ends() -> TestResult {
        let scratch = Scratch::new("mime-crlf")?;
        let file = mimeapps(&scratch);
        fs::create_dir_all(file.parent().ok_or("no parent")?)?;
        fs::write(
            &file,
            "[Default Applications]\r\ntext/plain=old.desktop;\r\nimage/png=v.desktop;\r\n\r\n[Other]\r\nk=v\r\n",
        )?;
        set_default_application(&file, "text/plain", "g.desktop")?;
        assert_eq!(
            fs::read_to_string(&file)?,
            "[Default Applications]\r\ntext/plain=g.desktop;\r\nimage/png=v.desktop;\r\n\r\n[Other]\r\nk=v\r\n"
        );
        set_default_application(&file, "audio/ogg", "p.desktop")?;
        assert_eq!(
            fs::read_to_string(&file)?,
            "[Default Applications]\r\ntext/plain=g.desktop;\r\nimage/png=v.desktop;\r\naudio/ogg=p.desktop;\r\n\r\n[Other]\r\nk=v\r\n"
        );
        assert_eq!(
            default_application(&file, "text/plain").as_deref(),
            Some("g.desktop")
        );
        Ok(())
    }

    #[test]
    fn remembering_refuses_values_that_would_corrupt_the_file() -> TestResult {
        let scratch = Scratch::new("mime-invalid")?;
        let file = mimeapps(&scratch);
        for (mime, id) in [
            ("text/plain", "a\nb.desktop"),
            ("text/plain", "a;b.desktop"),
            ("text/plain", ""),
            ("text/p=lain", "a.desktop"),
            ("text/plain\n[X]", "a.desktop"),
            ("", "a.desktop"),
        ] {
            let result = set_default_application(&file, mime, id);
            assert!(
                result
                    .as_ref()
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::InvalidInput),
                "{mime:?} {id:?}: {result:?}"
            );
        }
        assert!(!file.exists(), "a refusal writes nothing");
        Ok(())
    }

    #[test]
    fn the_remembered_default_is_read_back_from_the_default_group_only() -> TestResult {
        let scratch = Scratch::new("mime-read")?;
        let file = mimeapps(&scratch);
        fs::create_dir_all(file.parent().ok_or("no parent")?)?;
        fs::write(
            &file,
            "[Added Associations]\ntext/plain=added.desktop;\n[Default Applications]\n\
             text/plain=first.desktop;second.desktop;\n",
        )?;
        assert_eq!(
            default_application(&file, "text/plain").as_deref(),
            Some("first.desktop")
        );
        assert_eq!(default_application(&file, "image/png"), None);
        assert_eq!(
            default_application(&scratch.0.join("missing"), "text/plain"),
            None
        );
        Ok(())
    }

    #[test]
    fn the_config_file_follows_the_xdg_config_home_rules() {
        let os = OsStr::new;
        assert_eq!(
            mimeapps_path(Some(os("/h")), None),
            Some(PathBuf::from("/h/.config/mimeapps.list"))
        );
        assert_eq!(
            mimeapps_path(Some(os("/h")), Some(os("/cfg"))),
            Some(PathBuf::from("/cfg/mimeapps.list"))
        );
        assert_eq!(
            mimeapps_path(Some(os("/h")), Some(os("relative"))),
            Some(PathBuf::from("/h/.config/mimeapps.list")),
            "a relative XDG_CONFIG_HOME is ignored"
        );
        assert_eq!(mimeapps_path(None, None), None);
    }

    fn entry_with(extra: &str) -> Option<DesktopEntry> {
        parse_desktop_entry(&format!("[Desktop Entry]\nName=T\nExec=t\n{extra}\n"))
    }

    #[test]
    fn categories_split_on_semicolons_and_drop_empty_and_blank_segments() {
        let categories = |value: &str| {
            entry_with(&format!("Categories={value}"))
                .map(|entry| entry.categories)
                .unwrap_or_default()
        };
        assert_eq!(
            categories("System;TerminalEmulator;"),
            ["System", "TerminalEmulator"]
        );
        assert_eq!(
            categories(" System ;; ;  TerminalEmulator ;;"),
            ["System", "TerminalEmulator"]
        );
        assert_eq!(categories(""), Vec::<String>::new());
        assert_eq!(categories("   "), Vec::<String>::new());
        assert_eq!(categories(";;;"), Vec::<String>::new());
        assert_eq!(
            entry_with("").map(|entry| entry.categories),
            Some(Vec::new()),
            "no key, no categories"
        );
    }

    #[test]
    fn x_terminal_arg_dir_is_the_trimmed_value_and_none_when_empty() {
        let arg_dir = |line: &str| entry_with(line).and_then(|entry| entry.terminal_arg_dir);
        assert_eq!(
            arg_dir("X-TerminalArgDir=--cwd="),
            Some("--cwd=".to_owned())
        );
        assert_eq!(
            arg_dir("X-TerminalArgDir=   --working-directory  "),
            Some("--working-directory".to_owned())
        );
        assert_eq!(arg_dir("X-TerminalArgDir="), None);
        assert_eq!(arg_dir("X-TerminalArgDir=    "), None);
        assert_eq!(arg_dir(""), None);
    }

    #[test]
    fn remembering_a_default_writes_through_a_symlinked_mimeapps_list()
    -> Result<(), Box<dyn std::error::Error>> {
        let scratch = Scratch::new("mimeapps-symlink")?;
        let real = scratch.0.join("dotfiles/mimeapps.list");
        std::fs::create_dir_all(scratch.0.join("dotfiles"))?;
        std::fs::write(&real, "[Default Applications]\nimage/png=a.desktop;\n")?;
        let link = scratch.0.join("config/mimeapps.list");
        std::fs::create_dir_all(scratch.0.join("config"))?;
        std::os::unix::fs::symlink(&real, &link)?;

        set_default_application(&link, "text/plain", "zed.desktop")?;

        assert!(
            std::fs::symlink_metadata(&link)?.file_type().is_symlink(),
            "the link is kept"
        );
        let written = std::fs::read_to_string(&real)?;
        assert!(written.contains("text/plain=zed.desktop;"), "{written}");
        assert!(written.contains("image/png=a.desktop;"), "{written}");
        Ok(())
    }

    // --- several files at once --------------------------------------------

    fn many(exec: &str, paths: &[&str]) -> Result<Vec<Vec<String>>, ExecError> {
        let paths: Vec<&Path> = paths.iter().map(Path::new).collect();
        Ok(expand_exec_many(exec, &paths)?
            .into_iter()
            .map(|argv| {
                argv.into_iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect()
            })
            .collect())
    }

    fn rows(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter()
            .map(|row| row.iter().map(|arg| (*arg).to_owned()).collect())
            .collect()
    }

    #[test]
    fn a_list_code_gets_every_file_in_one_process() -> TestResult {
        let files = ["/d/a b.txt", "/d/100%.txt", "/d/c.txt"];
        assert_eq!(
            many("viewer --new %F", &files)?,
            rows(&[&["viewer", "--new", "/d/a b.txt", "/d/100%.txt", "/d/c.txt"]])
        );
        assert_eq!(
            many("viewer %U", &files)?,
            rows(&[&[
                "viewer",
                "file:///d/a%20b.txt",
                "file:///d/100%25.txt",
                "file:///d/c.txt"
            ]])
        );
        // One file: the same as the single-file expansion.
        assert_eq!(many("viewer %F", &["/d/a"])?, rows(&[&["viewer", "/d/a"]]));
        Ok(())
    }

    #[test]
    fn a_single_code_gets_one_process_per_file() -> TestResult {
        let files = ["/d/a b.txt", "/d/c.txt"];
        assert_eq!(
            many("viewer %f", &files)?,
            rows(&[&["viewer", "/d/a b.txt"], &["viewer", "/d/c.txt"]])
        );
        assert_eq!(
            many("viewer --open=%u", &files)?,
            rows(&[
                &["viewer", "--open=file:///d/a%20b.txt"],
                &["viewer", "--open=file:///d/c.txt"]
            ])
        );
        assert_eq!(many("viewer %f", &["/d/a"])?, rows(&[&["viewer", "/d/a"]]));
        Ok(())
    }

    #[test]
    fn the_other_codes_and_percent_signs_are_as_for_one_file() -> TestResult {
        let files = ["/d/a", "/d/b"];
        // %i %c %k are dropped (as in the single-file expansion), %% stays.
        assert_eq!(
            many("viewer %i %c %k 100%% %F", &files)?,
            rows(&[&["viewer", "100%", "/d/a", "/d/b"]])
        );
        assert_eq!(
            many("viewer 100%% %f", &files)?,
            rows(&[&["viewer", "100%", "/d/a"], &["viewer", "100%", "/d/b"]])
        );
        // A list code and a single code together: one process, the single
        // code takes the first file.
        assert_eq!(
            many("viewer --first=%f %F", &files)?,
            rows(&[&["viewer", "--first=/d/a", "/d/a", "/d/b"]])
        );
        Ok(())
    }

    #[test]
    fn non_utf8_paths_keep_their_bytes_in_every_invocation() -> TestResult {
        let raw = [b'/', b'd', b'/', 0xff, b'.', b't'];
        let odd = Path::new(OsStr::from_bytes(&raw));
        let plain = Path::new("/d/x");
        let all = expand_exec_many("viewer %F", &[odd, plain])?;
        assert_eq!(
            all,
            vec![vec![
                OsString::from("viewer"),
                OsString::from_vec(raw.to_vec()),
                OsString::from("/d/x")
            ]]
        );
        let each = expand_exec_many("viewer %f", &[odd, plain])?;
        let second_args: Vec<Option<&OsString>> = each.iter().map(|argv| argv.get(1)).collect();
        assert_eq!(
            second_args,
            vec![
                Some(&OsString::from_vec(raw.to_vec())),
                Some(&OsString::from("/d/x"))
            ]
        );
        let uris = expand_exec_many("viewer %U", &[odd])?;
        assert_eq!(
            uris,
            vec![vec![
                OsString::from("viewer"),
                OsString::from("file:///d/%FF.t")
            ]]
        );
        Ok(())
    }

    #[test]
    fn a_quoted_field_code_is_still_refused_for_many_files() {
        let files = [Path::new("/d/a"), Path::new("/d/b")];
        assert_eq!(
            expand_exec_many(r#"sh -c "echo %F""#, &files),
            Err(ExecError::QuotedFieldCode)
        );
        assert_eq!(expand_exec_many("", &files), Err(ExecError::Empty));
        assert_eq!(expand_exec_many("viewer %F", &[]), Ok(Vec::new()));
    }

    #[test]
    fn the_single_file_expansion_is_unchanged() -> TestResult {
        assert_eq!(argv("viewer %F", "/d/a b")?, vec!["viewer", "/d/a b"]);
        assert_eq!(
            argv("viewer %U", "/d/a b")?,
            vec!["viewer", "file:///d/a%20b"]
        );
        Ok(())
    }

    fn found_in(scratch: &Scratch) -> Vec<PathBuf> {
        vec![scratch.0.join("applications")]
    }

    #[test]
    fn apps_for_several_types_handle_every_one_of_them() -> TestResult {
        let scratch = Scratch::new("intersection")?;
        scratch.write(
            "applications/both.desktop",
            &entry("Both", "text/plain;image/png;", ""),
        )?;
        scratch.write("applications/png.desktop", &entry("Png", "image/png;", ""))?;
        scratch.write(
            "applications/text.desktop",
            &entry("Text", "text/plain;", ""),
        )?;
        scratch.write(
            "applications/none.desktop",
            &entry("None", "audio/ogg;", ""),
        )?;
        let dirs = found_in(&scratch);
        let names = |mimes: &[&str], default: Option<&str>| -> Vec<String> {
            let mimes: Vec<String> = mimes.iter().map(|m| (*m).to_owned()).collect();
            find_apps_for_all(&dirs, &mimes, default)
                .into_iter()
                .map(|app| app.name)
                .collect()
        };
        assert_eq!(names(&["text/plain", "image/png"], None), ["Both"]);
        assert_eq!(
            names(&["image/png", "text/plain", "image/png"], None),
            ["Both"]
        );
        // The text/* fallback of the single-file logic applies to every type.
        assert_eq!(
            names(&["text/markdown", "text/x-rust"], None),
            ["Both", "Text"]
        );
        assert_eq!(
            names(&["text/plain", "audio/ogg"], None),
            Vec::<String>::new()
        );
        // One type: exactly what find_apps lists (default first, even when
        // it does not declare the type).
        assert_eq!(
            names(&["image/png"], Some("none.desktop")),
            ["None", "Both", "Png"]
        );
        let single = find_apps(&dirs, "image/png", Some("none.desktop"));
        assert_eq!(
            single
                .iter()
                .map(|app| app.name.as_str())
                .collect::<Vec<_>>(),
            ["None", "Both", "Png"]
        );
        Ok(())
    }

    fn app(id: &str) -> App {
        App {
            id: id.to_owned(),
            name: id.to_owned(),
            exec: format!("{id} %F"),
        }
    }

    #[test]
    fn files_group_by_their_default_app_in_first_seen_order() {
        let groups = group_by_app(vec![
            (PathBuf::from("/d/1.txt"), Some(app("edit"))),
            (PathBuf::from("/d/2.pdf"), Some(app("pdf"))),
            (PathBuf::from("/d/3.bin"), None),
            (PathBuf::from("/d/4.txt"), Some(app("edit"))),
            (PathBuf::from("/d/5.bin"), None),
        ]);
        let shown: Vec<(Option<&str>, Vec<&str>)> = groups
            .iter()
            .map(|group| {
                (
                    group.app.as_ref().map(|app| app.id.as_str()),
                    group
                        .paths
                        .iter()
                        .map(|p| p.to_str().unwrap_or(""))
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            vec![
                (Some("edit"), vec!["/d/1.txt", "/d/4.txt"]),
                (Some("pdf"), vec!["/d/2.pdf"]),
                (None, vec!["/d/3.bin", "/d/5.bin"]),
            ]
        );
        assert!(group_by_app(Vec::new()).is_empty());
    }

    #[test]
    fn the_default_app_of_each_file_comes_from_its_type() -> TestResult {
        let scratch = Scratch::new("defaults")?;
        scratch.write(
            "applications/edit.desktop",
            &entry("Edit", "text/plain;", ""),
        )?;
        scratch.write(
            "applications/pdf.desktop",
            &entry("Pdf", "application/pdf;", ""),
        )?;
        scratch.write(
            "applications/term.desktop",
            &entry("Term", "text/plain;", "Terminal=true\n"),
        )?;
        let dirs = found_in(&scratch);
        let files = vec![
            (PathBuf::from("/d/a.txt"), "text/plain".to_owned()),
            (PathBuf::from("/d/b.pdf"), "application/pdf".to_owned()),
            (PathBuf::from("/d/c.dat"), "application/x-none".to_owned()),
            (PathBuf::from("/d/d.txt"), "text/plain".to_owned()),
            (
                PathBuf::from("/d/e.sh"),
                "application/x-shellscript".to_owned(),
            ),
        ];
        let default_of = |mime: &str| match mime {
            "text/plain" => Some("edit.desktop".to_owned()),
            "application/pdf" => Some("pdf.desktop".to_owned()),
            // Names an app that is not installed, and one that cannot run.
            "application/x-shellscript" => Some("term.desktop".to_owned()),
            _ => None,
        };
        let groups = group_by_app(default_apps(&dirs, &files, &default_of));
        let shown: Vec<(Option<&str>, usize)> = groups
            .iter()
            .map(|g| (g.app.as_ref().map(|a| a.id.as_str()), g.paths.len()))
            .collect();
        assert_eq!(
            shown,
            vec![
                (Some("edit.desktop"), 2),
                (Some("pdf.desktop"), 1),
                (None, 2)
            ]
        );
        Ok(())
    }
}
