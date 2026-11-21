//! Network locations: the URI model, the network bookmarks, gio's prompts
//! and the mount runner (against a fake `gio` script: no real server, no
//! keyring, no real bookmarks), and the lookup of the gvfs mount a URI made.
//! Every test works in a fresh directory under the system temp directory.

#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use super::{
    Answer, MountEvent, MountOutcome, MountSession, Prompt, PromptKind, PromptScanner, Scheme,
    Secret, ServerUri, UriError, mount_target, mounted_name, spawn_mount, wait_for_mount,
};
use crate::core::bookmarks::{self, NetworkBookmark};

/// The scripts are written and run right away: while another test forks, a
/// just-written script can be exec'd with ETXTBSY, so the tests take turns
/// writing, and spawning retries the busy case.
static TURN: Mutex<()> = Mutex::new(());

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fileflux-network-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(&dir).expect("scratch directory is created");
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// An executable `gio` script with `body`.
    fn gio(&self, body: &str) -> PathBuf {
        let _turn = TURN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = self.path("gio");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("script is written");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("script is executable");
        path
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.path(name)).unwrap_or_default()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.0));
    }
}

fn uri(text: &str) -> ServerUri {
    ServerUri::parse(text).expect("the URI is valid")
}

fn start(gio: &Path, target: &ServerUri) -> MountSession {
    for _ in 0..100 {
        match spawn_mount(gio, target) {
            Ok(session) => return session,
            Err(error) if error.to_string().to_lowercase().contains("busy") => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("gio did not start: {error}"),
        }
    }
    panic!("gio stayed busy");
}

fn next(session: &MountSession) -> MountEvent {
    session
        .events
        .recv_timeout(Duration::from_secs(10))
        .expect("the runner reports in time")
}

fn done(session: &MountSession) -> MountOutcome {
    match next(session) {
        MountEvent::Done(outcome) => outcome,
        MountEvent::Prompt(prompt) => panic!("unexpected prompt {prompt:?}"),
    }
}

fn prompt(session: &MountSession) -> Prompt {
    match next(session) {
        MountEvent::Prompt(prompt) => prompt,
        MountEvent::Done(outcome) => panic!("unexpected end {outcome:?}"),
    }
}

// --- the URI model ------------------------------------------------------------

#[test]
fn every_network_scheme_parses_and_prints_back() {
    for (text, scheme) in [
        ("sftp://host/", Scheme::Sftp),
        ("smb://host/share", Scheme::Smb),
        ("ftp://host/pub", Scheme::Ftp),
        ("ftps://host", Scheme::Ftps),
        ("dav://host/dav", Scheme::Dav),
        ("davs://host/dav", Scheme::Davs),
    ] {
        let parsed = uri(text);
        assert_eq!(parsed.scheme, scheme, "{text}");
        assert_eq!(parsed.to_string(), text);
    }
}

#[test]
fn other_schemes_and_non_uris_are_refused() {
    assert_eq!(ServerUri::parse(""), Err(UriError::Empty));
    assert_eq!(ServerUri::parse("   "), Err(UriError::Empty));
    assert_eq!(
        ServerUri::parse("example.com"),
        Err(UriError::MissingScheme)
    );
    for text in [
        "http://example.com",
        "file:///tmp",
        "ssh://host",
        "nfs://host/x",
    ] {
        assert!(
            matches!(ServerUri::parse(text), Err(UriError::UnsupportedScheme(_))),
            "{text}"
        );
    }
}

#[test]
fn a_host_is_required() {
    for text in [
        "sftp://",
        "sftp:///path",
        "sftp://:22/",
        "smb://user@/share",
    ] {
        assert_eq!(ServerUri::parse(text), Err(UriError::MissingHost), "{text}");
    }
}

#[test]
fn user_port_and_path_are_kept() {
    let parsed = uri("sftp://Bob@Example.COM:2222/home/bob/My%20Files");
    assert_eq!(parsed.user.as_deref(), Some("Bob"));
    assert_eq!(parsed.host, "example.com", "hosts are case-insensitive");
    assert_eq!(parsed.port, Some(2222));
    assert_eq!(parsed.path, "/home/bob/My%20Files");
    assert_eq!(
        parsed.to_string(),
        "sftp://Bob@example.com:2222/home/bob/My%20Files"
    );
}

#[test]
fn the_scheme_is_case_insensitive_and_whitespace_is_trimmed() {
    assert_eq!(uri("  SFTP://host/x \n").to_string(), "sftp://host/x");
}

#[test]
fn a_space_in_the_path_is_escaped_and_control_characters_are_refused() {
    assert_eq!(
        uri("smb://nas/My Share").to_string(),
        "smb://nas/My%20Share"
    );
    assert_eq!(
        ServerUri::parse("smb://nas/a\nb"),
        Err(UriError::InvalidCharacter)
    );
    assert_eq!(
        ServerUri::parse("smb://na\ts/x"),
        Err(UriError::InvalidCharacter)
    );
}

#[test]
fn an_ipv6_host_is_bracketed_and_may_have_a_port() {
    let plain = uri("sftp://[2001:db8::1]/srv");
    assert_eq!(plain.host, "2001:db8::1");
    assert_eq!(plain.port, None);
    assert_eq!(plain.to_string(), "sftp://[2001:db8::1]/srv");
    let ported = uri("sftp://root@[::1]:2222/");
    assert_eq!(ported.host, "::1");
    assert_eq!(ported.port, Some(2222));
    assert_eq!(ported.to_string(), "sftp://root@[::1]:2222/");
    for text in [
        "sftp://[::1/",
        "sftp://[]/",
        "sftp://[nothex]/",
        "sftp://[::1]x/",
    ] {
        assert!(ServerUri::parse(text).is_err(), "{text}");
    }
}

#[test]
fn ports_are_numbers_from_1_to_65535() {
    assert_eq!(uri("ftp://h:1/").port, Some(1));
    assert_eq!(uri("ftp://h:65535/").port, Some(65535));
    for text in [
        "ftp://h:0/",
        "ftp://h:65536/",
        "ftp://h:abc/",
        "ftp://h:/",
        "ftp://h:-1/",
    ] {
        assert_eq!(ServerUri::parse(text), Err(UriError::InvalidPort), "{text}");
    }
}

#[test]
fn a_password_in_the_address_is_refused_with_a_hint() {
    for text in [
        "sftp://bob:hunter2@host/",
        "smb://DOM;bob:pw@nas/share",
        "ftp://bob:@host/",
    ] {
        let error = ServerUri::parse(text).expect_err("a password is refused");
        assert_eq!(error, UriError::PasswordInUri, "{text}");
        let message = error.to_string();
        assert!(message.contains("password"), "{message}");
        assert!(
            message.contains("prompt"),
            "tells where to type it: {message}"
        );
        assert!(
            !message.contains("hunter2") && !message.contains("pw@"),
            "{message}"
        );
    }
}

#[test]
fn a_domain_user_is_a_user() {
    assert_eq!(
        uri("smb://WORKGROUP;bob@nas/share").user.as_deref(),
        Some("WORKGROUP;bob")
    );
}

#[test]
fn the_label_is_the_title_or_host_and_path() {
    let parsed = uri("smb://nas/media/Movies%20HD/");
    assert_eq!(parsed.label(Some("My NAS")), "My NAS");
    assert_eq!(parsed.label(Some("  ")), "nas/media/Movies HD");
    assert_eq!(parsed.label(None), "nas/media/Movies HD");
    assert_eq!(uri("sftp://example.com/").label(None), "example.com");
    assert_eq!(uri("sftp://example.com").label(None), "example.com");
    assert_eq!(
        uri("sftp://bob@example.com:2222/srv").label(None),
        "example.com:2222/srv"
    );
    assert_eq!(uri("sftp://[::1]/").label(None), "[::1]");
}

// --- network bookmarks --------------------------------------------------------

const BOOKMARKS: &str = "file:///home/me/Projects\n\
sftp://bob@example.com/srv My server\n\
smb://nas/media\n\
# a comment\n\
http://example.com/x Web\n\
sftp://bob:secret@evil/ Leaked\n\
sftp://bob@example.com/srv Duplicate\n\
ftp://[::1]:2121/pub FTP over v6\n\
file:///home/me/Music Tunes\n";

#[test]
fn network_bookmarks_are_listed_with_their_labels() {
    let file = bookmarks::parse(BOOKMARKS.as_bytes());
    let network = file.network();
    assert_eq!(
        network,
        vec![
            NetworkBookmark {
                uri: uri("sftp://bob@example.com/srv"),
                title: Some("My server".to_owned())
            },
            NetworkBookmark {
                uri: uri("smb://nas/media"),
                title: None
            },
            NetworkBookmark {
                uri: uri("ftp://[::1]:2121/pub"),
                title: Some("FTP over v6".to_owned())
            },
        ],
        "unsupported schemes, passwords and repeats are not listed"
    );
    assert_eq!(network[0].display_name(), "My server");
    assert_eq!(network[1].display_name(), "nas/media");
    assert_eq!(network[2].display_name(), "FTP over v6");
}

#[test]
fn local_favorites_are_unchanged_by_network_lines() {
    let file = bookmarks::parse(BOOKMARKS.as_bytes());
    let favorites = file.favorites();
    assert_eq!(favorites.len(), 2);
    assert_eq!(favorites[0].path, Path::new("/home/me/Projects"));
    assert_eq!(favorites[1].display_name(), "Tunes");
}

#[test]
fn removing_a_network_bookmark_drops_only_its_lines() {
    let scratch = Scratch::new("remove-network");
    let file = scratch.path("bookmarks");
    std::fs::write(&file, BOOKMARKS).expect("fixture is written");
    assert!(bookmarks::remove_network(&file, &uri("sftp://bob@example.com/srv")).expect("removed"));
    let left = scratch.read("bookmarks");
    assert!(!left.contains("My server") && !left.contains("Duplicate"));
    for kept in [
        "file:///home/me/Projects\n",
        "smb://nas/media\n",
        "# a comment\n",
        "http://example.com/x Web\n",
        "sftp://bob:secret@evil/ Leaked\n",
        "ftp://[::1]:2121/pub FTP over v6\n",
    ] {
        assert!(left.contains(kept), "{kept} survives");
    }
    assert!(
        !bookmarks::remove_network(&file, &uri("sftp://nobody@example.com/")).expect("no-op"),
        "nothing to remove"
    );
    assert_eq!(scratch.read("bookmarks"), left, "a no-op writes nothing");
}

#[test]
fn network_bookmark_edits_keep_every_other_byte() {
    let scratch = Scratch::new("bytes");
    let file = scratch.path("bookmarks");
    let original = "sftp://u:pw@h/ Secret\nsftp://h/ A\r\n# c\nfile:///tmp\nsmb://nas/media";
    std::fs::write(&file, original).expect("fixture");
    let listed = bookmarks::read_network(&file).expect("read");
    assert_eq!(listed.len(), 2);
    assert!(
        listed
            .iter()
            .all(|entry| !format!("{entry:?}").contains("pw"))
    );
    assert!(!bookmarks::remove_network(&file, &uri("sftp://zzz/")).expect("no-op"));
    assert_eq!(scratch.read("bookmarks"), original);
    assert!(bookmarks::remove_network(&file, &uri("sftp://h/")).expect("removed"));
    assert_eq!(
        scratch.read("bookmarks"),
        "sftp://u:pw@h/ Secret\n# c\nfile:///tmp\nsmb://nas/media"
    );
}

#[test]
fn reading_the_file_gives_the_network_entries() {
    let scratch = Scratch::new("read-network");
    let file = scratch.path("bookmarks");
    std::fs::write(&file, BOOKMARKS).expect("fixture is written");
    assert_eq!(bookmarks::read_network(&file).expect("read").len(), 3);
    assert!(
        bookmarks::read_network(&scratch.path("missing"))
            .expect("a missing file lists nothing")
            .is_empty()
    );
}

// --- gio's prompts ------------------------------------------------------------

fn scan(chunks: &[&str]) -> Vec<Prompt> {
    let mut scanner = PromptScanner::default();
    chunks
        .iter()
        .flat_map(|chunk| scanner.feed(chunk.as_bytes()))
        .collect()
}

#[test]
fn user_domain_and_password_prompts_are_recognised() {
    let prompts = scan(&[
        "Authentication Required\nEnter user and password for share \"media\" on \"nas\":\n",
        "User [bob]: ",
        "Domain [WORKGROUP]: ",
        "Password: ",
    ]);
    assert_eq!(prompts.len(), 3, "{prompts:?}");
    let Prompt::Text {
        kind,
        label,
        default,
        message,
    } = &prompts[0]
    else {
        panic!("{:?}", prompts[0]);
    };
    assert_eq!(*kind, PromptKind::User);
    assert_eq!(label, "User");
    assert_eq!(default.as_deref(), Some("bob"));
    assert_eq!(
        message,
        &[
            "Authentication Required",
            "Enter user and password for share \"media\" on \"nas\":"
        ]
    );
    assert!(!prompts[0].is_secret());
    let Prompt::Text {
        kind,
        default,
        message,
        ..
    } = &prompts[1]
    else {
        panic!("{:?}", prompts[1]);
    };
    assert_eq!(
        (*kind, default.as_deref()),
        (PromptKind::Domain, Some("WORKGROUP"))
    );
    assert!(
        message.is_empty(),
        "the message belongs to the first prompt only"
    );
    let Prompt::Text { kind, default, .. } = &prompts[2] else {
        panic!("{:?}", prompts[2]);
    };
    assert_eq!((*kind, default.as_deref()), (PromptKind::Password, None));
    assert!(prompts[2].is_secret());
}

#[test]
fn a_prompt_split_across_reads_is_found_once() {
    let prompts = scan(&["Passw", "ord:", " "]);
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].kind(), Some(PromptKind::Password));
}

#[test]
fn a_question_collects_its_choices() {
    let prompts = scan(&[
        "The authenticity of host 'example.com' can't be established.\nKey fingerprint is SHA256:abc.\n",
        "[1] Yes\n[2] No\n",
        "Choice: ",
    ]);
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    let Prompt::Question { message, choices } = &prompts[0] else {
        panic!("{:?}", prompts[0]);
    };
    assert_eq!(
        message,
        &[
            "The authenticity of host 'example.com' can't be established.",
            "Key fingerprint is SHA256:abc."
        ]
    );
    assert_eq!(choices, &[(1, "Yes".to_owned()), (2, "No".to_owned())]);
}

#[test]
fn only_the_numbered_block_before_the_choice_prompt_is_a_choice() {
    let prompts = scan(&[
        "Host says:\n[9] Trust forever\n[1] Log In Anyway\n[2] Cancel Login\n",
        "Choice: ",
    ]);
    let Prompt::Question { choices, .. } = &prompts[0] else {
        panic!("{:?}", prompts[0]);
    };
    assert_eq!(
        choices,
        &[
            (1, "Log In Anyway".to_owned()),
            (2, "Cancel Login".to_owned())
        ]
    );
}

#[test]
fn a_huge_stream_without_a_newline_stays_bounded_and_fast() {
    let started = std::time::Instant::now();
    let mut scanner = PromptScanner::default();
    let chunk = vec![b'x'; 4096];
    for _ in 0..1024 {
        assert!(scanner.feed(&chunk).is_empty());
    }
    assert!(scanner.feed(&vec![b'y'; 4 * 1024 * 1024]).is_empty());
    assert_eq!(
        scanner.feed(b"\nPassword: ").len(),
        1,
        "still finds a prompt"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn an_unknown_prompt_is_a_plain_text_question() {
    let prompts = scan(&["Anonymous login? [y/n] "]);
    assert_eq!(prompts.len(), 1);
    let Prompt::Text { kind, label, .. } = &prompts[0] else {
        panic!("{:?}", prompts[0]);
    };
    assert_eq!(*kind, PromptKind::Other);
    assert_eq!(label, "Anonymous login? [y/n]");
    assert!(!prompts[0].is_secret());
}

#[test]
fn output_that_is_not_a_prompt_is_not_one() {
    assert!(scan(&["Mounting...\n", "still working"]).is_empty());
    assert!(scan(&[""]).is_empty());
}

#[test]
fn the_echo_warning_is_not_part_of_a_message() {
    let prompts = scan(&["Warning! Password will be echoed\nPassword: "]);
    let Prompt::Text { message, .. } = &prompts[0] else {
        panic!("{:?}", prompts[0]);
    };
    assert!(message.is_empty(), "{message:?}");
}

#[test]
fn control_characters_in_a_message_are_dropped() {
    let prompts = scan(&["Hello \u{1b}[31mred\u{7}\nPassword: "]);
    let Prompt::Text { message, .. } = &prompts[0] else {
        panic!("{:?}", prompts[0]);
    };
    assert_eq!(message, &["Hello [31mred"]);
}

// --- the runner, against a fake gio -------------------------------------------

/// A fake gio that asks for user and password, records what arrives on its
/// standard input, its arguments and its environment, and fails with an
/// error that echoes the password back (to prove the runner redacts it).
fn asking_gio(scratch: &Scratch, right: &str) -> PathBuf {
    let dir = scratch.0.display();
    scratch.gio(&format!(
        r#"printf '%s\n' "$@" > '{dir}/argv'
env > '{dir}/env'
printf 'Authentication Required\n'
printf 'User [bob]: '
read user
printf 'Password: '
read pass
printf '%s\n%s\n' "$user" "$pass" >> '{dir}/stdin'
if [ "$pass" = '{right}' ]; then exit 0; fi
printf 'gio: sftp://bob@example.com/: Permission denied (password was %s)\n' "$pass" >&2
exit 2"#
    ))
}

#[test]
fn the_password_goes_through_stdin_once_and_nowhere_else() {
    let scratch = Scratch::new("round-trip");
    let gio = asking_gio(&scratch, "s3cret-Pw");
    let target = uri("sftp://example.com/srv");
    let session = start(&gio, &target);

    let Prompt::Text { kind, default, .. } = prompt(&session) else {
        panic!("a text prompt first");
    };
    assert_eq!((kind, default.as_deref()), (PromptKind::User, Some("bob")));
    session.control.answer(Answer::Line(Secret::new("bob")));
    assert_eq!(prompt(&session).kind(), Some(PromptKind::Password));
    session
        .control
        .answer(Answer::Line(Secret::new("s3cret-Pw")));
    assert_eq!(done(&session), MountOutcome::Mounted);

    assert_eq!(
        scratch.read("stdin"),
        "bob\ns3cret-Pw\n",
        "delivered once, in order"
    );
    assert_eq!(scratch.read("argv"), "mount\nsftp://example.com/srv\n");
    assert!(
        !scratch.read("env").contains("s3cret-Pw"),
        "not in the environment"
    );
    assert!(!scratch.read("argv").contains("s3cret-Pw"), "not in argv");
}

#[test]
fn a_failure_notice_never_carries_the_password() {
    let scratch = Scratch::new("redaction");
    let gio = asking_gio(&scratch, "right");
    let session = start(&gio, &uri("sftp://example.com/"));
    let mut seen = Vec::new();
    seen.push(format!("{:?}", prompt(&session)));
    session.control.answer(Answer::Line(Secret::new("bob")));
    seen.push(format!("{:?}", prompt(&session)));
    let answer = Secret::new("wr0ng-Pw");
    assert!(
        !format!("{answer:?}").contains("wr0ng-Pw"),
        "a secret never prints"
    );
    session.control.answer(Answer::Line(answer));
    let outcome = done(&session);
    seen.push(format!("{outcome:?}"));
    let MountOutcome::Failed(message) = outcome else {
        panic!("the wrong password fails: {outcome:?}");
    };
    assert!(message.contains("Permission denied"), "{message}");
    assert!(!message.contains("wr0ng-Pw"), "redacted: {message}");
    assert!(!message.starts_with("gio:"), "{message}");
    assert!(
        seen.iter().all(|text| !text.contains("wr0ng-Pw")),
        "{seen:?}"
    );
}

#[test]
fn a_choice_is_answered_by_the_number_gio_printed() {
    for (pick, text, mounted) in [(0, "Log In Anyway", true), (1, "Cancel Login", false)] {
        let scratch = Scratch::new(&format!("choice-{pick}"));
        let dir = scratch.0.display();
        let gio = scratch.gio(&format!(
            r#"printf "Host key unknown\n[1] Log In Anyway\n[2] Cancel Login\nChoice: "
read choice
printf '%s\n' "$choice" > '{dir}/choice'
[ "$choice" = 1 ] && exit 0
echo 'gio: refused' >&2
exit 2"#
        ));
        let session = start(&gio, &uri("sftp://example.com/"));
        let Prompt::Question { choices, .. } = prompt(&session) else {
            panic!("a question");
        };
        assert_eq!(choices[pick].1, text);
        session.control.answer(Answer::Choice(choices[pick].0));
        let outcome = done(&session);
        assert_eq!(outcome == MountOutcome::Mounted, mounted, "{outcome:?}");
        assert_eq!(scratch.read("choice"), format!("{}\n", pick + 1));
    }
}

#[test]
fn a_longer_password_is_redacted_before_a_shorter_one_inside_it() {
    let scratch = Scratch::new("redact-order");
    let gio = scratch.gio(
        r#"printf 'Password: '
read a
printf 'Password: '
read b
echo "gio: bad $b and $a" >&2
exit 2"#,
    );
    let session = start(&gio, &uri("sftp://example.com/"));
    prompt(&session);
    session.control.answer(Answer::Line(Secret::new("abc")));
    prompt(&session);
    session
        .control
        .answer(Answer::Line(Secret::new("abcXYZ987")));
    let MountOutcome::Failed(message) = done(&session) else {
        panic!("fails");
    };
    assert!(
        !message.contains("XYZ987") && !message.contains("abc"),
        "{message}"
    );
}

#[test]
fn cancel_still_kills_gio_after_it_closed_its_output() {
    let scratch = Scratch::new("closed-output");
    let dir = scratch.0.display();
    let gio = scratch.gio(&format!(
        "echo $$ > '{dir}/pid'\nexec >&- 2>&-\nexec sleep 60"
    ));
    let session = start(&gio, &uri("sftp://example.com/"));
    std::thread::sleep(Duration::from_millis(400));
    session.control.cancel();
    assert_eq!(done(&session), MountOutcome::Cancelled);
    let pid: u32 = scratch.read("pid").trim().parse().expect("pid");
    assert!(!Path::new(&format!("/proc/{pid}")).exists(), "gio is gone");
}

#[test]
fn a_mount_without_prompts_succeeds_and_runs_with_a_plain_locale() {
    let scratch = Scratch::new("quiet");
    let dir = scratch.0.display();
    let gio = scratch.gio(&format!("echo \"$LC_ALL\" > '{dir}/locale'\nexit 0"));
    let session = start(&gio, &uri("smb://nas/media"));
    assert_eq!(done(&session), MountOutcome::Mounted);
    assert_eq!(
        scratch.read("locale"),
        "C\n",
        "prompts are parsed in English"
    );
}

#[test]
fn already_mounted_counts_as_mounted() {
    let scratch = Scratch::new("already");
    let gio = scratch.gio("echo 'gio: sftp://h/: Location is already mounted' >&2\nexit 2");
    let session = start(&gio, &uri("sftp://h/"));
    assert_eq!(done(&session), MountOutcome::Mounted);
}

#[test]
fn a_failure_message_is_cleaned_up() {
    let scratch = Scratch::new("sanitize");
    let gio = scratch.gio(
        "printf 'gio: smb://nas/: Failed to mount \\033[31mWindows share\\033[0m\\n\\ngio: second line\\n' >&2\nexit 2",
    );
    let session = start(&gio, &uri("smb://nas/"));
    let MountOutcome::Failed(message) = done(&session) else {
        panic!("fails");
    };
    assert_eq!(
        message,
        "smb://nas/: Failed to mount [31mWindows share[0m second line"
    );
}

#[test]
fn a_silent_failure_names_the_exit_status() {
    let scratch = Scratch::new("silent");
    let gio = scratch.gio("exit 3");
    let session = start(&gio, &uri("smb://nas/"));
    let MountOutcome::Failed(message) = done(&session) else {
        panic!("fails");
    };
    assert!(message.contains('3'), "{message}");
}

#[test]
fn cancelling_kills_gio() {
    let scratch = Scratch::new("cancel");
    let dir = scratch.0.display();
    let gio = scratch.gio(&format!(
        r#"echo $$ > '{dir}/pid'
printf 'Password: '
exec sleep 60"#
    ));
    let session = start(&gio, &uri("sftp://example.com/"));
    assert_eq!(prompt(&session).kind(), Some(PromptKind::Password));
    let pid: u32 = scratch
        .read("pid")
        .trim()
        .parse()
        .expect("gio wrote its pid");
    assert!(
        Path::new(&format!("/proc/{pid}")).exists(),
        "gio is running"
    );
    session.control.cancel();
    assert_eq!(done(&session), MountOutcome::Cancelled);
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "gio is gone and reaped"
    );
}

#[test]
fn an_answer_with_a_line_break_is_refused_and_gio_is_stopped() {
    let scratch = Scratch::new("newline");
    let gio = scratch.gio("printf 'Password: '\nread pass\nexit 0");
    let session = start(&gio, &uri("sftp://example.com/"));
    prompt(&session);
    session.control.answer(Answer::Line(Secret::new("a\nb")));
    let MountOutcome::Failed(message) = done(&session) else {
        panic!("refused");
    };
    assert!(message.contains("line break"), "{message}");
}

#[test]
fn a_missing_gio_does_not_start() {
    let scratch = Scratch::new("missing");
    assert!(spawn_mount(&scratch.path("no-such-gio"), &uri("sftp://h/")).is_err());
}

// --- the gvfs mount a URI made ------------------------------------------------

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn a_mounted_uri_is_found_in_the_gvfs_names() {
    let list = names(&[
        "sftp:host=other.com,user=bob",
        "sftp:host=example.com,user=bob",
        "smb-share:server=nas,share=media",
        "smb-server:server=lab",
        "ftp:host=ftp.example.com",
        "dav:host=cloud.example.com,ssl=true,prefix=%2Fremote.php",
        "mtp:host=Pixel",
    ]);
    let find = |text: &str| mounted_name(&uri(text), &list);
    assert_eq!(
        find("sftp://bob@example.com/srv").as_deref(),
        Some("sftp:host=example.com,user=bob")
    );
    assert_eq!(
        find("sftp://example.com/").as_deref(),
        Some("sftp:host=example.com,user=bob"),
        "no user typed: any"
    );
    assert_eq!(find("sftp://alice@example.com/"), None, "another user");
    assert_eq!(find("sftp://example.com:2222/"), None, "another port");
    assert_eq!(
        find("smb://NAS/Media/Films").as_deref(),
        Some("smb-share:server=nas,share=media")
    );
    assert_eq!(find("smb://nas/other"), None);
    assert_eq!(find("smb://lab/").as_deref(), Some("smb-server:server=lab"));
    assert_eq!(
        find("ftps://ftp.example.com/").as_deref(),
        Some("ftp:host=ftp.example.com")
    );
    assert_eq!(
        find("davs://cloud.example.com/x").as_deref(),
        Some("dav:host=cloud.example.com,ssl=true,prefix=%2Fremote.php")
    );
    assert_eq!(
        find("dav://cloud.example.com/x"),
        None,
        "plain dav is not the ssl mount"
    );
    assert_eq!(find("sftp://nowhere/"), None);
}

#[test]
fn a_port_and_an_ipv6_host_match_their_mount() {
    let list = names(&["sftp:host=%3A%3A1,port=2222", "ftp:host=h,port=2121"]);
    assert!(mounted_name(&uri("sftp://[::1]:2222/"), &list).is_some());
    assert!(mounted_name(&uri("sftp://[::1]/"), &list).is_none());
    assert!(mounted_name(&uri("ftp://h:2121/"), &list).is_some());
}

#[test]
fn the_target_is_the_mount_plus_the_path_inside_it() {
    let root = Path::new("/run/user/1000/gvfs");
    let target = |text: &str, name: &str| mount_target(root, name, &uri(text));
    assert_eq!(
        target("sftp://h/home/bob/My%20Files", "sftp:host=h"),
        root.join("sftp:host=h/home/bob/My Files")
    );
    assert_eq!(
        target(
            "smb://nas/media/Films/2020",
            "smb-share:server=nas,share=media"
        ),
        root.join("smb-share:server=nas,share=media/Films/2020"),
        "the share is the mount, the rest is inside"
    );
    assert_eq!(target("sftp://h/", "sftp:host=h"), root.join("sftp:host=h"));
    assert_eq!(
        target("sftp://h/a/../../etc", "sftp:host=h"),
        root.join("sftp:host=h/a/etc"),
        "never above the mount"
    );
}

#[test]
fn the_mount_is_waited_for_in_the_gvfs_directory() {
    let scratch = Scratch::new("wait");
    let root = scratch.path("gvfs");
    std::fs::create_dir_all(&root).expect("gvfs dir");
    let target = uri("sftp://bob@example.com/srv");
    assert_eq!(
        wait_for_mount(&root, &target, 2, Duration::from_millis(10)),
        None,
        "nothing mounted"
    );
    let later = root.join("sftp:host=example.com,user=bob");
    let maker = std::thread::spawn({
        let later = later.clone();
        move || {
            std::thread::sleep(Duration::from_millis(80));
            std::fs::create_dir_all(later).expect("mount appears");
        }
    });
    let found = wait_for_mount(&root, &target, 100, Duration::from_millis(20));
    maker.join().expect("maker finished");
    assert_eq!(found, Some(later.join("srv")));
    assert_eq!(
        wait_for_mount(
            &scratch.path("no-gvfs"),
            &target,
            1,
            Duration::from_millis(1)
        ),
        None,
        "a missing gvfs directory lists nothing"
    );
}
