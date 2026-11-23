//! The pure parts of the owner change (`pkexec chown`): the command line,
//! the lookup of the two programs, the exit code to outcome mapping and the
//! identity re-check. pkexec is never run here: the runner is a fake.

use std::cell::RefCell;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use fileflux::core::elevate::{
    CHOWN_CANDIDATES, Elevation, Finished, Identity, Outcome, PKEXEC_CANDIDATES, Unavailable,
    change_owner, helper_beside, helper_outcome_of, outcome_of, resolve,
};
use fileflux::core::owner_helper::{Born, Exit, HELPER_NAME};

type TestResult = Result<(), Box<dyn Error>>;

fn elevation() -> Elevation {
    Elevation {
        pkexec: PathBuf::from("/usr/bin/pkexec"),
        chown: PathBuf::from("/usr/bin/chown"),
        helper: None,
    }
}

fn with_helper() -> Elevation {
    Elevation {
        helper: Some(PathBuf::from("/opt/ff/bin/fileflux-owner-helper")),
        ..elevation()
    }
}

fn os(text: &str) -> OsString {
    OsString::from(text)
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Result<Self, Box<dyn Error>> {
        let dir =
            std::env::temp_dir().join(format!("fileflux-elevate-{tag}-{}", std::process::id()));
        drop(fs::remove_dir_all(&dir));
        fs::create_dir_all(&dir)?;
        Ok(Self(fs::canonicalize(dir)?))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.0));
    }
}

// --- the command line ------------------------------------------------------------

#[test]
fn the_argv_is_pkexec_chown_no_dereference_from_uid_dashes_path() -> TestResult {
    let argv = elevation().argv(500, 1000, Path::new("/home/ana/report.txt"))?;
    assert_eq!(
        argv,
        vec![
            os("/usr/bin/pkexec"),
            os("/usr/bin/chown"),
            os("--no-dereference"),
            os("--from=+500"),
            os("+1000"),
            os("--"),
            os("/home/ana/report.txt"),
        ]
    );
    Ok(())
}

#[test]
fn the_uid_is_exact_and_numeric_even_at_the_extremes() -> TestResult {
    let argv = elevation().argv(0, 0, Path::new("/a"))?;
    assert_eq!(argv.get(3), Some(&os("--from=+0")));
    assert_eq!(argv.get(4), Some(&os("+0")));
    let argv = elevation().argv(4_294_967_293, 4_294_967_294, Path::new("/a"))?;
    assert_eq!(argv.get(3), Some(&os("--from=+4294967293")));
    assert_eq!(argv.get(4), Some(&os("+4294967294")));
    Ok(())
}

#[test]
fn paths_with_dashes_spaces_newlines_and_shell_syntax_stay_one_argument() -> TestResult {
    for name in [
        "/tmp/-rf",
        "/tmp/--reference=/etc/passwd",
        "/tmp/with space",
        "/tmp/line\nbreak",
        "/tmp/$(touch pwned);`x`|&*",
        "/tmp/--",
    ] {
        let argv = elevation().argv(3, 7, Path::new(name))?;
        assert_eq!(argv.len(), 7, "{name:?}");
        assert_eq!(
            argv.get(5),
            Some(&os("--")),
            "the separator precedes the path"
        );
        assert_eq!(argv.get(6), Some(&os(name)), "{name:?} is passed untouched");
    }
    Ok(())
}

#[test]
fn a_non_utf8_path_is_passed_byte_for_byte() -> TestResult {
    let path = PathBuf::from(OsStr::from_bytes(b"/tmp/caf\xe9"));
    let argv = elevation().argv(3, 7, &path)?;
    assert_eq!(
        argv.get(6).map(|arg| arg.as_bytes()),
        Some(&b"/tmp/caf\xe9"[..])
    );
    Ok(())
}

#[test]
fn a_relative_path_is_refused_because_pkexec_does_not_run_in_our_folder() {
    assert!(elevation().argv(3, 7, Path::new("report.txt")).is_err());
    assert!(elevation().argv(3, 7, Path::new("-rf")).is_err());
    assert!(elevation().argv(3, 7, Path::new("")).is_err());
}

// --- finding the programs ---------------------------------------------------------

#[test]
fn the_fixed_candidate_lists_are_absolute_and_start_with_the_nixos_wrapper() {
    assert_eq!(PKEXEC_CANDIDATES.first(), Some(&"/run/wrappers/bin/pkexec"));
    assert!(PKEXEC_CANDIDATES.contains(&"/usr/bin/pkexec"));
    assert!(PKEXEC_CANDIDATES.contains(&"/bin/pkexec"));
    for candidate in PKEXEC_CANDIDATES.iter().chain(CHOWN_CANDIDATES) {
        assert!(Path::new(candidate).is_absolute(), "{candidate}");
    }
}

#[test]
fn resolve_takes_the_first_candidate_that_passes_and_never_searches_path() {
    let found = resolve(&["/a/pkexec", "/b/pkexec", "/c/pkexec"], |path| {
        path != Path::new("/a/pkexec")
    });
    assert_eq!(found, Some(PathBuf::from("/b/pkexec")));
    assert_eq!(resolve(&["/a/pkexec"], |_| false), None);
    assert_eq!(resolve(&[], |_| true), None);
}

#[test]
fn elevation_needs_both_programs_and_says_which_one_is_missing() {
    let only = |wanted: &'static str| {
        move |path: &Path| path.file_name().is_some_and(|name| name == wanted)
    };
    assert_eq!(
        Elevation::detect_with(only("chown")).err(),
        Some(Unavailable::Pkexec)
    );
    assert_eq!(
        Elevation::detect_with(only("pkexec")).err(),
        Some(Unavailable::Chown)
    );
    let both = Elevation::detect_with(|_| true);
    assert_eq!(
        both.ok(),
        Some(Elevation {
            pkexec: PathBuf::from("/run/wrappers/bin/pkexec"),
            chown: PathBuf::from("/run/current-system/sw/bin/chown"),
            helper: None,
        })
    );
}

#[test]
fn the_unavailable_reason_is_the_tooltip_text() {
    let text = Unavailable::Pkexec.to_string();
    assert!(text.contains("pkexec"), "{text}");
    assert!(text.contains("polkit"), "{text}");
    assert!(Unavailable::Chown.to_string().contains("chown"));
}

// --- exit codes --------------------------------------------------------------------

#[test]
fn exit_zero_is_a_change() {
    assert_eq!(outcome_of(Some(0), ""), Outcome::Changed);
}

#[test]
fn pkexec_126_and_127_are_cancellations_with_a_reason() {
    for code in [126, 127] {
        let outcome = outcome_of(Some(code), "");
        assert!(
            matches!(&outcome, Outcome::Cancelled(reason) if reason.contains("not changed")),
            "{code}: {outcome:?}"
        );
    }
    assert_ne!(outcome_of(Some(126), ""), outcome_of(Some(127), ""));
}

#[test]
fn other_codes_and_signals_fail_with_the_programs_own_words() {
    let failed = outcome_of(
        Some(1),
        "chown: changing ownership: Operation not permitted\n",
    );
    assert!(
        matches!(&failed, Outcome::Failed(reason)
            if reason.contains("exit 1") && reason.contains("Operation not permitted")),
        "{failed:?}"
    );
    let signalled = outcome_of(None, "");
    assert!(
        matches!(&signalled, Outcome::Failed(reason) if reason.contains("signal")),
        "{signalled:?}"
    );
    assert!(matches!(outcome_of(Some(2), ""), Outcome::Failed(_)));
}

#[test]
fn a_huge_stderr_is_cut_before_it_reaches_a_toast() {
    let long = "x".repeat(10_000);
    let outcome = outcome_of(Some(1), &long);
    assert!(
        matches!(&outcome, Outcome::Failed(reason) if reason.len() < 600),
        "{outcome:?}"
    );
}

// --- the identity re-check ---------------------------------------------------------

#[test]
fn the_same_file_matches_and_a_replacement_does_not() -> TestResult {
    let scratch = Scratch::new("identity")?;
    let path = scratch.0.join("a");
    fs::write(&path, "one")?;
    let seen = Identity::of(&path)?;
    assert!(seen.check(&Identity::of(&path)?).is_ok());

    // A different file under the same name (the original is kept alive so
    // the inode number cannot be reused).
    let moved = scratch.0.join("moved");
    fs::rename(&path, &moved)?;
    fs::write(&path, "two")?;
    assert!(seen.check(&Identity::of(&path)?).is_err());
    Ok(())
}

#[test]
fn a_symbolic_link_put_in_the_place_is_refused() -> TestResult {
    let scratch = Scratch::new("symlink")?;
    let path = scratch.0.join("a");
    let target = scratch.0.join("target");
    fs::write(&path, "one")?;
    fs::write(&target, "two")?;
    let seen = Identity::of(&path)?;
    fs::remove_file(&path)?;
    std::os::unix::fs::symlink(&target, &path)?;
    assert!(seen.check(&Identity::of(&path)?).is_err());
    Ok(())
}

#[test]
fn a_birth_time_difference_alone_refuses() {
    let base = Identity {
        dev: 1,
        ino: 2,
        born: Some(std::time::UNIX_EPOCH),
        owner: 1,
        symlink: false,
    };
    let reborn = Identity {
        born: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(5)),
        ..base.clone()
    };
    assert!(base.check(&base.clone()).is_ok());
    assert!(base.check(&reborn).is_err());
    let other_dev = Identity {
        dev: 9,
        ..base.clone()
    };
    let other_ino = Identity {
        ino: 9,
        ..base.clone()
    };
    assert!(base.check(&other_dev).is_err());
    assert!(base.check(&other_ino).is_err());
}

// --- the whole change, with a fake runner -------------------------------------------

#[test]
fn a_swapped_path_is_refused_and_nothing_is_run() -> TestResult {
    let scratch = Scratch::new("swapped")?;
    let path = scratch.0.join("a");
    fs::write(&path, "one")?;
    let seen = Identity::of(&path)?;
    fs::rename(&path, scratch.0.join("kept"))?;
    fs::write(&path, "other")?;

    let ran = RefCell::new(false);
    let outcome = change_owner(&elevation(), 1000, &path, &seen, |_| {
        *ran.borrow_mut() = true;
        Ok(Finished {
            code: Some(0),
            stderr: String::new(),
        })
    });
    assert!(matches!(outcome, Outcome::Failed(ref reason) if reason.contains("changed")));
    assert!(!*ran.borrow(), "pkexec must not start for a swapped path");
    Ok(())
}

#[test]
fn a_missing_path_is_refused_and_nothing_is_run() -> TestResult {
    let scratch = Scratch::new("missing")?;
    let path = scratch.0.join("a");
    fs::write(&path, "one")?;
    let seen = Identity::of(&path)?;
    fs::remove_file(&path)?;
    let outcome = change_owner(&elevation(), 1000, &path, &seen, |_| {
        Err(io::Error::other("runner reached"))
    });
    assert!(
        matches!(&outcome, Outcome::Failed(reason) if !reason.contains("runner reached")),
        "the runner must not be reached: {outcome:?}"
    );
    Ok(())
}

#[test]
fn the_same_file_runs_the_exact_argv_once_and_maps_the_exit_code() -> TestResult {
    let scratch = Scratch::new("run")?;
    let path = scratch.0.join("-odd name\n");
    fs::write(&path, "one")?;
    let seen = Identity::of(&path)?;

    // The fake runner does not chown, so the requested uid is the owner the
    // file already has: exit 0 then is a real change as far as the re-read sees.
    let uid = seen.owner;
    for (code, expect) in [
        (Some(0), "changed"),
        (Some(126), "cancelled"),
        (Some(127), "cancelled"),
        (Some(1), "failed"),
    ] {
        let calls = RefCell::new(Vec::<Vec<OsString>>::new());
        let outcome = change_owner(&elevation(), uid, &path, &seen, |argv| {
            calls.borrow_mut().push(argv.to_vec());
            Ok(Finished {
                code,
                stderr: "boom".to_owned(),
            })
        });
        let kind = match outcome {
            Outcome::Changed => "changed",
            Outcome::Cancelled(_) => "cancelled",
            Outcome::Failed(_) => "failed",
        };
        assert_eq!(kind, expect, "exit {code:?}");
        let calls = calls.into_inner();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls.first(),
            Some(&vec![
                os("/usr/bin/pkexec"),
                os("/usr/bin/chown"),
                os("--no-dereference"),
                os(&format!("--from=+{}", seen.owner)),
                os(&format!("+{uid}")),
                os("--"),
                path.clone().into_os_string(),
            ])
        );
    }
    Ok(())
}

#[test]
fn a_runner_that_cannot_start_is_a_failure_with_the_reason() -> TestResult {
    let scratch = Scratch::new("spawn")?;
    let path = scratch.0.join("a");
    fs::write(&path, "one")?;
    let seen = Identity::of(&path)?;
    let outcome = change_owner(&elevation(), 1, &path, &seen, |_| {
        Err(io::Error::new(io::ErrorKind::NotFound, "no such program"))
    });
    assert!(
        matches!(&outcome, Outcome::Failed(reason) if reason.contains("no such program")),
        "{outcome:?}"
    );
    Ok(())
}

#[test]
fn the_owner_the_panel_saw_is_read_but_is_not_part_of_the_identity_check() -> TestResult {
    use std::os::unix::fs::MetadataExt as _;
    let scratch = Scratch::new("owner")?;
    let path = scratch.0.join("a");
    fs::write(&path, "one")?;
    let seen = Identity::of(&path)?;
    assert_eq!(seen.owner, fs::symlink_metadata(&path)?.uid());
    let elsewhere = Identity {
        owner: seen.owner.wrapping_add(1),
        ..seen.clone()
    };
    assert!(seen.check(&elsewhere).is_ok(), "the owner may change");
    Ok(())
}

#[test]
fn a_file_replaced_during_the_authorization_is_reported_after_exit_zero() -> TestResult {
    let scratch = Scratch::new("postswap")?;
    let path = scratch.0.join("a");
    fs::write(&path, "one")?;
    let seen = Identity::of(&path)?;
    let outcome = change_owner(&elevation(), 1000, &path, &seen, |_| {
        fs::rename(&path, scratch.0.join("kept"))?;
        fs::write(&path, "new")?;
        Ok(Finished {
            code: Some(0),
            stderr: String::new(),
        })
    });
    let shown = path.display().to_string();
    assert!(
        matches!(&outcome, Outcome::Failed(r)
            if r.contains("ran") && r.contains("replaced") && r.contains(&shown)),
        "{outcome:?}"
    );
    Ok(())
}

#[test]
fn a_path_removed_or_made_a_symlink_during_the_authorization_is_reported() -> TestResult {
    let scratch = Scratch::new("postgone")?;
    let path = scratch.0.join("a");
    fs::write(&path, "one")?;
    let seen = Identity::of(&path)?;
    let outcome = change_owner(&elevation(), 1000, &path, &seen, |_| {
        fs::remove_file(&path)?;
        std::os::unix::fs::symlink("/", &path)?;
        Ok(Finished {
            code: Some(0),
            stderr: String::new(),
        })
    });
    assert!(
        matches!(&outcome, Outcome::Failed(r) if r.contains("replaced")),
        "{outcome:?}"
    );
    Ok(())
}

#[test]
fn exit_zero_without_the_owner_changing_is_a_failure_naming_the_owner_found() -> TestResult {
    // `chown --from=+<shown>` exits 0 and does nothing when the current owner
    // differs; the runner here also changes nothing.
    let scratch = Scratch::new("noop")?;
    let path = scratch.0.join("a");
    fs::write(&path, "one")?;
    let seen = Identity::of(&path)?;
    let other = seen.owner.wrapping_add(1);
    let outcome = change_owner(&elevation(), other, &path, &seen, |_| {
        Ok(Finished {
            code: Some(0),
            stderr: String::new(),
        })
    });
    let found = seen.owner.to_string();
    assert!(
        matches!(&outcome, Outcome::Failed(r)
            if r.contains(&found) && r.contains("not the one chosen") && r.contains("changed elsewhere")),
        "{outcome:?}"
    );
    Ok(())
}

#[test]
fn a_failed_exit_is_not_second_guessed_by_the_post_check() -> TestResult {
    let scratch = Scratch::new("postfail")?;
    let path = scratch.0.join("a");
    fs::write(&path, "one")?;
    let seen = Identity::of(&path)?;
    let outcome = change_owner(&elevation(), 1000, &path, &seen, |_| {
        fs::remove_file(&path)?;
        Ok(Finished {
            code: Some(1),
            stderr: "boom".to_owned(),
        })
    });
    assert!(
        matches!(&outcome, Outcome::Failed(r) if r.contains("exit 1")),
        "{outcome:?}"
    );
    Ok(())
}

#[test]
fn a_parent_folder_swapped_for_a_symlink_before_the_check_is_refused() -> TestResult {
    let scratch = Scratch::new("parent")?;
    let dir = scratch.0.join("d");
    let other = scratch.0.join("other");
    fs::create_dir_all(&dir)?;
    fs::create_dir_all(&other)?;
    let path = dir.join("f");
    fs::write(&path, "one")?;
    fs::write(other.join("f"), "two")?;
    let seen = Identity::of(&path)?;
    fs::rename(&dir, scratch.0.join("kept"))?;
    std::os::unix::fs::symlink(&other, &dir)?;
    let outcome = change_owner(&elevation(), 1, &path, &seen, |_| {
        Err(io::Error::other("runner reached"))
    });
    assert!(
        matches!(&outcome, Outcome::Failed(r) if r.contains("changed")),
        "{outcome:?}"
    );
    Ok(())
}

// --- the root helper -------------------------------------------------------------------

fn identity(born: Option<SystemTime>) -> Identity {
    Identity {
        dev: 2049,
        ino: 77,
        born,
        owner: 1000,
        symlink: false,
    }
}

#[test]
fn the_helper_is_looked_for_next_to_the_executable_only() -> TestResult {
    let canon = |dir: &Path| {
        dir.strip_prefix("/link")
            .map(|rest| PathBuf::from("/real").join(rest))
            .map_err(io::Error::other)
    };
    let found = helper_beside(Path::new("/link/bin/fileflux"), canon, |path| {
        path == Path::new("/real/bin").join(HELPER_NAME)
    });
    assert_eq!(found, Some(PathBuf::from("/real/bin").join(HELPER_NAME)));
    // Missing next to it: no search anywhere else.
    let asked = RefCell::new(Vec::new());
    let found = helper_beside(Path::new("/link/bin/fileflux"), canon, |path| {
        asked.borrow_mut().push(path.to_path_buf());
        false
    });
    assert_eq!(found, None);
    assert_eq!(
        *asked.borrow(),
        vec![PathBuf::from("/real/bin").join(HELPER_NAME)]
    );
    // A directory that cannot be canonicalized, or no parent: none.
    let broken = |_: &Path| Err(io::Error::other("gone"));
    assert_eq!(
        helper_beside(Path::new("/x/fileflux"), broken, |_| true),
        None
    );
    assert_eq!(helper_beside(Path::new("/"), canon, |_| true), None);
    Ok(())
}

#[test]
fn the_helper_argv_is_pkexec_helper_path_snapshot_owner_uid() -> TestResult {
    let born = SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 5);
    let argv = with_helper().command(&identity(Some(born)), 0, Path::new("/home/ana/a b.txt"))?;
    assert_eq!(
        argv,
        vec![
            os("/usr/bin/pkexec"),
            os("/opt/ff/bin/fileflux-owner-helper"),
            os("/home/ana/a b.txt"),
            os("2049"),
            os("77"),
            os("1700000000:5"),
            os("1000"),
            os("0"),
        ]
    );
    let argv = with_helper().command(&identity(None), 1001, Path::new("/a"))?;
    assert_eq!(argv.get(5), Some(&os("none")));
    assert_eq!(argv.get(7), Some(&os("1001")));
    assert!(
        with_helper()
            .command(&identity(None), 1, Path::new("rel"))
            .is_err()
    );
    Ok(())
}

#[test]
fn without_the_helper_the_command_is_the_chown_fallback() -> TestResult {
    let argv = elevation().command(&identity(None), 1000, Path::new("/a"))?;
    assert_eq!(argv, elevation().argv(1000, 1000, Path::new("/a"))?);
    Ok(())
}

#[test]
fn a_birth_time_converts_like_statx_does_including_before_the_epoch() {
    let epoch = SystemTime::UNIX_EPOCH;
    assert_eq!(
        Born::from_system_time(epoch + Duration::new(7, 9)),
        Born { secs: 7, nanos: 9 }
    );
    assert_eq!(
        Born::from_system_time(epoch - Duration::new(5, 0)),
        Born { secs: -5, nanos: 0 }
    );
    assert_eq!(
        Born::from_system_time(epoch - Duration::new(5, 250_000_000)),
        Born {
            secs: -6,
            nanos: 750_000_000
        }
    );
}

#[test]
fn the_helper_exit_codes_map_to_outcomes_with_clear_reasons() {
    assert_eq!(helper_outcome_of(Some(0), ""), Outcome::Changed);
    assert!(matches!(
        helper_outcome_of(Some(126), ""),
        Outcome::Cancelled(_)
    ));
    assert!(matches!(
        helper_outcome_of(Some(127), ""),
        Outcome::Cancelled(_)
    ));
    for (exit, words) in [
        (Exit::IdentityMismatch, "replaced"),
        (Exit::OwnerMismatch, "owner changed"),
    ] {
        let outcome = helper_outcome_of(Some(i32::from(exit.code())), "h: detail");
        let reason = failed_reason(&outcome);
        assert!(reason.contains("Nothing was changed"), "{exit:?}: {reason}");
        assert!(reason.contains(words), "{exit:?}: {reason}");
    }
    for exit in [
        Exit::Usage,
        Exit::InvalidPath,
        Exit::Refused,
        Exit::NotFound,
        Exit::ChownFailed,
        Exit::NotRoot,
    ] {
        let outcome = helper_outcome_of(Some(i32::from(exit.code())), "h: why it failed");
        let reason = failed_reason(&outcome);
        assert!(reason.contains("why it failed"), "{exit:?}: {reason}");
    }
    assert!(matches!(
        helper_outcome_of(Some(3), "x"),
        Outcome::Failed(_)
    ));
    assert!(matches!(helper_outcome_of(None, ""), Outcome::Failed(_)));
}

fn failed_reason(outcome: &Outcome) -> &str {
    match outcome {
        Outcome::Failed(reason) => reason,
        _ => "",
    }
}

#[test]
fn with_the_helper_change_owner_runs_the_helper_and_still_re_reads_afterwards() -> TestResult {
    let scratch = Scratch::new("helper")?;
    let path = scratch.0.join("f");
    fs::write(&path, b"x")?;
    let seen = Identity::of(&path)?;
    let ran = RefCell::new(Vec::new());
    // The helper "succeeds" but the owner is not the one asked for: the
    // post-change re-read still reports it.
    let outcome = change_owner(
        &with_helper(),
        seen.owner.wrapping_add(1),
        &path,
        &seen,
        |argv| {
            ran.borrow_mut().push(argv.to_vec());
            Ok(Finished {
                code: Some(0),
                stderr: String::new(),
            })
        },
    );
    assert!(
        matches!(outcome, Outcome::Failed(ref r) if r.contains("not changed")),
        "{outcome:?}"
    );
    let ran = ran.into_inner();
    assert_eq!(ran.len(), 1);
    let argv = ran.first().ok_or("no run")?;
    assert_eq!(argv.get(1), Some(&os("/opt/ff/bin/fileflux-owner-helper")));
    assert_eq!(argv.get(2), Some(&path.clone().into_os_string()));
    // A mismatch exit from the helper changes nothing and says why.
    let outcome = change_owner(&with_helper(), 0, &path, &seen, |_| {
        Ok(Finished {
            code: Some(i32::from(Exit::IdentityMismatch.code())),
            stderr: String::new(),
        })
    });
    assert!(
        matches!(outcome, Outcome::Failed(ref r) if r.contains("Nothing was changed")),
        "{outcome:?}"
    );
    Ok(())
}

#[test]
fn a_channel_build_finds_its_own_helper_under_libexec_of_its_channel() {
    let canon = |dir: &Path| Ok(dir.to_path_buf());
    let own = PathBuf::from("/store/ff-nightly/libexec/nightly").join(HELPER_NAME);
    // The launcher and the Nix wrapped executable both name the channel.
    for exe in [
        "/store/ff-nightly/bin/fileflux-nightly",
        "/store/ff-nightly/bin/.fileflux-nightly-wrapped",
    ] {
        let asked = RefCell::new(Vec::new());
        let found = helper_beside(Path::new(exe), canon, |path| {
            asked.borrow_mut().push(path.to_path_buf());
            path == own
        });
        assert_eq!(found, Some(own.clone()), "{exe}");
        // Never the helper of the source package next to it.
        assert_eq!(*asked.borrow(), vec![own.clone()], "{exe}");
    }
    // Missing there: none, even with a helper in bin/.
    let found = helper_beside(
        Path::new("/store/ff-nightly/bin/fileflux-nightly"),
        canon,
        |p| p == Path::new("/store/ff-nightly/bin").join(HELPER_NAME),
    );
    assert_eq!(found, None);
}

#[test]
fn a_channel_that_could_climb_out_of_libexec_is_not_a_channel() {
    let canon = |dir: &Path| Ok(dir.to_path_buf());
    let asked = RefCell::new(Vec::new());
    let found = helper_beside(Path::new("/x/bin/fileflux-.."), canon, |path| {
        asked.borrow_mut().push(path.to_path_buf());
        false
    });
    assert_eq!(found, None);
    assert_eq!(
        *asked.borrow(),
        vec![PathBuf::from("/x/bin").join(HELPER_NAME)]
    );
}
