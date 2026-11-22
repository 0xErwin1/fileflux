//! Probes of the launch command line against hostile folder names, and of
//! the policy check against the shipped policy file.

#![allow(clippy::indexing_slicing)]

use std::error::Error;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use fileflux::cli::{self, Command};
use fileflux::core::admin::{Launch, LaunchError, policy_allows};

type TestResult = Result<(), Box<dyn Error>>;

fn os(text: &str) -> OsString {
    OsString::from(text)
}

fn launch(policy_installed: bool) -> Launch {
    Launch {
        pkexec: PathBuf::from("/usr/bin/pkexec"),
        launcher: PathBuf::from("/opt/ff/bin/fileflux"),
        env: PathBuf::from("/usr/bin/env"),
        policy_installed,
        display: Some(os(":0")),
        xauthority: Some(os("/run/user/1000/xauth")),
    }
}

#[test]
fn the_tail_of_either_command_line_parses_back_to_the_folder() -> TestResult {
    let base = std::env::temp_dir().join("fileflux-admin-probes");
    for name in [
        "-rf",
        "--new-instance",
        "--",
        "-h",
        "a b",
        "x\ny",
        "FOO=bar",
    ] {
        let folder = base.join(name);
        // A name starting with `-` is only dangerous as a bare argument:
        // also probe the relative-looking forms under the root.
        for folder in [folder, PathBuf::from(format!("/{name}"))] {
            for policy in [true, false] {
                let argv = launch(policy).argv(&folder)?;
                let tail = &argv[argv.len() - 4..];
                assert_eq!(
                    tail,
                    [
                        os("/opt/ff/bin/fileflux"),
                        os("--new-instance"),
                        os("--"),
                        folder.clone().into_os_string()
                    ],
                    "{name} policy={policy}"
                );
                let middle = &argv[1..argv.len() - 4];
                if policy {
                    assert!(middle.is_empty(), "{middle:?}");
                } else {
                    assert_eq!(
                        middle,
                        [
                            os("/usr/bin/env"),
                            os("DISPLAY=:0"),
                            os("XAUTHORITY=/run/user/1000/xauth")
                        ]
                    );
                }
                let parsed = cli::parse_with_flags(tail[1..].iter().cloned())?;
                assert_eq!(
                    parsed,
                    (Command::Open(vec![folder.clone()]), true),
                    "{name}"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn a_folder_that_is_not_absolute_is_refused() {
    for folder in ["", "rel", "./x", "-rf"] {
        for policy in [true, false] {
            assert_eq!(
                launch(policy).argv(Path::new(folder)),
                Err(LaunchError::RelativeFolder),
                "{folder:?}"
            );
        }
    }
}

#[test]
fn the_shipped_policy_is_accepted_only_for_its_own_launcher_with_the_gui() -> TestResult {
    let shipped = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging/dev.fileflux.open-as-admin.policy"),
    )?;
    let launcher = Path::new("/usr/bin/fileflux");
    assert!(policy_allows(&shipped, launcher));
    assert!(!policy_allows(&shipped, Path::new("/usr/bin/other")));
    let no_gui = shipped.replace("exec.allow_gui\">true", "exec.allow_gui\">false");
    assert_ne!(no_gui, shipped);
    assert!(!policy_allows(&no_gui, launcher));
    assert!(!policy_allows("", launcher));
    Ok(())
}
