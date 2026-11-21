//! Desktop integration over the session bus: a single running instance and
//! the `org.freedesktop.FileManager1` interface ("Show in folder" from other
//! applications).
//!
//! Everything here is free of GPUI. The bus runs on zbus's own executor
//! thread; what arrives is handed to the app as [`Request`]s through a
//! channel the UI thread drains, so the window never waits on the bus.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use zbus::Connection;
use zbus::connection::Builder;
use zbus::fdo::{RequestNameFlags, RequestNameReply};

use crate::core::clipboard::{path_to_uri, uri_to_path};

/// The unique name of the running instance (the app id of the desktop file).
pub const APP_NAME: &str = "io.github.fileflux.Fileflux";
const APP_PATH: &str = "/io/github/fileflux/Fileflux";
const APP_INTERFACE: &str = "io.github.fileflux.Fileflux1";
/// The file-manager interface other applications call.
pub const FILE_MANAGER_NAME: &str = "org.freedesktop.FileManager1";
const FILE_MANAGER_PATH: &str = "/org/freedesktop/FileManager1";

/// How long the whole claim may take before the app opens normally.
const CLAIM_TIMEOUT: Duration = Duration::from_secs(3);

/// Which bus to talk to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// The user's session bus, as the environment names it.
    Session,
    /// A bus at this address (tests use a private daemon).
    Address(String),
}

/// One folder to show and the entries of it to select.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Show {
    pub folder: PathBuf,
    pub select: Vec<PathBuf>,
}

/// What the bus asked the app to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// A new tab per folder, with its entries selected. Empty: only raise the
    /// window.
    pub targets: Vec<Show>,
    /// Open the details panel as well (`ShowItemProperties`).
    pub properties: bool,
}

/// This process owns [`APP_NAME`]: the requests of the bus arrive here.
#[derive(Debug)]
pub struct Primary {
    _connection: Connection,
    requests: Receiver<Request>,
    owns_file_manager: bool,
}

impl Primary {
    /// The next request, without waiting.
    pub fn try_recv(&self) -> Option<Request> {
        self.requests.try_recv().ok()
    }

    /// The next request, waiting up to `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Request> {
        self.requests.recv_timeout(timeout).ok()
    }

    /// Whether this process also owns `org.freedesktop.FileManager1`; false
    /// when another file manager already does.
    #[must_use]
    pub fn owns_file_manager(&self) -> bool {
        self.owns_file_manager
    }
}

/// What the start-up claim ended in.
#[derive(Debug)]
pub enum Role {
    /// First instance: open the window and serve the bus.
    Primary(Primary),
    /// Another instance owns the name and was handed the paths: exit.
    Forwarded,
    /// No usable bus, or any bus error: open normally.
    Standalone,
}

/// Claims [`APP_NAME`] on `endpoint`. When another instance owns it, `forward`
/// (absolute paths) is sent to it. Never fails: a missing bus or any error is
/// logged at debug level and answered with [`Role::Standalone`].
pub fn claim(endpoint: &Endpoint, forward: &[PathBuf]) -> Role {
    let (done, answer) = channel();
    let endpoint = endpoint.clone();
    let forward = forward.to_vec();
    // The bus work runs on its own thread so a wedged peer cannot hold the
    // start-up past the timeout.
    let spawned = std::thread::Builder::new()
        .name("fileflux-dbus-claim".into())
        .spawn(move || {
            let role = zbus::block_on(claim_async(&endpoint, &forward)).unwrap_or_else(|error| {
                log::debug!("no desktop integration: {error}");
                Role::Standalone
            });
            // The receiver is gone after a timeout: nothing to tell then.
            drop(done.send(role));
        });
    if let Err(error) = spawned {
        log::debug!("cannot start the D-Bus thread: {error}");
        return Role::Standalone;
    }
    answer.recv_timeout(CLAIM_TIMEOUT).unwrap_or_else(|_| {
        log::debug!("the session bus did not answer in time");
        Role::Standalone
    })
}

async fn claim_async(endpoint: &Endpoint, forward: &[PathBuf]) -> zbus::Result<Role> {
    let builder = match endpoint {
        Endpoint::Session => Builder::session()?,
        Endpoint::Address(address) => Builder::address(address.as_str())?,
    };
    let connection = builder.build().await?;
    let (requests_in, requests) = channel();
    // The objects exist before any name is requested, so no call is lost.
    let server = connection.object_server();
    server
        .at(
            APP_PATH,
            Instance {
                requests: requests_in.clone(),
            },
        )
        .await?;
    server
        .at(
            FILE_MANAGER_PATH,
            FileManager {
                requests: requests_in,
            },
        )
        .await?;

    let owned = |reply: &RequestNameReply| {
        matches!(
            reply,
            RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner
        )
    };
    // DoNotQueue and no AllowReplacement: never wait for, or take over from,
    // another owner.
    let reply = connection
        .request_name_with_flags(APP_NAME, RequestNameFlags::DoNotQueue.into())
        .await;
    match reply {
        Ok(reply) if owned(&reply) => {}
        // Someone else owns the name (DoNotQueue answers `Exists`).
        Ok(_) | Err(zbus::Error::NameTaken) => {
            let uris: Vec<String> = forward.iter().filter_map(|p| path_to_uri(p)).collect();
            connection
                .call_method(
                    Some(APP_NAME),
                    APP_PATH,
                    Some(APP_INTERFACE),
                    "Open",
                    &(uris, String::new()),
                )
                .await?;
            return Ok(Role::Forwarded);
        }
        Err(error) => return Err(error),
    }
    let owns_file_manager = match connection
        .request_name_with_flags(FILE_MANAGER_NAME, RequestNameFlags::DoNotQueue.into())
        .await
    {
        Ok(reply) => owned(&reply),
        Err(error) => {
            log::debug!("{FILE_MANAGER_NAME} stays with its owner: {error}");
            false
        }
    };
    if !owns_file_manager {
        log::debug!("{FILE_MANAGER_NAME} is not ours; not taking it over");
    }
    Ok(Role::Primary(Primary {
        _connection: connection,
        requests,
        owns_file_manager,
    }))
}

/// `io.github.fileflux.Fileflux1`: what a second launch calls.
struct Instance {
    requests: Sender<Request>,
}

#[zbus::interface(name = "io.github.fileflux.Fileflux1")]
impl Instance {
    /// Opens each folder in a new tab, or the folder of each file with the
    /// file selected. Entries are absolute paths or `file://` URIs.
    fn open(&self, paths: Vec<String>, _startup_id: String) {
        let paths = paths_of(&paths);
        self.send(Request {
            targets: open_targets(&paths, Path::is_dir),
            properties: false,
        });
    }
}

/// `org.freedesktop.FileManager1`.
struct FileManager {
    requests: Sender<Request>,
}

#[zbus::interface(name = "org.freedesktop.FileManager1")]
impl FileManager {
    fn show_folders(&self, uris: Vec<String>, _startup_id: String) {
        let folders = uris_to_paths(&uris);
        self.send(Request {
            targets: folder_targets(&folders),
            properties: false,
        });
    }

    fn show_items(&self, uris: Vec<String>, _startup_id: String) {
        self.send(Request {
            targets: group_by_parent(&uris_to_paths(&uris)),
            properties: false,
        });
    }

    fn show_item_properties(&self, uris: Vec<String>, _startup_id: String) {
        self.send(Request {
            targets: group_by_parent(&uris_to_paths(&uris)),
            properties: true,
        });
    }
}

impl Instance {
    fn send(&self, request: Request) {
        // The app is gone when the channel is closed: nothing to do.
        drop(self.requests.send(request));
    }
}

impl FileManager {
    fn send(&self, request: Request) {
        drop(self.requests.send(request));
    }
}

/// The local paths of `uris`; anything that is not a `file://` URI is ignored.
#[must_use]
pub fn uris_to_paths(uris: &[String]) -> Vec<PathBuf> {
    uris.iter().filter_map(|uri| uri_to_path(uri)).collect()
}

/// The paths of the `Open` argument: `file://` URIs are decoded, absolute
/// paths are kept, everything else (other schemes, relative paths) is ignored.
fn paths_of(entries: &[String]) -> Vec<PathBuf> {
    entries
        .iter()
        .filter_map(|entry| {
            if entry.contains("://") || entry.starts_with("file:") {
                uri_to_path(entry)
            } else {
                let path = PathBuf::from(entry);
                path.is_absolute().then_some(path)
            }
        })
        .collect()
}

/// One target per distinct folder, in order, nothing selected.
#[must_use]
pub fn folder_targets(folders: &[PathBuf]) -> Vec<Show> {
    let mut targets: Vec<Show> = Vec::new();
    for folder in folders {
        if !targets.iter().any(|target| &target.folder == folder) {
            targets.push(Show {
                folder: folder.clone(),
                select: Vec::new(),
            });
        }
    }
    targets
}

/// Items grouped by their parent folder, in order of first appearance; the
/// root has no parent and is shown itself.
#[must_use]
pub fn group_by_parent(items: &[PathBuf]) -> Vec<Show> {
    let mut targets: Vec<Show> = Vec::new();
    for item in items {
        let Some(parent) = item.parent() else {
            targets.extend(folder_targets(std::slice::from_ref(item)));
            continue;
        };
        match targets.iter_mut().find(|target| target.folder == parent) {
            Some(target) => {
                if !target.select.contains(item) {
                    target.select.push(item.clone());
                }
            }
            None => targets.push(Show {
                folder: parent.to_path_buf(),
                select: vec![item.clone()],
            }),
        }
    }
    targets
}

/// What `Open` shows: a folder opens itself, a file opens its folder with the
/// file selected (as many folders as needed, in order).
#[must_use]
pub fn open_targets(paths: &[PathBuf], is_dir: impl Fn(&Path) -> bool) -> Vec<Show> {
    let mut targets: Vec<Show> = Vec::new();
    for path in paths {
        let next = if is_dir(path) {
            folder_targets(std::slice::from_ref(path))
        } else {
            group_by_parent(std::slice::from_ref(path))
        };
        for show in next {
            match targets
                .iter_mut()
                .find(|target| target.folder == show.folder)
            {
                Some(target) => {
                    for item in show.select {
                        if !target.select.contains(&item) {
                            target.select.push(item);
                        }
                    }
                }
                None => targets.push(show),
            }
        }
    }
    targets
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn p(text: &str) -> PathBuf {
        PathBuf::from(text)
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    fn show(folder: &str, select: &[&str]) -> Show {
        Show {
            folder: p(folder),
            select: select.iter().map(|item| p(item)).collect(),
        }
    }

    #[test]
    fn uris_are_decoded_and_only_file_uris_are_kept() {
        let uris = strings(&[
            "file:///tmp/a%20b",
            "http://example.com/x",
            "smb://host/share/y",
            "file://otherhost/tmp/z",
            "/tmp/plain",
            "file://localhost/home/u/caf%C3%A9",
        ]);
        assert_eq!(uris_to_paths(&uris), vec![p("/tmp/a b"), p("/home/u/café")]);
        assert!(uris_to_paths(&[]).is_empty());
    }

    #[test]
    fn open_arguments_take_paths_and_file_uris_only() {
        let entries = strings(&["/tmp/a", "file:///tmp/b%20c", "relative/x", "ftp://h/p", ""]);
        assert_eq!(paths_of(&entries), vec![p("/tmp/a"), p("/tmp/b c")]);
    }

    #[test]
    fn items_group_by_parent_in_order_without_duplicates() {
        let items = vec![p("/a/one"), p("/b/two"), p("/a/three"), p("/a/one"), p("/")];
        assert_eq!(
            group_by_parent(&items),
            vec![
                show("/a", &["/a/one", "/a/three"]),
                show("/b", &["/b/two"]),
                show("/", &[]),
            ]
        );
    }

    #[test]
    fn folders_open_once_each_and_open_splits_files_from_folders() {
        assert_eq!(
            folder_targets(&[p("/a"), p("/b"), p("/a")]),
            vec![show("/a", &[]), show("/b", &[])]
        );
        let is_dir = |path: &Path| path == Path::new("/d");
        assert_eq!(
            open_targets(&[p("/d"), p("/d/f"), p("/e/g"), p("/e/h")], is_dir),
            vec![show("/d", &["/d/f"]), show("/e", &["/e/g", "/e/h"])],
            "a folder opens itself; a file opens its folder with the file selected"
        );
        assert!(open_targets(&[], is_dir).is_empty());
    }

    /// A private `dbus-daemon` with a bare configuration: it never reads the
    /// user's session bus or any service file.
    struct Daemon {
        child: Child,
        address: String,
        scratch: PathBuf,
    }

    impl Daemon {
        fn start(tag: &str) -> Result<Self, Box<dyn std::error::Error>> {
            let scratch =
                std::env::temp_dir().join(format!("fileflux-dbus-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&scratch)?;
            let config = scratch.join("bus.conf");
            std::fs::write(
                &config,
                format!(
                    "<busconfig><type>session</type>\
                     <listen>unix:path={}/bus</listen><auth>EXTERNAL</auth>\
                     <policy context=\"default\"><allow send_destination=\"*\"/><allow receive_sender=\"*\"/>\
                     <allow own=\"*\"/></policy></busconfig>",
                    scratch.display()
                ),
            )?;
            let mut child = Command::new("dbus-daemon")
                .arg("--config-file")
                .arg(&config)
                .args(["--nofork", "--print-address"])
                .env_remove("DBUS_SESSION_BUS_ADDRESS")
                .stdout(Stdio::piped())
                .spawn()?;
            let mut address = String::new();
            let stdout = child.stdout.take().ok_or("no stdout")?;
            BufReader::new(stdout).read_line(&mut address)?;
            let address = address.trim().to_owned();
            if address.is_empty() {
                return Err("dbus-daemon printed no address".into());
            }
            Ok(Self {
                child,
                address,
                scratch,
            })
        }

        fn endpoint(&self) -> Endpoint {
            Endpoint::Address(self.address.clone())
        }

        /// A client connection to this daemon.
        fn client(&self) -> zbus::Result<Connection> {
            let address = self.address.clone();
            zbus::block_on(async move { Builder::address(address.as_str())?.build().await })
        }
    }

    impl Drop for Daemon {
        fn drop(&mut self) {
            drop(self.child.kill());
            drop(self.child.wait());
            drop(std::fs::remove_dir_all(&self.scratch));
        }
    }

    fn primary(role: Role) -> Result<Primary, Box<dyn std::error::Error>> {
        match role {
            Role::Primary(primary) => Ok(primary),
            other => Err(format!("expected the primary role, got {other:?}").into()),
        }
    }

    const WAIT: Duration = Duration::from_secs(5);

    #[test]
    fn the_first_process_owns_the_name_and_a_second_one_forwards_its_paths() -> TestResult {
        let daemon = Daemon::start("single")?;
        let work = daemon.scratch.join("work");
        std::fs::create_dir_all(work.join("folder"))?;
        std::fs::write(work.join("file.txt"), b"x")?;

        let first = primary(claim(&daemon.endpoint(), &[]))?;
        assert!(first.owns_file_manager(), "the free FileManager1 is taken");

        let second = claim(
            &daemon.endpoint(),
            &[work.join("folder"), work.join("file.txt")],
        );
        assert!(matches!(second, Role::Forwarded), "got {second:?}");
        assert_eq!(
            first.recv_timeout(WAIT),
            Some(Request {
                targets: vec![
                    Show {
                        folder: work.join("folder"),
                        select: vec![]
                    },
                    Show {
                        folder: work.clone(),
                        select: vec![work.join("file.txt")]
                    },
                ],
                properties: false,
            })
        );

        // A launch without paths only raises the running window.
        assert!(matches!(claim(&daemon.endpoint(), &[]), Role::Forwarded));
        assert_eq!(
            first.recv_timeout(WAIT),
            Some(Request {
                targets: vec![],
                properties: false
            })
        );
        assert_eq!(first.try_recv(), None);
        Ok(())
    }

    #[test]
    fn file_manager_calls_reach_the_primary() -> TestResult {
        let daemon = Daemon::start("fm1")?;
        let first = primary(claim(&daemon.endpoint(), &[]))?;
        assert!(first.owns_file_manager());

        let client = daemon.client()?;
        let call = |method: &'static str, uris: Vec<String>| -> zbus::Result<()> {
            zbus::block_on(client.call_method(
                Some(FILE_MANAGER_NAME),
                FILE_MANAGER_PATH,
                Some(FILE_MANAGER_NAME),
                method,
                &(uris, "startup-1".to_owned()),
            ))
            .map(drop)
        };
        call(
            "ShowFolders",
            strings(&["file:///tmp/a%20b", "http://x/y", "file:///tmp/c"]),
        )?;
        assert_eq!(
            first.recv_timeout(WAIT),
            Some(Request {
                targets: vec![show("/tmp/a b", &[]), show("/tmp/c", &[])],
                properties: false
            })
        );
        call(
            "ShowItems",
            strings(&["file:///tmp/a/x", "file:///tmp/a/y", "file:///tmp/b/z"]),
        )?;
        assert_eq!(
            first.recv_timeout(WAIT),
            Some(Request {
                targets: vec![
                    show("/tmp/a", &["/tmp/a/x", "/tmp/a/y"]),
                    show("/tmp/b", &["/tmp/b/z"])
                ],
                properties: false
            })
        );
        call("ShowItemProperties", strings(&["file:///tmp/a/x"]))?;
        assert_eq!(
            first.recv_timeout(WAIT),
            Some(Request {
                targets: vec![show("/tmp/a", &["/tmp/a/x"])],
                properties: true
            })
        );
        Ok(())
    }

    #[test]
    fn file_manager_is_never_taken_from_another_owner() -> TestResult {
        let daemon = Daemon::start("other-owner")?;
        // Another file manager holds the name.
        let other = daemon.client()?;
        // It even allows replacement: still, Fileflux must not ask for it.
        let reply = zbus::block_on(other.request_name_with_flags(
            FILE_MANAGER_NAME,
            RequestNameFlags::DoNotQueue | RequestNameFlags::AllowReplacement,
        ))?;
        assert_eq!(reply, RequestNameReply::PrimaryOwner);

        let ours = primary(claim(&daemon.endpoint(), &[]))?;
        assert!(
            !ours.owns_file_manager(),
            "the existing owner keeps FileManager1 and Fileflux still runs"
        );
        let owner = zbus::block_on(other.call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetNameOwner",
            &FILE_MANAGER_NAME,
        ))?
        .body()
        .deserialize::<String>()?;
        let unique = other.unique_name().map(ToString::to_string);
        assert_eq!(Some(owner), unique, "the dummy owner still holds the name");
        Ok(())
    }

    #[test]
    fn without_a_bus_the_app_opens_normally() {
        let missing = std::env::temp_dir().join(format!("fileflux-no-bus-{}", std::process::id()));
        let endpoint = Endpoint::Address(format!("unix:path={}", missing.display()));
        assert!(matches!(claim(&endpoint, &[p("/tmp")]), Role::Standalone));
        assert!(matches!(
            claim(&Endpoint::Address("not an address".into()), &[]),
            Role::Standalone
        ));
    }
}
