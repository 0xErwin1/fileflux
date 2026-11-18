//! Command-line behavior that ends before any window opens: help, version
//! and every rejected start path. Opening a window is covered by the native
//! smoke helper, not here.

#![allow(clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Output};

fn fileflux(args: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fileflux"))
        .args(args)
        .current_dir(cwd)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .expect("the fileflux binary runs")
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("fileflux-cli-{name}-{}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("stale scratch directory is removed");
    }
    std::fs::create_dir_all(&dir).expect("scratch directory is created");
    dir
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn help_prints_usage_and_succeeds() {
    let cwd = scratch("help");
    for flag in ["--help", "-h"] {
        let out = fileflux(&[flag], &cwd);
        assert_eq!(out.status.code(), Some(0), "{flag}");
        assert_eq!(text(&out.stdout), fileflux::cli::USAGE, "{flag}");
        assert_eq!(text(&out.stderr), "", "{flag}");
    }
}

#[test]
fn version_prints_package_version() {
    let cwd = scratch("version");
    for flag in ["--version", "-V"] {
        let out = fileflux(&[flag], &cwd);
        assert_eq!(out.status.code(), Some(0), "{flag}");
        assert_eq!(
            text(&out.stdout),
            format!("fileflux {}\n", env!("CARGO_PKG_VERSION")),
            "{flag}"
        );
        assert_eq!(text(&out.stderr), "", "{flag}");
    }
}

#[test]
fn missing_path_is_rejected_without_opening_a_window() {
    let cwd = scratch("missing");
    let out = fileflux(&["does-not-exist"], &cwd);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(text(&out.stdout), "");
    assert_eq!(
        text(&out.stderr),
        format!(
            "fileflux: '{}' does not exist\n",
            cwd.join("does-not-exist").display()
        )
    );
}

#[test]
fn unknown_option_is_rejected() {
    let cwd = scratch("unknown");
    let out = fileflux(&["--bogus"], &cwd);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(text(&out.stdout), "");
    assert_eq!(
        text(&out.stderr),
        "fileflux: unknown option '--bogus'\nTry 'fileflux --help' for more information.\n"
    );
}

#[test]
fn more_than_one_path_is_rejected() {
    let cwd = scratch("many");
    let out = fileflux(&["a", "b"], &cwd);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(text(&out.stdout), "");
    assert_eq!(
        text(&out.stderr),
        "fileflux: expected at most one path\nTry 'fileflux --help' for more information.\n"
    );
}
