//! Live directory watching for the explorer (inotify through `rustix`).
//!
//! A [`WatchHub`] owns one inotify instance and one dedicated thread that
//! blocks in `poll` on it, however many folders are watched. Callers ask for
//! a [`WatchHandle`] per folder: handles on the same folder share one
//! inotify watch, and dropping the last one removes it. What happened is
//! delivered over a channel as `(folder, FolderEvent)`. [`Debounce`] is the
//! pure decision of when a burst of changes has settled enough to rescan.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustix::event::{EventfdFlags, PollFd, PollFlags, eventfd, poll};
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use rustix::io::Errno;

/// The quiet time after the last change before a rescan is due.
const QUIET: Duration = Duration::from_millis(250);
/// The longest a rescan may be postponed by a continuous stream of changes.
const MAX_DELAY: Duration = Duration::from_millis(1_000);

/// What happened to a watched folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FolderEvent {
    /// A direct child was created, deleted, moved in/out or changed, or the
    /// event queue overflowed so the listing may be stale.
    Changed,
    /// The folder itself was deleted or moved away; its watch is released.
    Gone,
}

/// What one raw inotify event means, before it is routed to folders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Signal {
    /// A child of the watch's folder changed.
    Changed,
    /// The watch's folder vanished or its watch was dropped by the kernel.
    Gone,
    /// The kernel queue overflowed: every folder may be stale.
    Overflow,
}

/// Maps raw inotify flags to a [`Signal`].
fn classify(flags: ReadFlags) -> Signal {
    if flags.contains(ReadFlags::QUEUE_OVERFLOW) {
        Signal::Overflow
    } else if flags.intersects(ReadFlags::DELETE_SELF | ReadFlags::MOVE_SELF | ReadFlags::IGNORED) {
        Signal::Gone
    } else {
        Signal::Changed
    }
}

/// One shared inotify watch and how many handles hold it.
#[derive(Debug)]
struct Entry {
    wd: i32,
    refs: usize,
    /// Tells a handle apart from the handles of an earlier watch of the same
    /// path that was released (the folder vanished and came back).
    serial: u64,
}

/// The watched folders: pure bookkeeping, no file descriptors.
#[derive(Debug, Default)]
struct Registry {
    paths: HashMap<PathBuf, Entry>,
    next_serial: u64,
}

impl Registry {
    /// Registers one more handle on `path`, adding the inotify watch through
    /// `add` only when the path is not watched yet. Returns the serial.
    fn attach(&mut self, path: &Path, add: impl FnOnce() -> io::Result<i32>) -> io::Result<u64> {
        if let Some(entry) = self.paths.get_mut(path) {
            entry.refs += 1;
            return Ok(entry.serial);
        }
        let wd = add()?;
        self.next_serial += 1;
        let serial = self.next_serial;
        self.paths.insert(
            path.to_path_buf(),
            Entry {
                wd,
                refs: 1,
                serial,
            },
        );
        Ok(serial)
    }

    /// Drops one handle of the watch `serial` on `path`. Returns the inotify
    /// descriptor to remove when that was the last handle and no other path
    /// shares it.
    fn detach(&mut self, path: &Path, serial: u64) -> Option<i32> {
        let entry = self.paths.get_mut(path)?;
        if entry.serial != serial {
            return None;
        }
        entry.refs -= 1;
        if entry.refs > 0 {
            return None;
        }
        let wd = entry.wd;
        self.paths.remove(path);
        (!self.paths.values().any(|other| other.wd == wd)).then_some(wd)
    }

    /// The events a raw `signal` on descriptor `wd` means for the watched
    /// folders. A [`Signal::Gone`] releases the folders it reports.
    fn route(&mut self, signal: Signal, wd: i32) -> Vec<(PathBuf, FolderEvent)> {
        match signal {
            Signal::Overflow => self
                .paths
                .keys()
                .map(|path| (path.clone(), FolderEvent::Changed))
                .collect(),
            Signal::Changed => self
                .paths
                .iter()
                .filter(|(_, entry)| entry.wd == wd)
                .map(|(path, _)| (path.clone(), FolderEvent::Changed))
                .collect(),
            Signal::Gone => {
                let gone: Vec<PathBuf> = self
                    .paths
                    .iter()
                    .filter(|(_, entry)| entry.wd == wd)
                    .map(|(path, _)| path.clone())
                    .collect();
                for path in &gone {
                    self.paths.remove(path);
                }
                gone.into_iter()
                    .map(|path| (path, FolderEvent::Gone))
                    .collect()
            }
        }
    }
}

/// What the hub and its thread share.
#[derive(Debug)]
struct Shared {
    inotify: OwnedFd,
    /// Written to wake the thread out of `poll` for shutdown.
    wake: OwnedFd,
    registry: Mutex<Registry>,
    shutdown: AtomicBool,
}

impl Shared {
    fn registry(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One inotify instance on one dedicated thread, watching any number of
/// folders. Dropping the hub stops and joins the thread.
#[derive(Debug)]
pub struct WatchHub {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

/// A hold on one folder's watch; dropping the last hold on a folder removes
/// its inotify watch.
#[derive(Debug)]
pub struct WatchHandle {
    shared: Arc<Shared>,
    path: PathBuf,
    serial: u64,
}

impl WatchHub {
    /// Starts the hub and its thread; events arrive on the returned channel.
    pub fn new() -> io::Result<(Self, Receiver<(PathBuf, FolderEvent)>)> {
        let shared = Arc::new(Shared {
            inotify: inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?,
            wake: eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?,
            registry: Mutex::new(Registry::default()),
            shutdown: AtomicBool::new(false),
        });
        let (sender, receiver) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("fileflux-watch".to_owned())
            .spawn({
                let shared = Arc::clone(&shared);
                move || run(&shared, &sender)
            })?;
        Ok((
            Self {
                shared,
                thread: Some(thread),
            },
            receiver,
        ))
    }

    /// Watches the direct children of `path`, sharing the inotify watch with
    /// every other handle on the same path. Fails when it is not a readable
    /// directory.
    pub fn watch(&self, path: &Path) -> io::Result<WatchHandle> {
        let mask = WatchFlags::CREATE
            | WatchFlags::DELETE
            | WatchFlags::MOVED_FROM
            | WatchFlags::MOVED_TO
            | WatchFlags::ATTRIB
            | WatchFlags::CLOSE_WRITE
            | WatchFlags::DELETE_SELF
            | WatchFlags::MOVE_SELF
            | WatchFlags::ONLYDIR;
        let serial = self.shared.registry().attach(path, || {
            Ok(inotify::add_watch(&self.shared.inotify, path, mask)?)
        })?;
        Ok(WatchHandle {
            shared: Arc::clone(&self.shared),
            path: path.to_path_buf(),
            serial,
        })
    }

    /// How many inotify watches exist (distinct folders, not handles).
    pub fn watch_count(&self) -> usize {
        self.shared.registry().paths.len()
    }
}

impl Drop for WatchHub {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        // Wakes the thread out of `poll`; a failed write leaves it parked,
        // so do not join then.
        if let Err(error) = rustix::io::write(&self.shared.wake, &1u64.to_ne_bytes()) {
            log::warn!("cannot wake the watcher thread: {error}");
            return;
        }
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            log::warn!("the watcher thread panicked");
        }
    }
}

impl WatchHandle {
    /// The folder this handle watches.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        // Removed under the lock: a concurrent `watch` of the same folder
        // would otherwise get this very descriptor back and lose it.
        let mut registry = self.shared.registry();
        if let Some(wd) = registry.detach(&self.path, self.serial) {
            // The kernel may have dropped it already (folder deleted).
            inotify::remove_watch(&self.shared.inotify, wd).ok();
        }
    }
}

/// The hub thread: sleeps in `poll` until inotify has events or the wake fd
/// fires, then routes what it reads to the folders watched.
fn run(shared: &Shared, sender: &Sender<(PathBuf, FolderEvent)>) {
    loop {
        let mut fds = [
            PollFd::new(&shared.inotify, PollFlags::IN),
            PollFd::new(&shared.wake, PollFlags::IN),
        ];
        match poll(&mut fds, None) {
            Ok(_) => {}
            Err(Errno::INTR) => continue,
            Err(error) => {
                log::warn!("inotify poll failed, stopping the watcher: {error}");
                return;
            }
        }
        if shared.shutdown.load(Ordering::SeqCst) {
            return;
        }
        if !drain(shared, sender) {
            return;
        }
    }
}

/// Reads every queued inotify event and sends one event per affected folder
/// (a burst collapses, `Gone` wins over `Changed`). `false` when reading
/// failed and the hub cannot go on.
fn drain(shared: &Shared, sender: &Sender<(PathBuf, FolderEvent)>) -> bool {
    let mut buffer = [MaybeUninit::<u8>::uninit(); 4096];
    let mut reader = inotify::Reader::new(&shared.inotify, &mut buffer);
    let mut batch: BTreeMap<PathBuf, FolderEvent> = BTreeMap::new();
    let mut healthy = true;
    loop {
        match reader.next() {
            Ok(event) => {
                let signal = classify(event.events());
                let wd = event.wd();
                let routed = {
                    let mut registry = shared.registry();
                    let routed = registry.route(signal, wd);
                    if signal == Signal::Gone && !routed.is_empty() {
                        // A moved folder keeps its watch; a deleted one is
                        // already ignored and this fails harmlessly.
                        inotify::remove_watch(&shared.inotify, wd).ok();
                    }
                    routed
                };
                for (path, folder_event) in routed {
                    batch
                        .entry(path)
                        .and_modify(|seen| *seen = (*seen).max(folder_event))
                        .or_insert(folder_event);
                }
            }
            Err(Errno::AGAIN) => break,
            Err(error) => {
                log::warn!("inotify read failed, stopping the watcher: {error}");
                healthy = false;
                break;
            }
        }
    }
    for entry in batch {
        // The receiver is gone when the explorer is; shutdown follows.
        sender.send(entry).ok();
    }
    healthy
}

/// The closest existing directory above `path`, or `path` itself when it
/// still is one. `None` only when not even the root exists.
pub fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|candidate| candidate.is_dir())
        .map(Path::to_path_buf)
}

/// Decides when a burst of change events is quiet enough to rescan.
#[derive(Clone, Copy, Debug, Default)]
pub struct Debounce {
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Debounce {
    /// Records a change observed at `now`.
    pub fn note(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    /// Whether a rescan is due: changes are pending and either nothing new
    /// arrived for the quiet period or the maximum delay has passed.
    pub fn ready(&self, now: Instant) -> bool {
        match (self.first, self.last) {
            (Some(first), Some(last)) => {
                now.saturating_duration_since(last) >= QUIET
                    || now.saturating_duration_since(first) >= MAX_DELAY
            }
            _ => false,
        }
    }

    /// Forgets the pending changes, after the rescan started.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, Instant};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const LONG: Duration = Duration::from_secs(3);
    const SHORT: Duration = Duration::from_millis(300);

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(label: &str) -> Result<Self, std::io::Error> {
            let path = std::env::temp_dir().join(format!(
                "fileflux-watch-{label}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            fs::remove_dir_all(&path).ok();
            fs::create_dir_all(&path)?;
            Ok(Self(path))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    type Events = Receiver<(PathBuf, FolderEvent)>;

    fn hub() -> Result<(WatchHub, Events), std::io::Error> {
        WatchHub::new()
    }

    /// The next event, or `None` when nothing arrives within `wait`.
    fn next(rx: &Events, wait: Duration) -> Option<(PathBuf, FolderEvent)> {
        rx.recv_timeout(wait).ok()
    }

    #[test]
    fn create_rename_and_delete_are_reported_for_their_folder_only() -> TestResult {
        let scratch = Scratch::new("crud")?;
        let one = scratch.0.join("one");
        let two = scratch.0.join("two");
        fs::create_dir_all(&one)?;
        fs::create_dir_all(&two)?;
        let (hub, rx) = hub()?;
        let _a = hub.watch(&one)?;
        let _b = hub.watch(&two)?;
        assert_eq!(hub.watch_count(), 2, "one watch per distinct folder");

        fs::write(one.join("a.txt"), b"x")?;
        assert_eq!(next(&rx, LONG), Some((one.clone(), FolderEvent::Changed)));
        fs::rename(one.join("a.txt"), one.join("b.txt"))?;
        assert_eq!(next(&rx, LONG), Some((one.clone(), FolderEvent::Changed)));
        fs::remove_file(one.join("b.txt"))?;
        assert_eq!(next(&rx, LONG), Some((one.clone(), FolderEvent::Changed)));
        // Drain what is left of the burst, then prove the other folder was
        // never mentioned.
        while let Some((path, _)) = next(&rx, SHORT) {
            assert_eq!(path, one, "the untouched folder reports nothing");
        }
        fs::write(two.join("c.txt"), b"x")?;
        assert_eq!(next(&rx, LONG), Some((two.clone(), FolderEvent::Changed)));
        Ok(())
    }

    #[test]
    fn handles_on_one_folder_share_one_watch_until_the_last_is_dropped() -> TestResult {
        let scratch = Scratch::new("refs")?;
        let (hub, rx) = hub()?;
        let first = hub.watch(&scratch.0)?;
        let second = hub.watch(&scratch.0)?;
        assert_eq!(hub.watch_count(), 1, "two tabs, one inotify watch");

        drop(first);
        assert_eq!(hub.watch_count(), 1, "the other handle keeps the watch");
        fs::write(scratch.0.join("a.txt"), b"x")?;
        assert_eq!(
            next(&rx, LONG),
            Some((scratch.0.clone(), FolderEvent::Changed)),
            "events still flow"
        );
        while next(&rx, SHORT).is_some() {}

        drop(second);
        assert_eq!(hub.watch_count(), 0, "the last drop removes the watch");
        fs::write(scratch.0.join("b.txt"), b"x")?;
        assert_eq!(next(&rx, SHORT), None, "a removed watch reports nothing");
        Ok(())
    }

    #[test]
    fn a_deleted_folder_is_reported_gone_once_and_released() -> TestResult {
        let scratch = Scratch::new("gone")?;
        let victim = scratch.0.join("victim");
        fs::create_dir(&victim)?;
        let (hub, rx) = hub()?;
        let handle = hub.watch(&victim)?;
        fs::remove_dir(&victim)?;
        assert_eq!(next(&rx, LONG), Some((victim.clone(), FolderEvent::Gone)));
        assert_eq!(
            next(&rx, SHORT),
            None,
            "reported once, not per kernel event"
        );
        assert_eq!(hub.watch_count(), 0, "the watch is released");

        // The folder comes back: a new watch is its own, and dropping the
        // handle of the dead one must not take it away.
        fs::create_dir(&victim)?;
        let fresh = hub.watch(&victim)?;
        drop(handle);
        assert_eq!(hub.watch_count(), 1, "the stale handle left the new watch");
        drop(fresh);
        assert_eq!(hub.watch_count(), 0);
        Ok(())
    }

    #[test]
    fn a_moved_folder_is_reported_gone() -> TestResult {
        let scratch = Scratch::new("moved")?;
        let from = scratch.0.join("from");
        fs::create_dir(&from)?;
        let (hub, rx) = hub()?;
        let _handle = hub.watch(&from)?;
        fs::rename(&from, scratch.0.join("to"))?;
        assert_eq!(next(&rx, LONG), Some((from, FolderEvent::Gone)));
        assert_eq!(hub.watch_count(), 0);
        Ok(())
    }

    #[test]
    fn watching_a_missing_directory_or_a_file_fails() -> TestResult {
        let scratch = Scratch::new("missing")?;
        let (hub, _rx) = hub()?;
        assert!(hub.watch(&scratch.0.join("nope")).is_err());
        fs::write(scratch.0.join("file"), b"x")?;
        assert!(hub.watch(&scratch.0.join("file")).is_err());
        assert_eq!(hub.watch_count(), 0, "a failed watch leaves nothing behind");
        Ok(())
    }

    #[test]
    fn dropping_the_hub_stops_its_thread() -> TestResult {
        let scratch = Scratch::new("shutdown")?;
        let (hub, rx) = hub()?;
        let handle = hub.watch(&scratch.0)?;
        let started = Instant::now();
        drop(hub);
        // The thread owns the sender: the channel disconnects only once the
        // thread has ended, and the drop joined it.
        assert!(
            matches!(
                rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Disconnected)
            ),
            "the watcher thread is gone"
        );
        assert!(
            started.elapsed() < LONG,
            "woken by the wake fd, not a timeout"
        );
        drop(handle); // outliving the hub is harmless
        Ok(())
    }

    #[test]
    fn raw_flags_map_to_signals() {
        assert_eq!(classify(ReadFlags::CREATE), Signal::Changed);
        assert_eq!(classify(ReadFlags::CLOSE_WRITE), Signal::Changed);
        assert_eq!(classify(ReadFlags::MOVED_TO), Signal::Changed);
        assert_eq!(classify(ReadFlags::DELETE_SELF), Signal::Gone);
        assert_eq!(classify(ReadFlags::MOVE_SELF), Signal::Gone);
        assert_eq!(classify(ReadFlags::IGNORED), Signal::Gone);
        assert_eq!(classify(ReadFlags::QUEUE_OVERFLOW), Signal::Overflow);
    }

    #[test]
    fn an_overflow_reports_every_watched_folder_changed() -> TestResult {
        let mut registry = Registry::default();
        registry.attach(Path::new("/a"), || Ok(1))?;
        registry.attach(Path::new("/b"), || Ok(2))?;
        let mut events = registry.route(Signal::Overflow, -1);
        events.sort();
        assert_eq!(
            events,
            [
                (PathBuf::from("/a"), FolderEvent::Changed),
                (PathBuf::from("/b"), FolderEvent::Changed)
            ]
        );
        assert_eq!(registry.paths.len(), 2, "an overflow releases nothing");
        Ok(())
    }

    #[test]
    fn paths_that_share_an_inode_share_the_descriptor() -> TestResult {
        let mut registry = Registry::default();
        let real = registry.attach(Path::new("/real"), || Ok(7))?;
        let link = registry.attach(Path::new("/link"), || Ok(7))?;
        assert_eq!(
            registry.detach(Path::new("/real"), real),
            None,
            "still shared"
        );
        assert_eq!(registry.detach(Path::new("/link"), link), Some(7));
        let mut again = Registry::default();
        again.attach(Path::new("/real"), || Ok(7))?;
        again.attach(Path::new("/link"), || Ok(7))?;
        let gone = again.route(Signal::Gone, 7);
        assert_eq!(gone.len(), 2, "both names of the folder are gone");
        assert!(again.paths.is_empty());
        Ok(())
    }

    #[test]
    fn the_nearest_existing_ancestor_skips_deleted_levels() -> TestResult {
        let scratch = Scratch::new("ancestor")?;
        let deep = scratch.0.join("a").join("b");
        assert_eq!(
            nearest_existing_ancestor(&deep),
            Some(scratch.0.clone()),
            "the missing levels are skipped"
        );
        assert_eq!(
            nearest_existing_ancestor(&scratch.0),
            Some(scratch.0.clone())
        );
        Ok(())
    }

    #[test]
    fn debounce_waits_for_a_quiet_period_but_never_starves() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut debounce = Debounce::default();
        assert!(!debounce.ready(t0), "nothing pending");
        debounce.note(t0);
        assert!(
            !debounce.ready(t0 + ms(100)),
            "still inside the quiet window"
        );
        debounce.note(t0 + ms(200));
        assert!(
            !debounce.ready(t0 + ms(400)),
            "a new event restarts the window"
        );
        assert!(debounce.ready(t0 + ms(460)), "quiet for 250 ms");
        debounce.reset();
        assert!(
            !debounce.ready(t0 + ms(10_000)),
            "reset clears the pending change"
        );

        // A steady stream of events still refreshes after the cap.
        let mut busy = Debounce::default();
        for step in 0..30 {
            busy.note(t0 + ms(step * 100));
        }
        assert!(
            busy.ready(t0 + ms(2_950)),
            "the maximum delay forces a refresh"
        );
    }
}
