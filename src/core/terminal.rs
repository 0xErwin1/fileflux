//! "Open in terminal": finds the user's terminal emulator the way the
//! [xdg-terminal-exec] specification does, and builds the command that opens
//! it in a folder. No shell is involved: the terminal is started directly,
//! with the folder as its working directory and, for terminals that ignore
//! the inherited one, the argument that sets it.
//!
//! The terminal is the first of these that exists:
//!
//! 1. `$TERMINAL`, an executable name or path;
//! 2. `xdg-terminal-exec` on `$PATH`;
//! 3. the desktop ids in `xdg-terminals.list` (with the desktop-prefixed
//!    `<desktop>-xdg-terminals.list` variants for `$XDG_CURRENT_DESKTOP`) in
//!    `$XDG_CONFIG_HOME`, then `$XDG_CONFIG_DIRS`;
//! 4. the desktop entries with `Categories=TerminalEmulator`, the known
//!    terminals first ([`KNOWN`]'s order), then by name;
//! 5. the known terminals found on `$PATH`.
//!
//! [xdg-terminal-exec]: https://github.com/Vladimir-csp/xdg-terminal-exec

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use super::apps::{self, DesktopEntry};

/// The terminals Fileflux knows, in the order it prefers them, with how each
/// is told its working directory. `xterm` has no such option: it uses the
/// directory it is started in.
const KNOWN: [(&str, DirArg); 10] = [
    ("ghostty", DirArg::Joined("--working-directory=")),
    ("kitty", DirArg::Split(&["--directory"])),
    ("alacritty", DirArg::Split(&["--working-directory"])),
    ("foot", DirArg::Joined("--working-directory=")),
    ("wezterm", DirArg::Split(&["start", "--cwd"])),
    ("gnome-terminal", DirArg::Joined("--working-directory=")),
    ("kgx", DirArg::Joined("--working-directory=")),
    ("konsole", DirArg::Split(&["--workdir"])),
    ("xfce4-terminal", DirArg::Joined("--working-directory=")),
    ("xterm", DirArg::Inherit),
];

/// How a terminal is told which folder to open in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirArg {
    /// Only the working directory of the process.
    Inherit,
    /// One argument: this text followed by the folder (`--working-directory=`).
    Joined(&'static str),
    /// These arguments, then the folder as the next one.
    Split(&'static [&'static str]),
}

/// A terminal emulator ready to be started in a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Terminal {
    program: PathBuf,
    /// Arguments that come before the working-directory one.
    args: Vec<OsString>,
    dir: DirArgOwned,
}

/// [`DirArg`] where a desktop entry's `X-TerminalArgDir` may own its text.
#[derive(Clone, Debug, PartialEq, Eq)]
enum DirArgOwned {
    Inherit,
    Joined(String),
    Split(Vec<String>),
}

impl From<DirArg> for DirArgOwned {
    fn from(arg: DirArg) -> Self {
        match arg {
            DirArg::Inherit => Self::Inherit,
            DirArg::Joined(flag) => Self::Joined(flag.to_owned()),
            DirArg::Split(flags) => {
                Self::Split(flags.iter().map(|&flag| flag.to_owned()).collect())
            }
        }
    }
}

/// What to run: the program, its arguments and the working directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
}

impl Terminal {
    /// The terminal at `program`; a known one gets its own way of taking the
    /// folder, any other relies on the working directory.
    fn at(program: PathBuf) -> Self {
        let dir = known_dir_arg(&program).map_or(DirArgOwned::Inherit, DirArgOwned::from);
        Self {
            program,
            args: Vec::new(),
            dir,
        }
    }

    /// The program that starts the terminal.
    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// The command that opens this terminal in `folder`.
    #[must_use]
    pub fn command(&self, folder: &Path) -> TerminalCommand {
        let mut args = self.args.clone();
        match &self.dir {
            DirArgOwned::Inherit => {}
            DirArgOwned::Joined(flag) => {
                let mut joined = OsString::from(flag);
                joined.push(folder);
                args.push(joined);
            }
            DirArgOwned::Split(flags) => {
                args.extend(flags.iter().map(OsString::from));
                args.push(folder.into());
            }
        }
        TerminalCommand {
            program: self.program.clone(),
            args,
            cwd: folder.to_path_buf(),
        }
    }
}

fn known_dir_arg(program: &Path) -> Option<DirArg> {
    let name = program.file_name()?.to_str()?;
    KNOWN
        .iter()
        .find(|(known, _)| *known == name)
        .map(|&(_, arg)| arg)
}

/// The place of `program` in the known list, for sorting: unknown last.
fn known_rank(program: &Path) -> usize {
    program
        .file_name()
        .and_then(OsStr::to_str)
        .and_then(|name| KNOWN.iter().position(|(known, _)| *known == name))
        .unwrap_or(KNOWN.len())
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// An executable named `name`: a path is checked as it is, a bare name is
/// looked up in `path_var`.
#[must_use]
pub fn resolve_program(name: &OsStr, path_var: Option<&OsStr>) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let direct = Path::new(name);
    if direct.components().count() > 1 {
        return is_executable(direct).then(|| direct.to_path_buf());
    }
    std::env::split_paths(path_var?)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// The environment the terminal is looked up in.
#[derive(Clone, Debug, Default)]
pub struct TerminalEnv {
    pub terminal: Option<OsString>,
    pub path: Option<OsString>,
    pub home: Option<OsString>,
    pub config_home: Option<OsString>,
    pub config_dirs: Option<OsString>,
    pub data_home: Option<OsString>,
    pub data_dirs: Option<OsString>,
    pub current_desktop: Option<OsString>,
}

impl TerminalEnv {
    /// The variables of this process.
    #[must_use]
    pub fn from_process() -> Self {
        let var = |name| std::env::var_os(name);
        Self {
            terminal: var("TERMINAL"),
            path: var("PATH"),
            home: var("HOME"),
            config_home: var("XDG_CONFIG_HOME"),
            config_dirs: var("XDG_CONFIG_DIRS"),
            data_home: var("XDG_DATA_HOME"),
            data_dirs: var("XDG_DATA_DIRS"),
            current_desktop: var("XDG_CURRENT_DESKTOP"),
        }
    }

    /// The directories `xdg-terminals.list` files are looked for in, most
    /// specific first.
    fn config_roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = Vec::new();
        let home = self
            .config_home
            .as_deref()
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                self.home
                    .as_deref()
                    .filter(|home| !home.is_empty())
                    .map(|home| Path::new(home).join(".config"))
            });
        roots.extend(home);
        let dirs = self
            .config_dirs
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or(OsStr::new("/etc/xdg"));
        roots.extend(std::env::split_paths(dirs).filter(|path| path.is_absolute()));
        roots
    }

    /// The `xdg-terminals.list` file names to read, most specific first: one
    /// per `$XDG_CURRENT_DESKTOP` entry, then the plain one.
    fn list_names(&self) -> Vec<String> {
        let desktops = self
            .current_desktop
            .as_deref()
            .map(|value| value.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        desktops
            .split(':')
            .filter(|desktop| !desktop.is_empty())
            .map(|desktop| format!("{desktop}-xdg-terminals.list"))
            .chain(["xdg-terminals.list".to_owned()])
            .collect()
    }
}

/// The desktop ids a `xdg-terminals.list` names, in order. Comments, blank
/// lines and `-` lines (the list's way to disable a terminal) are skipped, a
/// leading `+` is dropped, and an `:action` suffix is ignored.
fn list_ids(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with('-'))
        .map(|line| line.trim_start_matches('+'))
        .map(|line| line.split(':').next().unwrap_or(line).trim().to_owned())
        .filter(|id| !id.is_empty())
        .collect()
}

/// The terminal a desktop entry runs: its `Exec` program (on `$PATH` or an
/// absolute path) and the working-directory argument it declares through
/// `X-TerminalArgDir`, or the known one. A known terminal is run as Fileflux
/// knows it, ignoring the entry's own arguments (which may already pick a
/// directory); another one keeps them.
fn from_entry(entry: &DesktopEntry, path_var: Option<&OsStr>) -> Option<Terminal> {
    let argv = apps::split_exec(&entry.exec).ok()?;
    let mut words = argv
        .into_iter()
        .filter(|word| !(word.len() == 2 && word.starts_with('%')));
    let program = resolve_program(OsStr::new(&words.next()?), path_var)?;
    let mut terminal = Terminal::at(program);
    if known_dir_arg(&terminal.program).is_some() {
        return Some(terminal);
    }
    terminal.args = words.map(OsString::from).collect();
    if let Some(flag) = &entry.terminal_arg_dir {
        terminal.dir = if flag.ends_with('=') {
            DirArgOwned::Joined(flag.clone())
        } else {
            DirArgOwned::Split(vec![flag.clone()])
        };
    }
    Some(terminal)
}

/// The terminal to open, or `None` when the machine has none (the menus then
/// leave "Open in terminal" out). Reads `.desktop` files, so call it off the
/// UI thread.
#[must_use]
pub fn resolve(env: &TerminalEnv) -> Option<Terminal> {
    let path_var = env.path.as_deref();
    if let Some(program) = env
        .terminal
        .as_deref()
        .and_then(|name| resolve_program(name, path_var))
    {
        return Some(Terminal::at(program));
    }
    if let Some(program) = resolve_program(OsStr::new("xdg-terminal-exec"), path_var) {
        return Some(Terminal::at(program));
    }

    let dirs = apps::application_dirs(
        env.home.as_deref(),
        env.data_home.as_deref(),
        env.data_dirs.as_deref(),
    );
    let entries = apps::desktop_entries(&dirs);
    let listed = env.config_roots().into_iter().flat_map(|root| {
        env.list_names()
            .into_iter()
            .map(move |name| root.join(name))
    });
    for file in listed {
        let Ok(bytes) = std::fs::read(&file) else {
            continue;
        };
        for id in list_ids(&String::from_utf8_lossy(&bytes)) {
            let terminal = entries
                .iter()
                .find(|(known, _)| *known == id)
                .filter(|(_, entry)| !entry.hidden)
                .and_then(|(_, entry)| from_entry(entry, path_var));
            if terminal.is_some() {
                return terminal;
            }
        }
    }

    let mut categorized: Vec<(usize, String, Terminal)> = entries
        .iter()
        .filter(|(_, entry)| {
            !entry.hidden
                && !entry.no_display
                && entry
                    .categories
                    .iter()
                    .any(|category| category == "TerminalEmulator")
        })
        .filter_map(|(_, entry)| {
            from_entry(entry, path_var).map(|terminal| {
                (
                    known_rank(&terminal.program),
                    entry.name.to_lowercase(),
                    terminal,
                )
            })
        })
        .collect();
    categorized.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    if let Some((_, _, terminal)) = categorized.into_iter().next() {
        return Some(terminal);
    }

    KNOWN
        .iter()
        .find_map(|(name, _)| resolve_program(OsStr::new(name), path_var))
        .map(Terminal::at)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::ffi::OsStrExt as _;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);
    /// A script written while another test forks can be exec'd with
    /// ETXTBSY: the tests that run fake programs take turns.
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A scratch directory under the system temp directory, removed on drop.
    struct Scratch(
        PathBuf,
        #[allow(dead_code)] std::sync::MutexGuard<'static, ()>,
    );

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "fileflux-terminal-{tag}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).expect("scratch is created");
            let turn = TURN
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Self(dir, turn)
        }

        fn dir(&self, rel: &str) -> PathBuf {
            let dir = self.0.join(rel);
            fs::create_dir_all(&dir).expect("dir creates");
            dir
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().expect("has a parent")).expect("dir creates");
            fs::write(path, text).expect("fixture writes");
        }

        /// A fake terminal in `bin/` that logs its arguments and working
        /// directory to `log/<name>`.
        fn terminal(&self, name: &str) -> PathBuf {
            let bin = self.dir("bin");
            let log = self.dir("log").join(name);
            let script = format!(
                "#!/bin/sh\n{{ for a in \"$@\"; do printf '[%s]' \"$a\"; done; printf '\\n%s\\n' \"$PWD\"; }} > '{}'\n",
                log.display()
            );
            let path = bin.join(name);
            // Written and closed under another name, then renamed into place,
            // so nothing ever execs a file that is still open for writing.
            let temp = bin.join(format!(".{name}.new"));
            fs::write(&temp, script).expect("script writes");
            fs::set_permissions(&temp, fs::Permissions::from_mode(0o755)).expect("chmod");
            fs::rename(&temp, &path).expect("script is put in place");
            path
        }

        fn env(&self) -> TerminalEnv {
            TerminalEnv {
                path: Some(self.0.join("bin").into_os_string()),
                config_home: Some(self.0.join("config").into_os_string()),
                config_dirs: Some(self.0.join("etc").into_os_string()),
                data_home: Some(self.0.join("data").into_os_string()),
                data_dirs: Some(self.0.join("share").into_os_string()),
                home: Some(self.0.join("home").into_os_string()),
                ..TerminalEnv::default()
            }
        }

        fn app(&self, id: &str, extra: &str) {
            self.write(
                &format!("data/applications/{id}"),
                &format!("[Desktop Entry]\nType=Application\n{extra}\n"),
            );
        }

        /// Runs the command like the explorer does and returns what the fake
        /// terminal logged: its arguments and its working directory.
        fn run(&self, terminal: &Terminal, folder: &Path) -> (String, String) {
            let command = terminal.command(folder);
            let name = command
                .program
                .file_name()
                .expect("has a name")
                .to_string_lossy()
                .into_owned();
            // Tests in other modules fork too, and a child forked while the
            // script was open for writing holds it until its own exec: that
            // is ETXTBSY for a moment, never a real failure.
            let mut attempts = 0;
            let status = loop {
                let spawned = Command::new(&command.program)
                    .args(&command.args)
                    .current_dir(&command.cwd)
                    .stdin(Stdio::null())
                    .status();
                match spawned {
                    Err(error)
                        if error.raw_os_error()
                            == Some(rustix::io::Errno::TXTBSY.raw_os_error())
                            && attempts < 100 =>
                    {
                        attempts += 1;
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    other => break other.expect("the fake terminal runs"),
                }
            };
            assert!(status.success());
            let log = fs::read_to_string(self.0.join("log").join(&name)).expect("log exists");
            let (argv, cwd) = log.split_once('\n').expect("two lines");
            (argv.to_owned(), cwd.trim_end().to_owned())
        }

        fn which(&self, terminal: &Terminal) -> String {
            terminal
                .program()
                .file_name()
                .expect("has a name")
                .to_string_lossy()
                .into_owned()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            drop(fs::remove_dir_all(&self.0));
        }
    }

    #[test]
    fn terminal_wins_and_each_known_terminal_gets_its_own_directory_argument() {
        let scratch = Scratch::new("known");
        let folder = scratch.dir("work dir");
        let cwd = fs::canonicalize(&folder).expect("canonical");
        let cwd = cwd.to_string_lossy().into_owned();
        let shown = folder.to_string_lossy().into_owned();
        let expected: [(&str, String); 10] = [
            ("ghostty", format!("[--working-directory={shown}]")),
            ("kitty", format!("[--directory][{shown}]")),
            ("alacritty", format!("[--working-directory][{shown}]")),
            ("foot", format!("[--working-directory={shown}]")),
            ("wezterm", format!("[start][--cwd][{shown}]")),
            ("gnome-terminal", format!("[--working-directory={shown}]")),
            ("kgx", format!("[--working-directory={shown}]")),
            ("konsole", format!("[--workdir][{shown}]")),
            ("xfce4-terminal", format!("[--working-directory={shown}]")),
            ("xterm", String::new()),
        ];
        for (name, argv) in expected {
            scratch.terminal(name);
            let env = TerminalEnv {
                terminal: Some(name.into()),
                ..scratch.env()
            };
            let terminal = resolve(&env).expect("the terminal resolves");
            assert_eq!(scratch.which(&terminal), name);
            assert_eq!(
                scratch.run(&terminal, &folder),
                (argv, cwd.clone()),
                "{name}"
            );
        }
    }

    #[test]
    fn an_unusable_terminal_variable_falls_through_to_xdg_terminal_exec() {
        let scratch = Scratch::new("exec");
        scratch.terminal("xdg-terminal-exec");
        scratch.write("bin/plain", "#!/bin/sh\n");
        let folder = scratch.dir("w");
        for value in ["missing", "plain", ""] {
            let env = TerminalEnv {
                terminal: Some(value.into()),
                ..scratch.env()
            };
            let terminal = resolve(&env).expect("falls through");
            assert_eq!(scratch.which(&terminal), "xdg-terminal-exec", "{value:?}");
            let (argv, cwd) = scratch.run(&terminal, &folder);
            assert_eq!(argv, "", "only the working directory is set");
            assert_eq!(
                cwd,
                fs::canonicalize(&folder)
                    .expect("canonical")
                    .to_string_lossy()
            );
        }
    }

    #[test]
    fn a_terminal_path_is_used_as_written() {
        let scratch = Scratch::new("path");
        let kitty = scratch.terminal("kitty");
        let env = TerminalEnv {
            terminal: Some(kitty.clone().into_os_string()),
            path: None,
            ..scratch.env()
        };
        assert_eq!(
            resolve(&env).map(|t| t.program().to_path_buf()),
            Some(kitty)
        );
    }

    #[test]
    fn the_terminals_list_names_desktop_ids_and_the_desktop_prefixed_one_wins() {
        let scratch = Scratch::new("list");
        scratch.terminal("ghostty");
        scratch.terminal("foot");
        scratch.app(
            "com.mitchellh.ghostty.desktop",
            "Name=Ghostty\nExec=ghostty",
        );
        scratch.app("org.codeberg.dnkl.foot.desktop", "Name=foot\nExec=foot");
        scratch.write(
            "config/xdg-terminals.list",
            "# my terminals\n\n-ignored.desktop\norg.codeberg.dnkl.foot.desktop\n",
        );
        scratch.write(
            "config/gnome-xdg-terminals.list",
            "no.such.terminal.desktop\n+com.mitchellh.ghostty.desktop:new-window\n",
        );
        let plain = scratch.env();
        let terminal = resolve(&plain).expect("the list resolves");
        assert_eq!(scratch.which(&terminal), "foot");

        let gnome = TerminalEnv {
            current_desktop: Some("X-Cinnamon:GNOME".into()),
            ..scratch.env()
        };
        let terminal = resolve(&gnome).expect("the desktop list resolves");
        assert_eq!(scratch.which(&terminal), "ghostty");
    }

    #[test]
    fn the_system_config_dirs_are_read_after_the_users() {
        let scratch = Scratch::new("etc");
        scratch.terminal("foot");
        scratch.app("foot.desktop", "Name=foot\nExec=foot");
        scratch.write("etc/xdg-terminals.list", "foot.desktop\n");
        let terminal = resolve(&scratch.env()).expect("the system list resolves");
        assert_eq!(scratch.which(&terminal), "foot");
    }

    #[test]
    fn an_unknown_listed_terminal_uses_its_entry_arguments_and_x_terminal_arg_dir() {
        let scratch = Scratch::new("argdir");
        scratch.terminal("mytty");
        scratch.app(
            "my.tty.desktop",
            "Name=My tty\nExec=mytty --flag %U\nX-TerminalArgDir=--cd=",
        );
        scratch.write("config/xdg-terminals.list", "my.tty.desktop\n");
        let folder = scratch.dir("w");
        let terminal = resolve(&scratch.env()).expect("resolves");
        let shown = folder.to_string_lossy().into_owned();
        assert_eq!(
            scratch.run(&terminal, &folder).0,
            format!("[--flag][--cd={shown}]")
        );

        scratch.app(
            "my.tty.desktop",
            "Name=My tty\nExec=mytty\nX-TerminalArgDir=-d",
        );
        let terminal = resolve(&scratch.env()).expect("resolves");
        assert_eq!(scratch.run(&terminal, &folder).0, format!("[-d][{shown}]"));

        scratch.app("my.tty.desktop", "Name=My tty\nExec=mytty");
        let terminal = resolve(&scratch.env()).expect("resolves");
        assert_eq!(scratch.run(&terminal, &folder).0, "", "cwd only");
    }

    #[test]
    fn a_listed_known_terminal_ignores_the_arguments_of_its_entry() {
        let scratch = Scratch::new("known-entry");
        scratch.terminal("wezterm");
        scratch.app("wez.desktop", "Name=WezTerm\nExec=wezterm start --cwd .");
        scratch.write("config/xdg-terminals.list", "wez.desktop\n");
        let folder = scratch.dir("w");
        let terminal = resolve(&scratch.env()).expect("resolves");
        let shown = folder.to_string_lossy().into_owned();
        assert_eq!(
            scratch.run(&terminal, &folder).0,
            format!("[start][--cwd][{shown}]")
        );
    }

    #[test]
    fn a_listed_id_whose_program_is_missing_is_skipped() {
        let scratch = Scratch::new("skip");
        scratch.terminal("foot");
        scratch.app("gone.desktop", "Name=Gone\nExec=not-installed");
        scratch.app("hidden.desktop", "Name=Hidden\nExec=foot\nHidden=true");
        scratch.app("foot.desktop", "Name=foot\nExec=foot");
        scratch.write(
            "config/xdg-terminals.list",
            "gone.desktop\nhidden.desktop\nfoot.desktop\n",
        );
        let terminal = resolve(&scratch.env()).expect("resolves");
        assert_eq!(scratch.which(&terminal), "foot");
    }

    #[test]
    fn terminal_emulator_entries_come_known_first_then_by_name() {
        let scratch = Scratch::new("categories");
        for name in ["aaa-term", "kitty", "ghostty", "xterm"] {
            scratch.terminal(name);
        }
        scratch.app(
            "a.desktop",
            "Name=AAA\nExec=aaa-term\nCategories=System;TerminalEmulator;",
        );
        scratch.app(
            "x.desktop",
            "Name=XTerm\nExec=xterm\nCategories=System;TerminalEmulator;",
        );
        scratch.app(
            "k.desktop",
            "Name=Kitty\nExec=kitty\nCategories=System;TerminalEmulator;",
        );
        scratch.app(
            "g.desktop",
            "Name=Ghostty\nExec=ghostty\nCategories=TerminalEmulator;\nNoDisplay=true",
        );
        let terminal = resolve(&scratch.env()).expect("resolves");
        assert_eq!(
            scratch.which(&terminal),
            "kitty",
            "known first; NoDisplay skipped"
        );

        fs::remove_file(scratch.0.join("data/applications/k.desktop")).expect("removes");
        fs::remove_file(scratch.0.join("data/applications/x.desktop")).expect("removes");
        let terminal = resolve(&scratch.env()).expect("resolves");
        assert_eq!(
            scratch.which(&terminal),
            "aaa-term",
            "an unknown one still counts"
        );
    }

    #[test]
    fn the_list_beats_the_categories_and_xdg_terminal_exec_beats_the_list() {
        let scratch = Scratch::new("order");
        for name in ["kitty", "foot", "xdg-terminal-exec"] {
            scratch.terminal(name);
        }
        scratch.app(
            "k.desktop",
            "Name=Kitty\nExec=kitty\nCategories=TerminalEmulator;",
        );
        scratch.app("f.desktop", "Name=foot\nExec=foot");
        scratch.write("config/xdg-terminals.list", "f.desktop\n");
        let terminal = resolve(&scratch.env()).expect("resolves");
        assert_eq!(scratch.which(&terminal), "xdg-terminal-exec");

        fs::remove_file(scratch.0.join("bin/xdg-terminal-exec")).expect("removes");
        let terminal = resolve(&scratch.env()).expect("resolves");
        assert_eq!(scratch.which(&terminal), "foot");

        fs::remove_file(scratch.0.join("config/xdg-terminals.list")).expect("removes");
        let terminal = resolve(&scratch.env()).expect("resolves");
        assert_eq!(scratch.which(&terminal), "kitty");
    }

    #[test]
    fn known_binaries_are_the_last_resort_in_the_known_order() {
        let scratch = Scratch::new("binaries");
        for name in ["xterm", "konsole", "alacritty"] {
            scratch.terminal(name);
        }
        let terminal = resolve(&scratch.env()).expect("resolves");
        assert_eq!(scratch.which(&terminal), "alacritty");
    }

    #[test]
    fn a_machine_without_a_terminal_has_none() {
        let scratch = Scratch::new("none");
        scratch.dir("bin");
        // Every lookup root is a scratch directory and `TERMINAL` and the
        // desktop are spelled out as absent, so nothing of the machine
        // (its PATH, /usr/share, /etc/xdg) can supply a terminal. The default
        // environment is not used: its fallbacks are the system directories.
        let env = TerminalEnv {
            terminal: None,
            current_desktop: None,
            ..scratch.env()
        };
        assert_eq!(resolve(&env), None);
        scratch.write("bin/notes", "text");
        assert_eq!(resolve(&env), None, "a plain file is no terminal");
    }

    #[test]
    fn the_folder_is_passed_as_one_argument_and_never_reaches_a_shell() {
        let scratch = Scratch::new("quoting");
        scratch.terminal("ghostty");
        let folder = scratch.dir("a b; touch pwned $(id)");
        let env = TerminalEnv {
            terminal: Some("ghostty".into()),
            ..scratch.env()
        };
        let terminal = resolve(&env).expect("resolves");
        let command = terminal.command(&folder);
        assert_eq!(command.args.len(), 1);
        assert!(
            command.args[0]
                .as_bytes()
                .ends_with(b"a b; touch pwned $(id)")
        );
        let (argv, _) = scratch.run(&terminal, &folder);
        assert!(argv.contains("a b; touch pwned $(id)"), "{argv}");
        assert!(!scratch.0.join("pwned").exists());
        assert!(!folder.join("pwned").exists());
    }

    #[test]
    fn a_non_utf8_folder_keeps_its_bytes() {
        let terminal = Terminal::at(PathBuf::from("/bin/kitty"));
        let folder = PathBuf::from(OsStr::from_bytes(b"/d/caf\xe9"));
        let command = terminal.command(&folder);
        assert_eq!(command.cwd, folder);
        assert_eq!(
            command.args.last().map(|arg| arg.as_bytes()),
            Some(&b"/d/caf\xe9"[..])
        );
    }

    #[test]
    fn a_relative_config_home_is_ignored_for_the_list() {
        let scratch = Scratch::new("relative");
        let env = TerminalEnv {
            config_home: Some("relative".into()),
            home: Some("/nonexistent-home".into()),
            config_dirs: Some("".into()),
            ..scratch.env()
        };
        let roots = env.config_roots();
        assert_eq!(
            roots,
            [
                PathBuf::from("/nonexistent-home/.config"),
                PathBuf::from("/etc/xdg")
            ]
        );
    }
}
