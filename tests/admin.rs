//! "Open as Administrator": the pure parts. The launch command lines, the
//! environment allowlist, the polkit policy check, the claim-skip decision,
//! the menu offer, the refusals and the exit mapping. pkexec is never run
//! here.

use std::error::Error;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use fileflux::cli::{self, Command};
use fileflux::core::Location;
use fileflux::core::admin::{
    self, BANNER, Launch, LaunchError, NEW_INSTANCE_FLAG, Refusal, claims_session_name,
    exit_notice, launcher_of, policy_allows, policy_candidates, refusal, row_offered, window_title,
};

type TestResult = Result<(), Box<dyn Error>>;

fn os(text: &str) -> OsString {
    OsString::from(text)
}

fn launch(policy_installed: bool, display: Option<&str>, xauthority: Option<&str>) -> Launch {
    Launch {
        pkexec: PathBuf::from("/usr/bin/pkexec"),
        launcher: PathBuf::from("/opt/ff/bin/fileflux"),
        env: PathBuf::from("/usr/bin/env"),
        policy_installed,
        display: display.map(os),
        xauthority: xauthority.map(os),
    }
}

#[test]
fn with_the_policy_installed_pkexec_runs_the_launcher_with_no_environment() -> TestResult {
    let argv =
        launch(true, Some(":0"), Some("/home/u/.Xauthority")).argv(Path::new("/srv/data"))?;
    assert_eq!(
        argv,
        [
            "/usr/bin/pkexec",
            "/opt/ff/bin/fileflux",
            "--new-instance",
            "--",
            "/srv/data"
        ]
        .map(os)
    );
    Ok(())
}

#[test]
fn without_the_policy_only_display_and_xauthority_are_passed_through_env() -> TestResult {
    let argv = launch(false, Some(":1"), Some("/run/user/1000/xauth_x")).argv(Path::new("/srv"))?;
    assert_eq!(
        argv,
        [
            "/usr/bin/pkexec",
            "/usr/bin/env",
            "DISPLAY=:1",
            "XAUTHORITY=/run/user/1000/xauth_x",
            "/opt/ff/bin/fileflux",
            "--new-instance",
            "--",
            "/srv"
        ]
        .map(os)
    );
    Ok(())
}

#[test]
fn a_missing_or_empty_display_variable_is_skipped() -> TestResult {
    let only_display = launch(false, Some(":0"), None).argv(Path::new("/a"))?;
    assert_eq!(
        only_display,
        [
            "/usr/bin/pkexec",
            "/usr/bin/env",
            "DISPLAY=:0",
            "/opt/ff/bin/fileflux",
            "--new-instance",
            "--",
            "/a"
        ]
        .map(os)
    );
    let none = launch(false, None, Some("")).argv(Path::new("/a"))?;
    assert_eq!(
        none,
        [
            "/usr/bin/pkexec",
            "/usr/bin/env",
            "/opt/ff/bin/fileflux",
            "--new-instance",
            "--",
            "/a"
        ]
        .map(os)
    );
    Ok(())
}

#[test]
fn the_session_bus_wayland_and_runtime_dir_are_never_passed() -> TestResult {
    for policy in [true, false] {
        let argv = launch(policy, Some(":0"), Some("/x")).argv(Path::new("/a"))?;
        let text: Vec<String> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        for banned in [
            "DBUS_SESSION_BUS_ADDRESS",
            "WAYLAND_DISPLAY",
            "XDG_RUNTIME_DIR",
        ] {
            assert!(!text.iter().any(|arg| arg.contains(banned)), "{banned}");
        }
    }
    Ok(())
}

#[test]
fn a_folder_with_spaces_or_a_leading_dash_is_one_argument_after_the_separator() -> TestResult {
    for folder in ["/srv/my docs", "/srv/-rf", "/srv/a;b $(x)"] {
        let argv = launch(true, None, None).argv(Path::new(folder))?;
        assert_eq!(argv.last(), Some(&os(folder)));
        assert_eq!(argv.iter().rev().nth(1), Some(&os("--")));
    }
    // A relative path could be read as an option: refused.
    assert!(launch(true, None, None).argv(Path::new("-rf")).is_err());
    assert!(launch(true, None, None).argv(Path::new("rel")).is_err());
    Ok(())
}

#[test]
fn the_nix_wrapper_name_maps_back_to_the_wrapper() {
    assert_eq!(
        launcher_of(Path::new("/nix/store/x/bin/.fileflux-wrapped")),
        PathBuf::from("/nix/store/x/bin/fileflux")
    );
    assert_eq!(
        launcher_of(Path::new("/nix/store/x/bin/.fileflux-nightly-wrapped")),
        PathBuf::from("/nix/store/x/bin/fileflux-nightly")
    );
    assert_eq!(
        launcher_of(Path::new("/usr/bin/fileflux")),
        PathBuf::from("/usr/bin/fileflux")
    );
}

#[test]
fn the_policy_counts_only_when_it_names_this_launcher_and_allows_the_gui() {
    let policy = |path: &str, gui: &str| {
        format!(
            "<annotate key=\"org.freedesktop.policykit.exec.path\">{path}</annotate>\n\
             <annotate key=\"org.freedesktop.policykit.exec.allow_gui\">{gui}</annotate>"
        )
    };
    let launcher = Path::new("/opt/ff/bin/fileflux");
    assert!(policy_allows(
        &policy("/opt/ff/bin/fileflux", "true"),
        launcher
    ));
    assert!(!policy_allows(
        &policy("/usr/bin/fileflux", "true"),
        launcher
    ));
    assert!(!policy_allows(
        &policy("/opt/ff/bin/fileflux", "false"),
        launcher
    ));
    assert!(!policy_allows("", launcher));
}

#[test]
fn a_channel_launcher_looks_for_its_own_policy_file() {
    let found = policy_candidates(Path::new("/nix/store/x/bin/fileflux-nightly"));
    assert!(!found.is_empty());
    for candidate in &found {
        assert_eq!(
            candidate.file_name().and_then(|name| name.to_str()),
            Some("dev.fileflux.open-as-admin-nightly.policy"),
            "{candidate:?}"
        );
    }
    // The wrapped Nix name and a name that is no channel.
    assert_eq!(
        policy_candidates(Path::new("/x/bin/.fileflux-nightly-wrapped")),
        found
    );
    for plain in ["/x/bin/fileflux", "/x/bin/other", "/x/bin/fileflux-.."] {
        let files: Vec<_> = policy_candidates(Path::new(plain))
            .iter()
            .filter_map(|c| c.file_name().map(ToOwned::to_owned))
            .collect();
        assert!(
            files
                .iter()
                .all(|f| f == "dev.fileflux.open-as-admin.policy"),
            "{plain}: {files:?}"
        );
    }
}

#[test]
fn the_policy_is_looked_for_only_where_polkitd_reads_actions() {
    let found = policy_candidates(Path::new("/nix/store/x/bin/fileflux"));
    assert!(
        found.contains(&PathBuf::from(
            "/usr/share/polkit-1/actions/dev.fileflux.open-as-admin.policy"
        )),
        "{found:?}"
    );
    assert!(
        found.contains(&PathBuf::from(
            "/run/current-system/sw/share/polkit-1/actions/dev.fileflux.open-as-admin.policy"
        )),
        "{found:?}"
    );
    for candidate in &found {
        let text = candidate.to_string_lossy();
        assert!(!text.starts_with("/nix/store"), "{text}");
        assert!(!text.contains(".."), "{text}");
        assert!(!text.contains("/bin/"), "{text}");
        assert!(text.contains("polkit-1/actions"), "{text}");
    }
}

#[test]
fn the_root_title_and_the_banner_text() {
    assert_eq!(
        window_title("Docs — Fileflux", true),
        "Docs — Fileflux (root)"
    );
    assert_eq!(window_title("Docs — Fileflux", false), "Docs — Fileflux");
    assert_eq!(
        BANNER,
        "Running as administrator — changes affect the whole system"
    );
}

#[test]
fn the_single_instance_name_is_claimed_only_by_a_normal_user_launch() {
    assert!(claims_session_name(false, false));
    assert!(!claims_session_name(true, false), "--new-instance");
    assert!(!claims_session_name(false, true), "uid 0");
    assert!(!claims_session_name(true, true));
}

#[test]
fn new_instance_is_a_flag_not_a_path() -> TestResult {
    let args = |list: &[&str]| list.iter().map(|a| os(a)).collect::<Vec<_>>();
    let (command, new_instance) =
        cli::parse_with_flags(args(&[NEW_INSTANCE_FLAG, "--", "/srv/-odd"]))?;
    assert!(new_instance);
    assert_eq!(command, Command::Open(vec![PathBuf::from("/srv/-odd")]));
    let (command, new_instance) = cli::parse_with_flags(args(&["/srv"]))?;
    assert!(!new_instance);
    assert_eq!(command, Command::Open(vec![PathBuf::from("/srv")]));
    assert!(cli::USAGE.contains(NEW_INSTANCE_FLAG));
    Ok(())
}

#[test]
fn the_row_is_offered_on_one_folder_or_the_empty_space_of_a_directory_unless_root() {
    let here = Location::Directory(PathBuf::from("/srv"));
    let folder = [(PathBuf::from("/srv/a"), true)];
    let file = [(PathBuf::from("/srv/f"), false)];
    let two = [
        (PathBuf::from("/srv/a"), true),
        (PathBuf::from("/srv/b"), true),
    ];
    assert!(row_offered(false, &here, &folder));
    assert!(row_offered(false, &here, &[]));
    assert!(!row_offered(false, &here, &file));
    assert!(!row_offered(false, &here, &two));
    assert!(!row_offered(true, &here, &folder), "hidden as root");
    assert!(!row_offered(true, &here, &[]), "hidden as root");
    for other in [
        Location::Trash,
        Location::Recent,
        Location::Search {
            root: PathBuf::from("/srv"),
            query: "x".into(),
        },
        Location::Usage {
            root: PathBuf::from("/srv"),
        },
    ] {
        assert!(!row_offered(false, &other, &[]), "{other:?}");
    }
}

#[test]
fn non_local_places_are_refused_with_a_reason() {
    let remote = |path: &Path| path.starts_with("/run/user/1000/gvfs");
    assert_eq!(refusal(&Location::Trash, remote), Some(Refusal::NotAFolder));
    assert_eq!(
        refusal(&Location::Recent, remote),
        Some(Refusal::NotAFolder)
    );
    assert_eq!(
        refusal(
            &Location::Directory(PathBuf::from("/run/user/1000/gvfs/mtp:host=x")),
            remote
        ),
        Some(Refusal::Remote)
    );
    assert_eq!(
        refusal(&Location::Directory(PathBuf::from("/srv")), remote),
        None
    );
    assert!(
        Refusal::Remote
            .to_string()
            .contains("Open as Administrator")
    );
}

#[test]
fn an_exit_notice_is_neutral_about_who_ended() {
    assert_eq!(exit_notice(Some(0)), None);
    assert_eq!(exit_notice(None), None);
    for code in [126, 127, 3] {
        let notice = exit_notice(Some(code)).unwrap_or_default();
        assert!(
            notice.starts_with("The administrator instance could not start"),
            "{notice}"
        );
        assert!(notice.contains(&format!("exit {code}")), "{notice}");
    }
}

#[test]
fn a_launcher_with_an_equals_sign_is_refused_for_the_env_fallback_only() {
    let mut odd = launch(false, Some(":0"), None);
    odd.launcher = PathBuf::from("/opt/a=b/bin/fileflux");
    assert_eq!(
        odd.argv(Path::new("/srv")),
        Err(LaunchError::LauncherHasEquals)
    );
    assert!(LaunchError::LauncherHasEquals.to_string().contains('='));
    odd.policy_installed = true;
    assert!(odd.argv(Path::new("/srv")).is_ok());
}

#[test]
fn a_policy_path_is_compared_as_the_exact_string() {
    let policy = |path: &str| {
        format!(
            "<annotate key=\"org.freedesktop.policykit.exec.path\">{path}</annotate>\
             <annotate key=\"org.freedesktop.policykit.exec.allow_gui\">true</annotate>"
        )
    };
    let launcher = Path::new("/opt/ff/bin/fileflux");
    assert!(!policy_allows(&policy("/opt/ff/bin/./fileflux"), launcher));
    assert!(!policy_allows(&policy("/opt/ff/bin//fileflux"), launcher));
    assert!(!policy_allows(&policy(" /opt/ff/bin/fileflux"), launcher));
    assert!(policy_allows(&policy("/opt/ff/bin/fileflux"), launcher));
}

#[test]
fn only_real_remote_mounts_are_remote_and_local_fuseblk_is_not() {
    let table = b"/dev/sda1 / ext4 rw 0 0\n\
/dev/sdb1 /mnt/ntfs fuseblk rw 0 0\n\
me@host:/ /mnt/remote fuse.sshfs rw 0 0\n\
x /mnt/my\\040share fuse rw 0 0\n\
gvfsd-fuse /run/user/1000/gvfs fuse.gvfsd-fuse rw 0 0\n";
    let remote = |path: &str| admin::remote_in(table, Path::new(path));
    assert!(!remote("/home/me"));
    assert!(!remote("/mnt/ntfs/dir"));
    assert!(remote("/mnt/remote/dir"));
    assert!(remote("/mnt/my share/x"));
    assert!(remote("/run/user/1000/gvfs/mtp:host=x/DCIM"));
}

#[test]
fn launch_errors_say_what_is_missing() {
    assert!(LaunchError::Pkexec.to_string().contains("pkexec"));
    let _ = admin::NEW_INSTANCE_FLAG;
}
