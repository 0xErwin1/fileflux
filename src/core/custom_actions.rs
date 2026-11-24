//! Custom actions: user-defined context menu entries.
//!
//! They live in `$XDG_CONFIG_HOME/fileflux/actions.toml` (fallback
//! `~/.config`) as `[[action]]` tables:
//!
//! ```toml
//! [[action]]
//! name = "Open a terminal here"
//! command = ["alacritty", "--working-directory", "%d"]
//! appears_on = "background"   # files | folders | both (default) | background
//! selection = "any"           # one | many | any (default)
//! mime = "image/*"            # optional
//! glob = "*.png"              # optional, matched against the names
//! ```
//!
//! `command` is an argv array and is never given to a shell (a user who
//! wants one writes `["sh", "-c", "...", "sh", "%f"]` themselves). Its
//! placeholders are `%f` (the first path), `%F` (every path, each its own
//! argument; only as a whole argument), `%d` (the folder of the first path;
//! for a `background` action, the shown folder itself), `%n` (the first
//! name) and `%%` (a literal percent sign). Paths are passed as raw bytes,
//! so a name that is not UTF-8 survives.
//!
//! The whole file is checked when it is read: a file with any error offers no
//! actions and reports one error naming the line. A missing file offers none
//! and reports nothing.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::SystemTime;

use serde::Deserialize;

use super::child;
use super::mimes;
use super::settings::settings_path;
use super::{Entry, glob};

const ACTIONS_FILE: &str = "actions.toml";

/// Where the actions file lives; `None` when neither the XDG variable nor
/// `$HOME` is usable. Same rules as [`settings_path`], next to it.
pub fn actions_path(home: Option<&OsStr>, xdg_config_home: Option<&OsStr>) -> Option<PathBuf> {
    settings_path(home, xdg_config_home).map(|path| path.with_file_name(ACTIONS_FILE))
}

/// [`actions_path`] for the process environment.
pub fn actions_path_from_env() -> Option<PathBuf> {
    actions_path(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
    )
}

// --- the model ----------------------------------------------------------------

/// What an action applies to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppearsOn {
    Files,
    Folders,
    #[default]
    Both,
    /// The empty space of a folder; the shown folder is the path.
    Background,
}

/// How many entries the action takes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SelectionCount {
    One,
    Many,
    #[default]
    Any,
}

/// One piece of an argument.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Part {
    Text(String),
    /// `%f`
    First,
    /// `%d`
    Folder,
    /// `%n`
    Name,
}

/// One argument of the command, parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Arg {
    /// A standalone `%F`: every path, each its own argument.
    All,
    Parts(Vec<Part>),
}

/// A validated action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    pub name: String,
    pub appears_on: AppearsOn,
    pub selection: SelectionCount,
    pub mime: Option<String>,
    pub glob: Option<String>,
    program: String,
    args: Vec<Arg>,
}

/// What a selected entry looks like to [`Action::applies_to`].
#[derive(Clone, Copy, Debug)]
pub struct Candidate<'a> {
    pub name: &'a OsStr,
    pub is_directory: bool,
}

impl<'a> Candidate<'a> {
    #[must_use]
    pub fn of(entry: &'a Entry) -> Self {
        Self {
            name: &entry.name,
            is_directory: entry.is_directory(),
        }
    }
}

/// A program and its arguments, ready to spawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl Action {
    /// Whether the action is offered for `selection` (the selected entries;
    /// empty for the menu of the empty space).
    #[must_use]
    pub fn applies_to(&self, selection: &[Candidate<'_>]) -> bool {
        if selection.is_empty() {
            return self.appears_on == AppearsOn::Background;
        }
        let count_fits = match self.selection {
            SelectionCount::One => selection.len() == 1,
            SelectionCount::Many => selection.len() >= 2,
            SelectionCount::Any => true,
        };
        let kind_fits = |entry: &Candidate<'_>| match self.appears_on {
            AppearsOn::Files => !entry.is_directory,
            AppearsOn::Folders => entry.is_directory,
            AppearsOn::Both => true,
            AppearsOn::Background => false,
        };
        count_fits
            && selection.iter().all(|entry| {
                kind_fits(entry)
                    && self
                        .mime
                        .as_deref()
                        .is_none_or(|pattern| mime_pattern_matches(pattern, entry))
                    && self
                        .glob
                        .as_deref()
                        .is_none_or(|pattern| glob::matches(pattern, &entry.name.to_string_lossy()))
            })
    }

    /// The command to run on `paths` (the selected paths, or the shown
    /// folder for a `background` action). Never goes through a shell.
    pub fn expand(&self, paths: &[PathBuf]) -> Result<Invocation, ExpandError> {
        let first = paths.first();
        let mut args = Vec::with_capacity(self.args.len());
        for arg in &self.args {
            match arg {
                Arg::All => args.extend(paths.iter().map(|path| path.as_os_str().to_owned())),
                Arg::Parts(parts) => {
                    let mut text = OsString::new();
                    for part in parts {
                        match part {
                            Part::Text(literal) => text.push(literal),
                            Part::First => {
                                text.push(first.ok_or(ExpandError::NoPaths)?.as_os_str());
                            }
                            Part::Folder => {
                                let first = first.ok_or(ExpandError::NoPaths)?;
                                let folder = if self.appears_on == AppearsOn::Background {
                                    first.as_path()
                                } else {
                                    first.parent().unwrap_or(first)
                                };
                                text.push(folder.as_os_str());
                            }
                            Part::Name => {
                                let first = first.ok_or(ExpandError::NoPaths)?;
                                text.push(first.file_name().unwrap_or_default());
                            }
                        }
                    }
                    args.push(text);
                }
            }
        }
        Ok(Invocation {
            program: OsString::from(&self.program),
            args,
        })
    }
}

/// Why a command could not be expanded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpandError {
    /// A path placeholder with no path to take it from.
    NoPaths,
}

impl std::fmt::Display for ExpandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPaths => f.write_str("there is no file to run it on"),
        }
    }
}

/// `image/*` or `image/png`, compared in lowercase. An entry with no known
/// type never matches; a folder is `inode/directory`.
fn mime_pattern_matches(pattern: &str, entry: &Candidate<'_>) -> bool {
    let mime = if entry.is_directory {
        "inode/directory"
    } else if let Some(mime) = mimes::of_name(entry.name) {
        mime
    } else {
        return false;
    };
    let pattern = pattern.to_ascii_lowercase();
    match pattern.strip_suffix("/*") {
        Some(group) => mime
            .split_once('/')
            .is_some_and(|(kind, _)| kind.eq_ignore_ascii_case(group)),
        None => mime.eq_ignore_ascii_case(&pattern),
    }
}

// --- parsing ------------------------------------------------------------------

/// Why the file offers no actions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionsError {
    /// The 1-based line of the problem, when known.
    pub line: Option<usize>,
    pub message: String,
}

impl std::fmt::Display for ActionsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(line) => write!(f, "{ACTIONS_FILE}, line {line}: {}", self.message),
            None => write!(f, "{ACTIONS_FILE}: {}", self.message),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    action: Vec<toml::Spanned<RawAction>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAction {
    name: String,
    command: Vec<String>,
    #[serde(default)]
    appears_on: AppearsOn,
    #[serde(default)]
    selection: SelectionCount,
    mime: Option<String>,
    glob: Option<String>,
}

/// 1-based line of byte offset `at` in `text`.
fn line_of(text: &str, at: usize) -> usize {
    text.as_bytes()
        .iter()
        .take(at)
        .filter(|&&byte| byte == b'\n')
        .count()
        + 1
}

/// Splits one argument into its parts; the error names the bad placeholder.
fn parse_arg(arg: &str) -> Result<Arg, String> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut has_all = false;
    let mut chars = arg.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            text.push(ch);
            continue;
        }
        let part = match chars.next() {
            Some('%') => {
                text.push('%');
                continue;
            }
            Some('f') => Part::First,
            Some('d') => Part::Folder,
            Some('n') => Part::Name,
            Some('F') => {
                has_all = true;
                continue;
            }
            Some(other) => return Err(format!("unknown placeholder %{other} in {arg:?}")),
            None => return Err(format!("a lone % at the end of {arg:?} (write %% for one)")),
        };
        if !text.is_empty() {
            parts.push(Part::Text(std::mem::take(&mut text)));
        }
        parts.push(part);
    }
    if has_all {
        return if arg == "%F" {
            Ok(Arg::All)
        } else {
            Err(format!(
                "%F must be an argument on its own, not part of {arg:?} (use %f, %d or %n inside a larger argument)"
            ))
        };
    }
    if !text.is_empty() {
        parts.push(Part::Text(text));
    }
    Ok(Arg::Parts(parts))
}

/// Whether `program` is an absolute path to an executable file, or a bare
/// name found as one in a directory of `path_var`.
fn resolves(program: &str, path_var: Option<&OsStr>) -> bool {
    let executable = |path: &Path| {
        std::fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    };
    if program.starts_with('/') {
        return executable(Path::new(program));
    }
    if program.contains('/') {
        return false;
    }
    path_var.is_some_and(|dirs| {
        std::env::split_paths(dirs)
            .filter(|dir| dir.is_absolute())
            .any(|dir| executable(&dir.join(program)))
    })
}

/// Reads and validates the file's `text`. `path_var` is the `PATH` the
/// programs are looked up in.
pub fn parse(text: &str, path_var: Option<&OsStr>) -> Result<Vec<Action>, ActionsError> {
    let raw: RawFile = toml::from_str(text).map_err(|error| ActionsError {
        line: error.span().map(|span| line_of(text, span.start)),
        message: error.message().trim().to_owned(),
    })?;
    let mut actions = Vec::with_capacity(raw.action.len());
    for (index, spanned) in raw.action.iter().enumerate() {
        let line = Some(line_of(text, spanned.span().start));
        let fail = |message: String| ActionsError {
            line,
            message: format!("action {}: {message}", index + 1),
        };
        let raw = spanned.get_ref();
        if raw.name.trim().is_empty() {
            return Err(fail("the name is empty".to_owned()));
        }
        let name = raw.name.trim().to_owned();
        let Some((program, rest)) = raw.command.split_first() else {
            return Err(fail(format!("{name:?} has an empty command")));
        };
        if program.is_empty() || program.contains('%') {
            return Err(fail(format!(
                "{name:?}: the program must be a plain name or path, without placeholders"
            )));
        }
        if !resolves(program, path_var) {
            return Err(fail(format!(
                "{name:?}: the program {program:?} is not an executable file on PATH or at that absolute path"
            )));
        }
        let args = rest
            .iter()
            .map(|arg| parse_arg(arg))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|message| fail(format!("{name:?}: {message}")))?;
        for (what, pattern) in [("mime", &raw.mime), ("glob", &raw.glob)] {
            if pattern
                .as_deref()
                .is_some_and(|pattern| pattern.trim().is_empty())
            {
                return Err(fail(format!("{name:?}: {what} is empty")));
            }
        }
        actions.push(Action {
            name,
            appears_on: raw.appears_on,
            selection: raw.selection,
            mime: raw.mime.clone(),
            glob: raw.glob.clone(),
            program: program.clone(),
            args,
        });
    }
    Ok(actions)
}

// --- the loaded file ------------------------------------------------------------

/// The actions file as last read: reloaded when its modification time or
/// size changes, so a menu opening costs one `stat`.
#[derive(Debug, Default)]
pub struct ActionsStore {
    path: Option<PathBuf>,
    stamp: Option<(SystemTime, u64)>,
    seen: bool,
    actions: Vec<Action>,
}

impl ActionsStore {
    /// A store for the file at `path` (`None`: nothing is ever offered).
    #[must_use]
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            ..Self::default()
        }
    }

    /// The actions as of the last [`refresh`](Self::refresh).
    #[must_use]
    pub fn actions(&self) -> &[Action] {
        &self.actions
    }

    /// Re-reads the file when it changed since the last call. Returns the
    /// error to show once when the new content is invalid (the file offers
    /// nothing then); an unchanged, missing or valid file returns `None`.
    pub fn refresh(&mut self, path_var: Option<&OsStr>) -> Option<ActionsError> {
        let path = self.path.as_deref()?;
        let stamp = std::fs::metadata(path)
            .ok()
            .and_then(|meta| Some((meta.modified().ok()?, meta.len())));
        if self.seen && stamp == self.stamp {
            return None;
        }
        self.seen = true;
        self.stamp = stamp;
        self.actions.clear();
        stamp?;
        let text = match std::fs::read(path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) => {
                // Unreadable: try again at the next change of the file.
                log::warn!("cannot read {}: {error}", path.display());
                return Some(ActionsError {
                    line: None,
                    message: format!("cannot be read: {error}"),
                });
            }
        };
        match parse(&text, path_var) {
            Ok(actions) => {
                self.actions = actions;
                None
            }
            Err(error) => {
                log::warn!("custom actions disabled: {error}");
                Some(error)
            }
        }
    }
}

// --- running ------------------------------------------------------------------

/// Starts `program` detached: in a process group of its own, stdin from
/// `/dev/null`, output discarded, working directory `cwd`. The child is
/// reaped on a thread of its own, so it may outlive the call. No shell.
pub fn spawn_detached(program: &OsStr, args: &[OsString], cwd: Option<&Path>) -> io::Result<()> {
    let mut command = child::command(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("{}: {error}", Path::new(program).display()),
        )
    })?;
    std::thread::Builder::new()
        .name("fileflux-reap".to_owned())
        .spawn(move || {
            if let Err(error) = child.wait() {
                log::debug!("waiting for a launched program failed: {error}");
            }
        })?;
    Ok(())
}

/// Shorthand for tests and callers holding a byte path.
#[must_use]
pub fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use std::fs;
    use std::os::unix::ffi::OsStrExt as _;
    use std::time::{Duration, Instant};

    use super::*;

    const SH: &str = "/bin/sh";

    fn path_var() -> Option<OsString> {
        Some(OsString::from("/usr/bin:/bin"))
    }

    fn parse_ok(text: &str) -> Vec<Action> {
        parse(text, path_var().as_deref()).expect("valid actions")
    }

    fn parse_err(text: &str) -> ActionsError {
        parse(text, path_var().as_deref()).expect_err("invalid actions")
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "fileflux-custom-actions-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            fs::remove_dir_all(&dir).ok();
            fs::create_dir_all(&dir).expect("scratch dir");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn action(appears_on: &str, selection: &str, extra: &str) -> Action {
        let text = format!(
            "[[action]]\nname = \"A\"\ncommand = [\"sh\"]\nappears_on = \"{appears_on}\"\nselection = \"{selection}\"\n{extra}\n"
        );
        parse_ok(&text).remove(0)
    }

    fn file(name: &str) -> Candidate<'_> {
        Candidate {
            name: OsStr::new(name),
            is_directory: false,
        }
    }

    fn folder(name: &str) -> Candidate<'_> {
        Candidate {
            name: OsStr::new(name),
            is_directory: true,
        }
    }

    fn argv(action: &Action, paths: &[&str]) -> Vec<OsString> {
        let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        action.expand(&paths).expect("expands").args
    }

    fn with_command(command: &str) -> Action {
        parse_ok(&format!(
            "[[action]]\nname = \"A\"\ncommand = [\"sh\", {command}]\n"
        ))
        .remove(0)
    }

    // --- config ---

    #[test]
    fn the_file_lives_next_to_the_settings() {
        assert_eq!(
            actions_path(Some(OsStr::new("/h")), Some(OsStr::new("/x"))),
            Some(PathBuf::from("/x/fileflux/actions.toml"))
        );
        assert_eq!(
            actions_path(Some(OsStr::new("/h")), Some(OsStr::new("rel"))),
            Some(PathBuf::from("/h/.config/fileflux/actions.toml"))
        );
        assert_eq!(actions_path(None, None), None);
    }

    #[test]
    fn a_valid_file_parses_with_defaults() {
        let actions = parse_ok(
            "[[action]]\nname = \"Term\"\ncommand = [\"sh\", \"-c\", \"x\"]\nappears_on = \"background\"\n\n\
             [[action]]\nname = \"Img\"\ncommand = [\"/bin/sh\", \"%F\"]\nmime = \"image/*\"\nglob = \"*.png\"\nselection = \"many\"\n",
        );
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].name, "Term");
        assert_eq!(actions[0].appears_on, AppearsOn::Background);
        assert_eq!(actions[0].selection, SelectionCount::Any);
        assert_eq!(actions[1].appears_on, AppearsOn::Both);
        assert_eq!(actions[1].selection, SelectionCount::Many);
        assert_eq!(actions[1].mime.as_deref(), Some("image/*"));
        assert_eq!(actions[1].glob.as_deref(), Some("*.png"));
    }

    #[test]
    fn an_empty_file_has_no_actions() {
        assert!(parse_ok("").is_empty());
    }

    #[test]
    fn invalid_toml_names_the_line() {
        let error = parse_err("[[action]]\nname = \"A\"\ncommand = [\n");
        assert!(error.line.is_some_and(|line| line >= 1), "{error:?}");
        assert!(error.to_string().starts_with("actions.toml, line "));
    }

    #[test]
    fn an_empty_command_is_refused_with_its_line() {
        let error = parse_err("\n[[action]]\nname = \"A\"\ncommand = []\n");
        assert_eq!(error.line, Some(2));
        assert!(error.message.contains("empty command"), "{error:?}");
    }

    #[test]
    fn an_empty_name_is_refused() {
        let error = parse_err("[[action]]\nname = \"  \"\ncommand = [\"sh\"]\n");
        assert!(error.message.contains("name is empty"), "{error:?}");
    }

    #[test]
    fn an_unknown_placeholder_is_refused() {
        for bad in ["%x", "100%", "--a=%u"] {
            let error = parse_err(&format!(
                "[[action]]\nname = \"A\"\ncommand = [\"sh\", \"{bad}\"]\n"
            ));
            assert!(
                error.message.contains("placeholder") || error.message.contains("lone %"),
                "{bad}: {error:?}"
            );
        }
    }

    #[test]
    fn placeholders_in_the_program_are_refused() {
        let error = parse_err("[[action]]\nname = \"A\"\ncommand = [\"%f\"]\n");
        assert!(error.message.contains("placeholders"), "{error:?}");
    }

    #[test]
    fn an_unknown_enum_value_or_key_is_refused() {
        let enum_error =
            parse_err("[[action]]\nname = \"A\"\ncommand = [\"sh\"]\nappears_on = \"sometimes\"\n");
        assert_eq!(enum_error.line, Some(4));
        let count =
            parse_err("[[action]]\nname = \"A\"\ncommand = [\"sh\"]\nselection = \"two\"\n");
        assert_eq!(count.line, Some(4));
        let key =
            parse_err("[[action]]\nname = \"A\"\ncommand = [\"sh\"]\nappear_on = \"files\"\n");
        assert!(key.message.contains("appear_on"), "{key:?}");
    }

    #[test]
    fn the_program_must_resolve() {
        let missing =
            parse_err("[[action]]\nname = \"A\"\ncommand = [\"fileflux-no-such-program\"]\n");
        assert!(missing.message.contains("not an executable"), "{missing:?}");
        let relative = parse_err("[[action]]\nname = \"A\"\ncommand = [\"./sh\"]\n");
        assert!(
            relative.message.contains("not an executable"),
            "{relative:?}"
        );
        let absolute_missing = parse_err("[[action]]\nname = \"A\"\ncommand = [\"/no/such/x\"]\n");
        assert!(absolute_missing.line.is_some());
        assert_eq!(
            parse("[[action]]\nname = \"A\"\ncommand = [\"sh\"]\n", None)
                .expect_err("no PATH")
                .line,
            Some(1)
        );
        assert_eq!(
            parse_ok("[[action]]\nname = \"A\"\ncommand = [\"/bin/sh\"]\n").len(),
            1
        );
    }

    #[test]
    fn the_store_reloads_when_the_file_changes() {
        let scratch = Scratch::new("store");
        let path = scratch.0.join("actions.toml");
        let mut store = ActionsStore::new(Some(path.clone()));
        let var = path_var();
        assert_eq!(
            store.refresh(var.as_deref()),
            None,
            "a missing file is silent"
        );
        assert!(store.actions().is_empty());

        fs::write(&path, "[[action]]\nname = \"A\"\ncommand = [\"sh\"]\n").expect("write");
        assert_eq!(store.refresh(var.as_deref()), None);
        assert_eq!(store.actions().len(), 1);

        fs::write(&path, "[[action]]\nname = \"A\"\ncommand = []\n").expect("write");
        let error = store.refresh(var.as_deref()).expect("invalid reports");
        assert_eq!(error.line, Some(1));
        assert!(store.actions().is_empty(), "an invalid file offers nothing");
        assert_eq!(store.refresh(var.as_deref()), None, "the notice shows once");
        assert!(store.actions().is_empty());

        fs::remove_file(&path).expect("remove");
        assert_eq!(store.refresh(var.as_deref()), None);
        assert!(store.actions().is_empty());

        assert_eq!(ActionsStore::new(None).refresh(var.as_deref()), None);
    }

    // --- expansion ---

    #[test]
    fn each_placeholder_expands() {
        let a = with_command(r#""%f", "%d", "%n", "--file=%f", "%d/%n", "100%%", "%%f""#);
        assert_eq!(
            argv(&a, &["/h/u/pic one.png", "/h/u/two.png"]),
            [
                "/h/u/pic one.png",
                "/h/u",
                "pic one.png",
                "--file=/h/u/pic one.png",
                "/h/u/pic one.png",
                "100%",
                "%f",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn a_standalone_all_is_one_argument_per_path() {
        let a = with_command(r#""-x", "%F", "end""#);
        assert_eq!(
            argv(&a, &["/a b", "/c"]),
            ["-x", "/a b", "/c", "end"].map(OsString::from)
        );
    }

    #[test]
    fn all_inside_a_larger_argument_is_refused() {
        for bad in ["--files=%F", "%F%f", "%F%F", "a%F"] {
            let error = parse_err(&format!(
                "[[action]]\nname = \"A\"\ncommand = [\"sh\", \"{bad}\"]\n"
            ));
            assert!(
                error.message.contains("%F must be an argument on its own"),
                "{bad}: {error:?}"
            );
        }
        // An escaped percent sign next to an F is no placeholder.
        assert_eq!(
            argv(&with_command(r#""%%F""#), &["/x"]),
            [OsString::from("%F")]
        );
    }

    #[test]
    fn a_background_action_takes_the_shown_folder_as_its_folder() {
        let text = "[[action]]\nname = \"A\"\ncommand = [\"sh\", \"%f\", \"%d\", \"%n\"]\nappears_on = \"background\"\n";
        let a = parse_ok(text).remove(0);
        assert_eq!(
            argv(&a, &["/home/u/docs"]),
            ["/home/u/docs", "/home/u/docs", "docs"].map(OsString::from)
        );
    }

    #[test]
    fn non_utf8_paths_survive_expansion() {
        let a = with_command(r#""%f", "--f=%f", "%F", "%n""#);
        let odd = path_from_bytes(b"/d/bad\xff\xfe.txt".to_vec());
        let args = a.expand(std::slice::from_ref(&odd)).expect("expands").args;
        assert_eq!(args[0], odd.as_os_str());
        assert_eq!(args[1].as_bytes(), b"--f=/d/bad\xff\xfe.txt");
        assert_eq!(args[2], odd.as_os_str());
        assert_eq!(args[3].as_bytes(), b"bad\xff\xfe.txt");
    }

    #[test]
    fn a_path_placeholder_without_paths_fails_but_all_expands_to_nothing() {
        assert_eq!(
            with_command(r#""%f""#).expand(&[]),
            Err(ExpandError::NoPaths)
        );
        assert!(argv(&with_command(r#""%F""#), &[]).is_empty());
    }

    // --- matching ---

    #[test]
    fn appears_on_picks_files_folders_both_and_background() {
        let files = action("files", "any", "");
        let folders = action("folders", "any", "");
        let both = action("both", "any", "");
        let background = action("background", "any", "");
        let one_file = [file("a.txt")];
        let one_folder = [folder("d")];
        let mixed = [file("a.txt"), folder("d")];
        assert!(files.applies_to(&one_file));
        assert!(!files.applies_to(&one_folder));
        assert!(!files.applies_to(&mixed));
        assert!(folders.applies_to(&one_folder));
        assert!(!folders.applies_to(&one_file));
        assert!(both.applies_to(&mixed));
        assert!(!background.applies_to(&one_file));
        assert!(!background.applies_to(&one_folder));
        assert!(background.applies_to(&[]), "the empty space");
        for other in [&files, &folders, &both] {
            assert!(!other.applies_to(&[]), "not on the empty space");
        }
    }

    #[test]
    fn selection_counts_one_many_and_any() {
        let one = action("both", "one", "");
        let many = action("both", "many", "");
        let any = action("both", "any", "");
        let a = [file("a")];
        let b = [file("a"), file("b")];
        assert!(one.applies_to(&a) && !one.applies_to(&b));
        assert!(!many.applies_to(&a) && many.applies_to(&b));
        assert!(any.applies_to(&a) && any.applies_to(&b));
        // The count of a background action means nothing.
        assert!(action("background", "many", "").applies_to(&[]));
    }

    #[test]
    fn mime_patterns_match_the_type_of_every_selected_entry() {
        let images = action("both", "any", "mime = \"image/*\"");
        let png = action("both", "any", "mime = \"IMAGE/PNG\"");
        let dirs = action("both", "any", "mime = \"inode/directory\"");
        assert!(images.applies_to(&[file("a.PNG"), file("b.jpg")]));
        assert!(!images.applies_to(&[file("a.png"), file("b.txt")]));
        assert!(!images.applies_to(&[file("noextension")]), "unknown type");
        assert!(!images.applies_to(&[folder("images")]));
        assert!(png.applies_to(&[file("a.png")]));
        assert!(!png.applies_to(&[file("a.jpg")]));
        assert!(dirs.applies_to(&[folder("x")]));
        assert!(!dirs.applies_to(&[file("x.png")]));
    }

    #[test]
    fn glob_patterns_match_the_name_of_every_selected_entry() {
        let docs = action("both", "any", "glob = \"*.md\"");
        assert!(docs.applies_to(&[file("README.MD"), file("notes.md")]));
        assert!(!docs.applies_to(&[file("a.md"), file("a.txt")]));
        assert!(action("both", "any", "glob = \"d*\"").applies_to(&[folder("docs")]));
    }

    #[test]
    fn every_filter_must_hold_together() {
        let all = action("files", "many", "mime = \"image/*\"\nglob = \"holiday*\"");
        assert!(all.applies_to(&[file("holiday1.png"), file("holiday2.jpg")]));
        assert!(!all.applies_to(&[file("holiday1.png")]), "count");
        assert!(
            !all.applies_to(&[file("holiday1.png"), file("work.png")]),
            "glob"
        );
        assert!(
            !all.applies_to(&[file("holiday1.png"), file("holiday2.txt")]),
            "mime"
        );
        assert!(
            !all.applies_to(&[file("holiday1.png"), folder("holiday")]),
            "kind"
        );
    }

    // --- running ---

    fn wait_for(path: &Path) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(bytes) = fs::read(path)
                && bytes.ends_with(b"END\n")
            {
                return bytes;
            }
            assert!(Instant::now() < deadline, "the script never wrote {path:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_spawned_action_gets_exact_argv_and_no_shell() {
        let scratch = Scratch::new("spawn");
        let script = scratch.0.join("record.sh");
        // The first argument is the output file; every other one is written
        // on its own line between brackets, after the working directory.
        fs::write(
            &script,
            "out=$1; shift\n{ printf 'cwd=%s\\n' \"$PWD\"; for a in \"$@\"; do printf '[%s]\\n' \"$a\"; done; echo END; } > \"$out.tmp\" </dev/null\nmv \"$out.tmp\" \"$out\"\n",
        )
        .expect("script");
        let out = scratch.0.join("out.txt");
        let work = scratch.0.join("work dir");
        fs::create_dir(&work).expect("cwd");

        let text = format!(
            "[[action]]\nname = \"Rec\"\ncommand = [\"{SH}\", \"{}\", \"{}\", \"--f=%f\", \"%F\", \"%n\", \"$(touch pwned)\"]\n",
            script.display(),
            out.display()
        );
        let action = parse(&text, None).expect("valid").remove(0);
        let tricky = "/d/it's \"quoted\" $HOME `x` ; rm -rf *.txt";
        let paths = [PathBuf::from(tricky), PathBuf::from("/d/two words")];
        let invocation = action.expand(&paths).expect("expands");
        spawn_detached(&invocation.program, &invocation.args, Some(&work)).expect("spawns");

        let written = String::from_utf8(wait_for(&out)).expect("utf8");
        let expected = format!(
            "cwd={}\n[--f={tricky}]\n[{tricky}]\n[/d/two words]\n[it's \"quoted\" $HOME `x` ; rm -rf *.txt]\n[$(touch pwned)]\nEND\n",
            work.display()
        );
        assert_eq!(written, expected);
        assert!(!work.join("pwned").exists() && !scratch.0.join("pwned").exists());
    }

    #[test]
    fn a_program_that_cannot_start_reports_its_name() {
        let error =
            spawn_detached(OsStr::new("/no/such/program"), &[], None).expect_err("cannot start");
        assert!(error.to_string().contains("/no/such/program"), "{error}");
    }
}
