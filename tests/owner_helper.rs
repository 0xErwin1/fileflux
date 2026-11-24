//! The pure parts of the root owner helper (argument parsing, the snapshot
//! comparison, the exit codes, the component walk over a fake tree) and the
//! helper binary itself, which is only ever run here as the current
//! non-root user: it must refuse without root and validate its arguments.
//! Nothing here runs pkexec, sudo or anything as root.

use std::collections::HashMap;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use fileflux::core::child;
use fileflux::core::owner_helper::{
    Born, Exit, Failure, Found, HELPER_NAME, Kernel, Kind, Request, Snapshot, Step, Tree,
    open_without_links, split_path, stat, walk,
};

type TestResult = Result<(), Box<dyn Error>>;

fn os(text: &str) -> OsString {
    OsString::from(text)
}

fn args(items: &[&str]) -> Vec<OsString> {
    items.iter().map(|item| os(item)).collect()
}

fn failure_of<T: std::fmt::Debug>(result: Result<T, Failure>) -> Result<Failure, Box<dyn Error>> {
    match result {
        Err(failure) => Ok(failure),
        Ok(value) => Err(format!("expected a failure, got {value:?}").into()),
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Result<Self, Box<dyn Error>> {
        let dir =
            std::env::temp_dir().join(format!("fileflux-ownerhelper-{tag}-{}", std::process::id()));
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

fn snapshot() -> Snapshot {
    Snapshot {
        dev: 2049,
        ino: 77,
        born: Some(Born {
            secs: 1_700_000_000,
            nanos: 5,
        }),
        owner: 1000,
    }
}

// --- arguments -----------------------------------------------------------------------

#[test]
fn six_arguments_parse_into_a_request() -> TestResult {
    let request = Request::parse(args(&[
        "/home/ana/a b.txt",
        "2049",
        "77",
        "1700000000:5",
        "1000",
        "0",
    ]))
    .map_err(|f| f.reason)?;
    assert_eq!(request.path, os("/home/ana/a b.txt"));
    assert_eq!(request.snapshot, snapshot());
    assert_eq!(request.new_uid, 0);
    Ok(())
}

#[test]
fn a_birth_time_can_be_none_or_before_the_epoch() -> TestResult {
    let none = Request::parse(args(&["/a", "1", "2", "none", "3", "4"])).map_err(|f| f.reason)?;
    assert_eq!(none.snapshot.born, None);
    let old =
        Request::parse(args(&["/a", "1", "2", "-5:999999999", "3", "4"])).map_err(|f| f.reason)?;
    assert_eq!(
        old.snapshot.born,
        Some(Born {
            secs: -5,
            nanos: 999_999_999
        })
    );
    Ok(())
}

#[test]
fn to_args_round_trips_through_parse() -> TestResult {
    for born in [None, Some(Born { secs: -1, nanos: 0 }), snapshot().born] {
        let request = Request {
            path: os("/x/y z"),
            snapshot: Snapshot { born, ..snapshot() },
            new_uid: 4_294_967_294,
        };
        let parsed = Request::parse(request.to_args()).map_err(|f| f.reason)?;
        assert_eq!(parsed, request);
    }
    Ok(())
}

#[test]
fn wrong_argument_counts_and_malformed_numbers_are_usage_errors() -> TestResult {
    let bad: &[&[&str]] = &[
        &[],
        &["/a", "1", "2", "none", "3"],
        &["/a", "1", "2", "none", "3", "4", "5"],
        &["/a", "x", "2", "none", "3", "4"],
        &["/a", "1", "+2", "none", "3", "4"],
        &["/a", "1", "2", "none", "-3", "4"],
        &["/a", "1", "2", "none", "3", ""],
        &["/a", "1", "2", "7", "3", "4"],
        &["/a", "1", "2", "7:1000000000", "3", "4"],
        &["/a", "1", "2", "7:", "3", "4"],
        // (uid_t)-1 means "leave the owner alone" to chown, never a target.
        &["/a", "1", "2", "none", "3", "4294967295"],
        &["/a", "1", "2", "none", "4294967295", "4"],
    ];
    for case in bad {
        let failure = failure_of(Request::parse(args(case)))?;
        assert_eq!(failure.exit, Exit::Usage, "{case:?}");
    }
    Ok(())
}

#[test]
fn the_path_must_be_absolute_and_clean() -> TestResult {
    for path in [
        "relative/a",
        "",
        "/",
        "/a/",
        "//a",
        "/a//b",
        "/a/./b",
        "/a/../b",
        "/..",
        "/a/.",
    ] {
        let failure = failure_of(Request::parse(args(&[path, "1", "2", "none", "3", "4"])))?;
        assert_eq!(failure.exit, Exit::InvalidPath, "{path:?}");
    }
    let long = format!("/{}", "a".repeat(5000));
    let failure = failure_of(Request::parse(args(&[&long, "1", "2", "none", "3", "4"])))?;
    assert_eq!(failure.exit, Exit::InvalidPath);
    Ok(())
}

#[test]
fn a_non_utf8_path_is_kept_byte_for_byte() -> TestResult {
    let raw = OsStr::from_bytes(b"/tmp/\xff\xfe");
    let mut all = vec![raw.to_owned()];
    all.extend(args(&["1", "2", "none", "3", "4"]));
    let request = Request::parse(all).map_err(|f| f.reason)?;
    assert_eq!(request.path.as_bytes(), b"/tmp/\xff\xfe");
    Ok(())
}

#[test]
fn split_path_gives_the_components() -> TestResult {
    let parts = split_path(OsStr::new("/a/b c/d")).map_err(|f| f.reason)?;
    assert_eq!(
        parts,
        vec![OsStr::new("a"), OsStr::new("b c"), OsStr::new("d")]
    );
    Ok(())
}

// --- exit codes and reasons -----------------------------------------------------------

#[test]
fn exit_codes_are_distinct_and_avoid_pkexecs_own() {
    let all = [
        Exit::Usage,
        Exit::InvalidPath,
        Exit::Refused,
        Exit::NotFound,
        Exit::IdentityMismatch,
        Exit::OwnerMismatch,
        Exit::ChownFailed,
        Exit::NotRoot,
    ];
    let mut seen = std::collections::BTreeSet::new();
    for exit in all {
        let code = exit.code();
        assert!(code != 0 && code != 126 && code != 127, "{exit:?}");
        assert!(seen.insert(code), "{exit:?} repeats {code}");
        assert_eq!(Exit::from_code(i32::from(code)), Some(exit));
    }
    assert_eq!(Exit::from_code(0), None);
    assert_eq!(Exit::from_code(126), None);
    assert_eq!(Exit::from_code(-1), None);
}

#[test]
fn a_reason_is_one_line() {
    let failure = Failure::new(Exit::ChownFailed, "bad\nthing\r\u{1b}[0m");
    assert!(
        !failure.reason.contains(['\n', '\r', '\u{1b}']),
        "{}",
        failure.reason
    );
}

// --- comparing the snapshot -----------------------------------------------------------

fn found(kind: Kind, snap: Snapshot) -> Found {
    Found {
        kind,
        snapshot: snap,
    }
}

#[test]
fn the_same_file_passes_for_files_and_folders() {
    assert!(snapshot().verify(&found(Kind::File, snapshot())).is_ok());
    assert!(snapshot().verify(&found(Kind::Dir, snapshot())).is_ok());
}

#[test]
fn each_identity_field_alone_is_an_identity_mismatch() -> TestResult {
    let variants = [
        Snapshot {
            dev: 1,
            ..snapshot()
        },
        Snapshot {
            ino: 1,
            ..snapshot()
        },
        Snapshot {
            born: None,
            ..snapshot()
        },
        Snapshot {
            born: Some(Born {
                secs: 1_700_000_000,
                nanos: 6,
            }),
            ..snapshot()
        },
        Snapshot {
            born: Some(Born {
                secs: 1_700_000_001,
                nanos: 5,
            }),
            ..snapshot()
        },
    ];
    for variant in variants {
        let failure = failure_of(snapshot().verify(&found(Kind::File, variant.clone())))?;
        assert_eq!(failure.exit, Exit::IdentityMismatch, "{variant:?}");
    }
    Ok(())
}

#[test]
fn another_owner_is_an_owner_mismatch_and_identity_wins_over_it() -> TestResult {
    let other_owner = Snapshot {
        owner: 0,
        ..snapshot()
    };
    let failure = failure_of(snapshot().verify(&found(Kind::File, other_owner)))?;
    assert_eq!(failure.exit, Exit::OwnerMismatch);
    let both = Snapshot {
        ino: 1,
        owner: 0,
        ..snapshot()
    };
    let failure = failure_of(snapshot().verify(&found(Kind::File, both)))?;
    assert_eq!(failure.exit, Exit::IdentityMismatch);
    Ok(())
}

#[test]
fn a_symlink_or_a_special_file_is_refused_even_if_everything_matches() -> TestResult {
    for kind in [Kind::Symlink, Kind::Other] {
        let failure = failure_of(snapshot().verify(&found(kind, snapshot())))?;
        assert_eq!(failure.exit, Exit::Refused, "{kind:?}");
    }
    Ok(())
}

// --- the component walk over a fake tree ----------------------------------------------

/// Nodes are paths; a table says what each name is.
struct Fake {
    entries: HashMap<&'static str, Result<(), Step>>,
}

impl Tree for Fake {
    type Node = String;

    fn root(&self) -> Result<String, String> {
        Ok(String::new())
    }

    fn open(&self, parent: &String, name: &OsStr, _last: bool) -> Result<String, Step> {
        let path = format!("{parent}/{}", name.to_string_lossy());
        match self.entries.get(path.as_str()) {
            Some(Ok(())) => Ok(path),
            Some(Err(step)) => Err(step.clone()),
            None => Err(Step::Missing),
        }
    }
}

#[test]
fn the_walk_opens_each_component_in_order_and_returns_the_last() -> TestResult {
    let fake = Fake {
        entries: HashMap::from([("/a", Ok(())), ("/a/b", Ok(())), ("/a/b/c", Ok(()))]),
    };
    let parts = split_path(OsStr::new("/a/b/c")).map_err(|f| f.reason)?;
    let node = walk(&fake, &parts).map_err(|f| f.reason)?;
    assert_eq!(node, "/a/b/c");
    Ok(())
}

#[test]
fn the_walk_stops_at_a_symlink_in_any_position() -> TestResult {
    for link in ["/a", "/a/b", "/a/b/c"] {
        let mut entries = HashMap::from([("/a", Ok(())), ("/a/b", Ok(())), ("/a/b/c", Ok(()))]);
        entries.insert(link, Err(Step::Symlink));
        let parts = split_path(OsStr::new("/a/b/c")).map_err(|f| f.reason)?;
        let failure = failure_of(walk(&Fake { entries }, &parts))?;
        assert_eq!(failure.exit, Exit::Refused, "{link}");
    }
    Ok(())
}

#[test]
fn the_walk_reports_a_missing_component_and_other_failures() -> TestResult {
    let fake = Fake {
        entries: HashMap::from([("/a", Ok(()))]),
    };
    let parts = split_path(OsStr::new("/a/b")).map_err(|f| f.reason)?;
    assert_eq!(failure_of(walk(&fake, &parts))?.exit, Exit::NotFound);
    let fake = Fake {
        entries: HashMap::from([("/a", Err(Step::Failed("denied".to_owned())))]),
    };
    let failure = failure_of(walk(&fake, &parts))?;
    assert_eq!(failure.exit, Exit::Refused);
    assert!(failure.reason.contains("denied"), "{}", failure.reason);
    Ok(())
}

// --- the real kernel, as the current user ---------------------------------------------

fn parts_of(path: &Path) -> Result<Vec<OsString>, Failure> {
    split_path(path.as_os_str()).map(|parts| parts.into_iter().map(OsStr::to_owned).collect())
}

fn open_each_way(path: &Path) -> Result<[Result<Found, Failure>; 2], Box<dyn Error>> {
    let owned = parts_of(path).map_err(|f| f.reason)?;
    let parts: Vec<&OsStr> = owned.iter().map(OsString::as_os_str).collect();
    let via_openat2 = open_without_links(&parts).and_then(|fd| stat(&fd));
    let via_walk = walk(&Kernel, &parts).and_then(|fd| stat(&fd));
    Ok([via_openat2, via_walk])
}

#[test]
fn a_real_file_resolves_both_ways_to_the_same_snapshot() -> TestResult {
    let scratch = Scratch::new("real")?;
    let file = scratch.0.join("report.txt");
    fs::write(&file, b"x")?;
    let meta = fs::symlink_metadata(&file)?;
    for result in open_each_way(&file)? {
        let found = result.map_err(|f| f.reason)?;
        assert_eq!(found.kind, Kind::File);
        assert_eq!(found.snapshot.dev, meta.dev());
        assert_eq!(found.snapshot.ino, meta.ino());
        assert_eq!(found.snapshot.owner, meta.uid());
        assert_eq!(
            found.snapshot.born.map(|born| (born.secs, born.nanos)),
            meta.created().ok().map(|time| {
                let since = time
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                (
                    i64::try_from(since.as_secs()).unwrap_or(0),
                    since.subsec_nanos(),
                )
            })
        );
    }
    let folder = scratch.0.join("dir");
    fs::create_dir(&folder)?;
    for result in open_each_way(&folder)? {
        assert_eq!(result.map_err(|f| f.reason)?.kind, Kind::Dir);
    }
    Ok(())
}

#[test]
fn a_symlink_in_the_middle_or_at_the_end_is_refused_both_ways() -> TestResult {
    let scratch = Scratch::new("links")?;
    fs::create_dir(scratch.0.join("real"))?;
    fs::write(scratch.0.join("real/f"), b"x")?;
    symlink("real", scratch.0.join("link"))?;
    symlink("real/f", scratch.0.join("flink"))?;
    for path in [scratch.0.join("link/f"), scratch.0.join("flink")] {
        for result in open_each_way(&path)? {
            // The last component may open as the link itself; it is then
            // refused by its kind, so either way nothing is accepted.
            let refused = match result {
                Err(failure) => failure.exit == Exit::Refused,
                Ok(found) => found.kind == Kind::Symlink,
            };
            assert!(refused, "{path:?}");
        }
    }
    Ok(())
}

#[test]
fn a_missing_path_and_a_file_used_as_a_folder_are_not_found() -> TestResult {
    let scratch = Scratch::new("missing")?;
    fs::write(scratch.0.join("f"), b"x")?;
    for path in [scratch.0.join("nope"), scratch.0.join("f/inside")] {
        for result in open_each_way(&path)? {
            assert_eq!(failure_of(result)?.exit, Exit::NotFound, "{path:?}");
        }
    }
    Ok(())
}

// --- the binary, as a non-root user ----------------------------------------------------

fn helper() -> Command {
    let mut command = child::command(env!("CARGO_BIN_EXE_fileflux-owner-helper"));
    command.stdin(Stdio::null());
    command
}

fn run(items: &[&str]) -> Result<(Option<i32>, String, String), Box<dyn Error>> {
    let output = helper().args(items).output()?;
    Ok((
        output.status.code(),
        String::from_utf8(output.stdout)?,
        String::from_utf8(output.stderr)?,
    ))
}

fn running_as_root() -> bool {
    fs::metadata("/proc/self").is_ok_and(|meta| meta.uid() == 0)
}

#[test]
fn the_binary_name_matches_the_constant() {
    assert!(env!("CARGO_BIN_EXE_fileflux-owner-helper").ends_with(HELPER_NAME));
}

#[test]
fn the_binary_reports_usage_and_path_errors_with_their_codes() -> TestResult {
    let (code, out, err) = run(&[])?;
    assert_eq!(code, Some(i32::from(Exit::Usage.code())));
    assert_eq!(out, "");
    assert!(
        err.starts_with(HELPER_NAME) && err.trim_end().lines().count() == 1,
        "{err:?}"
    );
    let (code, out, _) = run(&["relative", "1", "2", "none", "3", "4"])?;
    assert_eq!(code, Some(i32::from(Exit::InvalidPath.code())));
    assert_eq!(out, "");
    let (code, _, _) = run(&["/a/../b", "1", "2", "none", "3", "4"])?;
    assert_eq!(code, Some(i32::from(Exit::InvalidPath.code())));
    Ok(())
}

#[test]
fn the_binary_refuses_without_root_and_changes_nothing() -> TestResult {
    if running_as_root() {
        return Ok(());
    }
    let scratch = Scratch::new("bin")?;
    let file = scratch.0.join("f");
    fs::write(&file, b"x")?;
    let meta = fs::symlink_metadata(&file)?;
    let (dev, ino, uid) = (
        meta.dev().to_string(),
        meta.ino().to_string(),
        meta.uid().to_string(),
    );
    let path = file.to_string_lossy().into_owned();
    let (code, out, err) = run(&[&path, &dev, &ino, "none", &uid, &uid])?;
    assert_eq!(code, Some(i32::from(Exit::NotRoot.code())));
    assert_eq!(out, "");
    assert!(err.contains("root"), "{err:?}");
    assert_eq!(fs::symlink_metadata(&file)?.uid(), meta.uid());
    Ok(())
}

// From the independent verification: magic links and /proc/self are refused
// both ways.
#[test]
fn magic_links_are_refused_both_ways() -> TestResult {
    let pid = std::process::id();
    for p in [
        format!("/proc/{pid}/cwd"),
        format!("/proc/{pid}/exe"),
        "/proc/self/status".to_owned(),
    ] {
        for r in open_each_way(Path::new(&p))? {
            assert!(
                match r {
                    Err(f) => f.exit == Exit::Refused,
                    Ok(f) => f.kind == Kind::Symlink,
                },
                "{p}"
            );
        }
    }
    Ok(())
}

// The panel's snapshot equals what the helper reads; otherwise every change
// would fail as "replaced".
#[test]
fn the_panel_snapshot_matches_the_helper_stat() -> TestResult {
    let scratch = Scratch::new("snapmatch")?;
    let file = scratch.0.join("a\nb");
    fs::write(&file, b"x")?;
    for p in [file, scratch.0.clone(), PathBuf::from("/proc/version")] {
        let want = fileflux::core::elevate::Identity::of(&p)?.snapshot();
        for r in open_each_way(&p)? {
            let found = r.map_err(|f| f.reason)?;
            assert_eq!(found.snapshot, want, "{p:?}");
        }
    }
    Ok(())
}

// The whole run on an O_PATH descriptor reaches the inode (own uid succeeds,
// another is EPERM); a stale owner, a replaced file or a parent symlink
// change nothing.
#[test]
fn run_chowns_through_the_o_path_descriptor() -> TestResult {
    let scratch = Scratch::new("run")?;
    let p = scratch.0.join("f");
    fs::write(&p, b"x")?;
    let snap = fileflux::core::elevate::Identity::of(&p)?.snapshot();
    let me = snap.owner;
    let req = Request {
        path: p.clone().into_os_string(),
        snapshot: snap.clone(),
        new_uid: me,
    };
    assert_eq!(fileflux::core::owner_helper::run(&req, true), Ok(()));
    if me != 0 {
        let e = failure_of(fileflux::core::owner_helper::run(
            &Request {
                new_uid: me + 1,
                ..req.clone()
            },
            true,
        ))?;
        assert_eq!(e.exit, Exit::ChownFailed);
    }
    let stale = Request {
        snapshot: Snapshot {
            owner: me.wrapping_add(7),
            ..snap.clone()
        },
        ..req.clone()
    };
    assert_eq!(
        failure_of(fileflux::core::owner_helper::run(&stale, true))?.exit,
        Exit::OwnerMismatch
    );
    fs::rename(&p, scratch.0.join("kept"))?;
    fs::write(&p, b"y")?;
    assert_eq!(
        failure_of(fileflux::core::owner_helper::run(&req, true))?.exit,
        Exit::IdentityMismatch
    );
    fs::create_dir(scratch.0.join("d"))?;
    fs::write(scratch.0.join("d/x"), b"x")?;
    let s2 = fileflux::core::elevate::Identity::of(&scratch.0.join("d/x"))?.snapshot();
    symlink("d", scratch.0.join("l"))?;
    let via = Request {
        path: scratch.0.join("l/x").into_os_string(),
        snapshot: s2,
        new_uid: me,
    };
    assert_eq!(
        failure_of(fileflux::core::owner_helper::run(&via, true))?.exit,
        Exit::Refused
    );
    Ok(())
}

// A FIFO is refused by its kind.
#[test]
fn a_fifo_is_refused() -> TestResult {
    let scratch = Scratch::new("fifo")?;
    let p = scratch.0.join("q");
    assert!(child::command("mkfifo").arg(&p).status()?.success());
    let want = fileflux::core::elevate::Identity::of(&p)?.snapshot();
    for r in open_each_way(&p)? {
        let found = r.map_err(|f| f.reason)?;
        assert_eq!(failure_of(want.verify(&found))?.exit, Exit::Refused);
    }
    Ok(())
}
