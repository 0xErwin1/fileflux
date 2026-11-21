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

/// A scratch directory under the system temp dir, removed when dropped so
/// test runs leave nothing behind.
struct Scratch(std::path::PathBuf);

impl std::ops::Deref for Scratch {
    type Target = std::path::Path;

    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

impl AsRef<std::path::Path> for Scratch {
    fn as_ref(&self) -> &std::path::Path {
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

fn scratch(name: &str) -> Scratch {
    let dir = std::env::temp_dir().join(format!("fileflux-cli-{name}-{}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("stale scratch directory is removed");
    }
    std::fs::create_dir_all(&dir).expect("scratch directory is created");
    Scratch(dir)
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

#[test]
fn help_describes_the_image_viewer() {
    assert!(fileflux::cli::USAGE.contains("image viewer"));
    assert!(fileflux::cli::USAGE.contains("IMAGE..."));
}

#[test]
fn several_paths_without_an_image_are_still_rejected() {
    let cwd = scratch("many-files");
    std::fs::write(cwd.join("notes.txt"), b"x").expect("fixture file is written");
    let out = fileflux(&["notes.txt", "other.txt"], &cwd);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(text(&out.stdout), "");
    assert_eq!(
        text(&out.stderr),
        "fileflux: expected at most one path\nTry 'fileflux --help' for more information.\n"
    );
}

#[test]
fn a_missing_image_path_is_rejected_without_opening_a_window() {
    let cwd = scratch("missing-image");
    let out = fileflux(&["gone.png"], &cwd);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(text(&out.stdout), "");
    assert_eq!(
        text(&out.stderr),
        format!(
            "fileflux: '{}' does not exist\n",
            cwd.join("gone.png").display()
        )
    );
}

/// The desktop entry must claim every image type the viewer opens, and the
/// directory type so file managers can offer Fileflux.
#[test]
fn the_desktop_entry_lists_every_image_mime_type() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let entry = std::fs::read_to_string(root.join("packaging/fileflux.desktop"))
        .expect("the desktop entry exists");
    let mime_line = entry
        .lines()
        .find_map(|line| line.strip_prefix("MimeType="))
        .expect("the desktop entry has a MimeType line");
    let claimed: Vec<&str> = mime_line.split(';').filter(|m| !m.is_empty()).collect();
    for mime in fileflux::core::images::IMAGE_MIME_TYPES {
        assert!(claimed.contains(mime), "{mime} missing from MimeType");
    }
    assert!(claimed.contains(&"inode/directory"));
    assert!(entry.lines().any(|line| line == "Exec=fileflux %F"));
}

#[test]
fn a_scratch_directory_is_removed_when_the_test_is_done() {
    let path = {
        let dir = scratch("cleanup");
        std::fs::write(dir.join("left-over"), b"x").expect("a file is written");
        dir.to_path_buf()
    };
    assert!(!path.exists(), "{} was left behind", path.display());
}
