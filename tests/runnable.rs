//! What the "Run / Run in Terminal / Open" prompt keys on: a regular file the
//! user may execute whose content starts with ELF magic or `#!`, the
//! `.desktop` rule, the launch plan of a Desktop Entry and the terminal
//! command that runs a program.

use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs as unix_fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use fileflux::core::apps::desktop_launch;
use fileflux::core::runnable::{is_desktop_file, is_runnable};
use fileflux::core::terminal::{TerminalEnv, resolve};

type TestResult = Result<(), Box<dyn Error>>;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A scratch directory under the system temp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> io::Result<Scratch> {
        let dir = std::env::temp_dir().join(format!(
            "fileflux-runnable-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir)?;
        Ok(Scratch(dir))
    }

    fn file(&self, name: &str, content: &[u8], mode: u32) -> io::Result<PathBuf> {
        let path = self.0.join(name);
        fs::write(&path, content)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
        Ok(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!("could not remove {}: {error}", self.0.display());
        }
    }
}

const ELF: &[u8] = b"\x7fELF\x02\x01\x01\x00rest of the header";

#[test]
fn an_executable_elf_file_is_runnable() -> TestResult {
    let scratch = Scratch::new()?;
    assert!(is_runnable(&scratch.file("tool", ELF, 0o755)?));
    Ok(())
}

#[test]
fn an_executable_script_with_a_shebang_is_runnable() -> TestResult {
    let scratch = Scratch::new()?;
    assert!(is_runnable(&scratch.file(
        "run.sh",
        b"#!/bin/sh\necho hi\n",
        0o755
    )?));
    // Only the first two bytes decide: an interpreter-less `#!` still counts.
    assert!(is_runnable(&scratch.file("odd", b"#!", 0o700)?));
    Ok(())
}

#[test]
fn an_executable_text_file_without_a_shebang_is_not_runnable() -> TestResult {
    let scratch = Scratch::new()?;
    // vfat, ntfs and MTP mark everything executable.
    assert!(!is_runnable(&scratch.file(
        "notes.txt",
        b"hello\n",
        0o755
    )?));
    assert!(!is_runnable(&scratch.file("empty", b"", 0o755)?));
    assert!(!is_runnable(&scratch.file(
        "hash",
        b"# comment\n",
        0o755
    )?));
    assert!(!is_runnable(&scratch.file("short", b"\x7fEL", 0o755)?));
    Ok(())
}

#[test]
fn a_file_without_the_execute_bit_is_not_runnable() -> TestResult {
    let scratch = Scratch::new()?;
    assert!(!is_runnable(&scratch.file("tool", ELF, 0o644)?));
    assert!(!is_runnable(&scratch.file(
        "run.sh",
        b"#!/bin/sh\n",
        0o644
    )?));
    Ok(())
}

#[test]
fn a_directory_and_a_missing_path_are_not_runnable() -> TestResult {
    let scratch = Scratch::new()?;
    let dir = scratch.0.join("bin");
    fs::create_dir(&dir)?;
    assert!(!is_runnable(&dir));
    assert!(!is_runnable(&scratch.0.join("missing")));
    Ok(())
}

#[test]
fn a_symlink_is_resolved_like_the_default_open_does() -> TestResult {
    let scratch = Scratch::new()?;
    let tool = scratch.file("tool", ELF, 0o755)?;
    let link = scratch.0.join("link");
    unix_fs::symlink(&tool, &link)?;
    assert!(is_runnable(&link));

    let text = scratch.file("notes", b"hello\n", 0o755)?;
    let text_link = scratch.0.join("text-link");
    unix_fs::symlink(&text, &text_link)?;
    assert!(!is_runnable(&text_link));

    let dir_link = scratch.0.join("dir-link");
    unix_fs::symlink(&scratch.0, &dir_link)?;
    assert!(!is_runnable(&dir_link));

    let broken = scratch.0.join("broken");
    unix_fs::symlink(scratch.0.join("nowhere"), &broken)?;
    assert!(!is_runnable(&broken));
    Ok(())
}

#[test]
fn a_fifo_is_not_runnable_and_does_not_block() -> TestResult {
    let scratch = Scratch::new()?;
    let fifo = scratch.0.join("fifo");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o755),
        0,
    )?;
    assert!(!is_runnable(&fifo));
    Ok(())
}

#[test]
fn only_a_desktop_file_name_is_a_launcher() {
    assert!(is_desktop_file(Path::new("/a/App.desktop")));
    assert!(is_desktop_file(Path::new("org.example.App.desktop")));
    assert!(!is_desktop_file(Path::new("/a/App.desktop.bak")));
    assert!(!is_desktop_file(Path::new("/a/desktop")));
    assert!(!is_desktop_file(Path::new("/a/notes.txt")));
    assert!(!is_desktop_file(Path::new("/a/.desktop")));
}

fn argv(plan: &fileflux::core::apps::DesktopLaunch) -> Vec<String> {
    plan.argv
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn launch_expands_the_exec_line_with_no_files() -> TestResult {
    let plan = desktop_launch(
        "[Desktop Entry]\nType=Application\nName=App\nExec=app --new %U --x=%f %i %%\n",
    )?;
    assert!(!plan.terminal);
    assert_eq!(argv(&plan), ["app", "--new", "--x=", "%"]);
    Ok(())
}

#[test]
fn launch_reports_terminal_true_and_refuses_unusable_entries() -> TestResult {
    let plan =
        desktop_launch("[Desktop Entry]\nType=Application\nName=T\nExec=htop\nTerminal=true\n")?;
    assert!(plan.terminal);
    assert_eq!(argv(&plan), ["htop"]);

    assert!(desktop_launch("[Desktop Entry]\nType=Link\nName=L\nURL=x\n").is_err());
    assert!(desktop_launch("not a desktop file").is_err());
    assert!(
        desktop_launch("[Desktop Entry]\nType=Application\nName=Q\nExec=sh -c \"echo %f\"\n")
            .is_err()
    );
    Ok(())
}

fn fake_terminal(scratch: &Scratch, name: &str) -> io::Result<PathBuf> {
    scratch.file(name, b"#!/bin/sh\n", 0o755)
}

#[test]
fn a_terminal_runs_a_program_with_its_own_exec_argument() -> TestResult {
    let scratch = Scratch::new()?;
    let program: Vec<OsString> = vec!["/opt/tool".into(), "arg one".into()];
    let folder = Path::new("/opt");
    let cases: [(&str, &[&str]); 5] = [
        ("kitty", &["--directory", "/opt", "/opt/tool", "arg one"]),
        (
            "alacritty",
            &["--working-directory", "/opt", "-e", "/opt/tool", "arg one"],
        ),
        (
            "gnome-terminal",
            &["--working-directory=/opt", "--", "/opt/tool", "arg one"],
        ),
        (
            "wezterm",
            &["start", "--cwd", "/opt", "--", "/opt/tool", "arg one"],
        ),
        ("someterm", &["-e", "/opt/tool", "arg one"]),
    ];
    for (name, expected) in cases {
        let env = TerminalEnv {
            terminal: Some(fake_terminal(&scratch, name)?.into_os_string()),
            ..TerminalEnv::default()
        };
        let terminal = resolve(&env).ok_or("no terminal")?;
        let command = terminal.command_running(folder, &program);
        let args: Vec<String> = command
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, expected, "{name}");
        assert_eq!(command.cwd, folder, "{name}");
        // Opening a folder without a program is untouched.
        assert!(
            !terminal
                .command(folder)
                .args
                .iter()
                .any(|arg| arg == "/opt/tool")
        );
    }
    Ok(())
}

#[test]
fn a_desktop_file_opens_with_the_default_text_application_when_it_is_installed() -> TestResult {
    let scratch = Scratch::new()?;
    let apps_dir = scratch.0.join("applications");
    fs::create_dir(&apps_dir)?;
    fs::write(
        apps_dir.join("editor.desktop"),
        "[Desktop Entry]\nType=Application\nName=Editor\nExec=editor %F\nMimeType=text/plain;\n",
    )?;
    let dirs = [apps_dir];

    let asked = std::cell::RefCell::new(Vec::new());
    let named = |mime: &str| {
        asked.borrow_mut().push(mime.to_owned());
        Some("editor.desktop".to_owned())
    };
    let app = fileflux::core::apps::default_text_app_in(&dirs, &named).ok_or("no editor")?;
    assert_eq!(
        (app.id.as_str(), app.name.as_str()),
        ("editor.desktop", "Editor")
    );
    assert_eq!(
        *asked.borrow(),
        ["text/plain"],
        "never the .desktop's own type"
    );

    // No default, or one that is not installed: the caller falls back to Open with….
    assert!(fileflux::core::apps::default_text_app_in(&dirs, &|_| None).is_none());
    assert!(
        fileflux::core::apps::default_text_app_in(&dirs, &|_| Some("gone.desktop".to_owned()))
            .is_none()
    );
    Ok(())
}
