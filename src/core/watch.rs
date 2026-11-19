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

/// What every folder is watched for.
const WATCH_MASK: WatchFlags = WatchFlags::CREATE
    .union(WatchFlags::DELETE)
    .union(WatchFlags::MOVED_FROM)
    .union(WatchFlags::MOVED_TO)
    .union(WatchFlags::ATTRIB)
    .union(WatchFlags::CLOSE_WRITE)
    .union(WatchFlags::DELETE_SELF)
    .union(WatchFlags::MOVE_SELF)
    .union(WatchFlags::ONLYDIR);

/// The watched folders: pure bookkeeping, no file descriptors.
#[derive(Debug, Default)]
struct Registry {
    paths: HashMap<PathBuf, Entry>,
    next_serial: u64,
    /// Folders whose entry `attach` found replaced and dropped: their
    /// holders still have to be told [`FolderEvent::Gone`].
    evicted: Vec<PathBuf>,
    /// Kernel watches nobody refers to any more, to be removed.
    dead_watches: Vec<i32>,
}

impl Registry {
    /// Registers one more handle on `path`. `add` asks the kernel to watch
    /// the folder at `path` now and returns its descriptor: the kernel
    /// returns the existing descriptor for an inode it already watches and a
    /// new one otherwise, so the descriptor is the folder's identity. An
    /// entry holding another descriptor is dead (the folder was replaced
    /// behind the hub's back), so it is evicted rather than shared. `add`
    /// must run under the same lock as this call. Returns the serial.
    fn attach(&mut self, path: &Path, add: impl FnOnce() -> io::Result<i32>) -> io::Result<u64> {
        let wd = add()?;
        if let Some(entry) = self.paths.get_mut(path) {
            if entry.wd == wd {
                entry.refs += 1;
                return Ok(entry.serial);
            }
            self.evict(path);
        }
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

    /// Removes the entry of `path`, remembering its holders still have to be
    /// told and its kernel watch (unless another path shares it) removed.
    fn evict(&mut self, path: &Path) -> Option<i32> {
        let entry = self.paths.remove(path)?;
        self.evicted.push(path.to_path_buf());
        let unshared = !self.paths.values().any(|other| other.wd == entry.wd);
        if unshared {
            self.dead_watches.push(entry.wd);
        }
        unshared.then_some(entry.wd)
    }

    /// The folders `attach` evicted since the last call.
    fn take_evicted(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.evicted)
    }

    /// The kernel watches to remove since the last call.
    fn take_dead_watches(&mut self) -> Vec<i32> {
        std::mem::take(&mut self.dead_watches)
    }

    /// Whether a live entry holds the kernel watch `wd`.
    fn owns(&self, wd: i32) -> bool {
        self.paths.values().any(|entry| entry.wd == wd)
    }

    /// Releases the listed `(path, serial)` watches (those still current):
    /// each reports [`FolderEvent::Gone`]. Also returns the kernel watches
    /// to remove.
    fn release_stale(
        &mut self,
        stale: &[(PathBuf, u64)],
    ) -> (Vec<(PathBuf, FolderEvent)>, Vec<i32>) {
        let mut events = Vec::new();
        let mut removals = Vec::new();
        for (path, serial) in stale {
            if self
                .paths
                .get(path)
                .is_none_or(|entry| entry.serial != *serial)
            {
                continue;
            }
            if let Some(wd) = self.evict(path) {
                removals.push(wd);
            }
            events.push((path.clone(), FolderEvent::Gone));
        }
        // These holders are told right here, not through the queue.
        self.evicted
            .retain(|path| !events.iter().any(|(done, _)| done == path));
        self.dead_watches.retain(|wd| !removals.contains(wd));
        (events, removals)
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
        let serial = {
            // The kernel watch is added under the lock, so the descriptor it
            // returns is the folder actually watched and nothing else can
            // remove or take it before the entry records it.
            let mut registry = self.shared.registry();
            let attached = registry.attach(path, || {
                Ok(inotify::add_watch(&self.shared.inotify, path, WATCH_MASK)?)
            });
            let evicted = !registry.evicted.is_empty();
            for wd in registry.take_dead_watches() {
                inotify::remove_watch(&self.shared.inotify, wd).ok();
            }
            if evicted {
                // The thread tells the holders of the replaced folder.
                rustix::io::write(&self.shared.wake, &1u64.to_ne_bytes()).ok();
            }
            attached?
        };
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

    /// Whether the watch this handle holds still exists. It does not once
    /// the folder was deleted, moved or replaced: the hub released it, and
    /// only a new [`WatchHub::watch`] watches the path again.
    pub fn is_live(&self) -> bool {
        self.shared
            .registry()
            .paths
            .get(&self.path)
            .is_some_and(|entry| entry.serial == self.serial)
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
        // Resets the wake counter (a watch evicted a replaced folder). The
        // shutdown flag is read only after it: a shutdown wake consumed here
        // must not be missed, or the join in `Drop` would wait forever.
        let mut counter = [0u8; 8];
        rustix::io::read(&shared.wake, &mut counter).ok();
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
                let revalidated = if signal == Signal::Overflow {
                    revalidate(shared)
                } else {
                    Vec::new()
                };
                for (path, folder_event) in routed.into_iter().chain(revalidated) {
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
    let evicted = shared.registry().take_evicted();
    for path in evicted {
        batch.insert(path, FolderEvent::Gone);
    }
    for entry in batch {
        // The receiver is gone when the explorer is; shutdown follows.
        sender.send(entry).ok();
    }
    healthy
}

/// After a queue overflow the kernel may have dropped the `Gone` of any
/// folder: checks every watched folder is still the one watched, and
/// releases (with a `Gone`) those deleted or replaced. Re-adding the watch
/// returns the descriptor of whatever is at the path now: the entry's own
/// when unchanged, another (a stray, removed unless an entry owns it) when
/// the folder was replaced, an error when it is gone. Runs under the
/// registry lock so no removal races a concurrent `watch`.
fn revalidate(shared: &Shared) -> Vec<(PathBuf, FolderEvent)> {
    let mut registry = shared.registry();
    let mut stale = Vec::new();
    let mut strays = Vec::new();
    for (path, entry) in &registry.paths {
        match inotify::add_watch(&shared.inotify, path, WATCH_MASK) {
            Ok(wd) if wd == entry.wd => {}
            Ok(wd) => {
                strays.push(wd);
                stale.push((path.clone(), entry.serial));
            }
            Err(_) => stale.push((path.clone(), entry.serial)),
        }
    }
    let (events, removals) = registry.release_stale(&stale);
    for wd in removals {
        inotify::remove_watch(&shared.inotify, wd).ok();
    }
    for wd in strays {
        if !registry.owns(wd) {
            inotify::remove_watch(&shared.inotify, wd).ok();
        }
    }
    events
}

/// The status notice for a watcher channel that reported `error`, or `None`
/// when it is not a stop or was already announced.
pub fn stop_notice(error: mpsc::TryRecvError, already_announced: bool) -> Option<&'static str> {
    (error == mpsc::TryRecvError::Disconnected && !already_announced)
        .then_some("Live updates stopped; press Ctrl+R to reload")
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

// --- device events -----------------------------------------------------------

/// Quiet time after the last device event before the list is re-read.
pub const DEVICE_QUIET: Duration = Duration::from_millis(300);
/// The longest a device re-read may be postponed by a stream of events.
pub const DEVICE_MAX_DELAY: Duration = Duration::from_millis(1_500);
/// Extra re-reads after a USB device appears: gvfs mounts a phone, and
/// udisks a stick, a moment after the kernel node shows up.
pub const USB_FOLLOW_UPS: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(5)];
/// How often the list is re-read when a watch could not be established.
pub const SAFETY_POLL: Duration = Duration::from_secs(30);

/// Where a change that may alter the device list was seen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeviceSource {
    /// A node appeared or went away in `/dev`.
    Dev,
    /// A link appeared or went away in `/dev/disk/by-id`.
    DiskById,
    /// A USB device node appeared or went away under `/dev/bus/usb`.
    Usb,
    /// The kernel mount table changed (`/proc/self/mountinfo`).
    Mounts,
    /// A gvfs mount appeared or went away.
    Gvfs,
}

/// Decides when a burst of device events is quiet enough to re-read: the
/// pure half of the device watcher.
#[derive(Clone, Debug, Default)]
pub struct RefreshGate {
    first: Option<Instant>,
    last: Option<Instant>,
    follow_ups: Vec<Instant>,
}

impl RefreshGate {
    /// Records an event from `source` seen at `now`. A USB event also
    /// schedules the follow-up reads.
    pub fn note(&mut self, now: Instant, source: DeviceSource) {
        self.first.get_or_insert(now);
        self.last = Some(now);
        if source == DeviceSource::Usb {
            self.follow_ups = USB_FOLLOW_UPS.iter().map(|after| now + *after).collect();
        }
    }

    /// Whether a re-read is due at `now`: events are pending and either
    /// nothing new arrived for [`DEVICE_QUIET`] or the oldest one has
    /// waited [`DEVICE_MAX_DELAY`], or a follow-up read has come due.
    pub fn ready(&self, now: Instant) -> bool {
        let burst = match (self.first, self.last) {
            (Some(first), Some(last)) => {
                now.saturating_duration_since(last) >= DEVICE_QUIET
                    || now.saturating_duration_since(first) >= DEVICE_MAX_DELAY
            }
            _ => false,
        };
        burst || self.follow_ups.iter().any(|at| *at <= now)
    }

    /// Whether anything is waiting to be re-read.
    pub fn pending(&self) -> bool {
        self.first.is_some() || !self.follow_ups.is_empty()
    }

    /// Marks the re-read as started at `now`: the events seen so far and
    /// the follow-ups already due are consumed; events that arrive while it
    /// runs are not lost.
    pub fn start(&mut self, now: Instant) {
        self.first = None;
        self.last = None;
        self.follow_ups.retain(|at| *at > now);
    }
}

/// Whether the slow fallback poll is due: only while a watch is missing
/// (`degraded`), every [`SAFETY_POLL`].
pub fn safety_poll_due(last: Option<Instant>, now: Instant, degraded: bool) -> bool {
    degraded && last.is_none_or(|last| now.saturating_duration_since(last) >= SAFETY_POLL)
}

/// What the kernel's answer on the open mount table means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountPoll {
    /// Nothing changed since the last answer.
    Quiet,
    /// A mount or unmount happened: the kernel raises `POLLPRI | POLLERR`.
    Changed,
    /// The descriptor is not usable any more.
    Broken,
}

/// Reads the `poll` result on `/proc/self/mountinfo`.
pub fn mount_poll(revents: PollFlags) -> MountPoll {
    if revents.intersects(PollFlags::HUP | PollFlags::NVAL) {
        MountPoll::Broken
    } else if revents.intersects(PollFlags::PRI | PollFlags::ERR) {
        MountPoll::Changed
    } else {
        MountPoll::Quiet
    }
}

/// The places the device watcher looks at.
#[derive(Clone, Debug)]
pub struct DeviceRoots {
    /// Block device nodes appear here.
    pub dev: PathBuf,
    /// The stable links to them; optional.
    pub by_id: PathBuf,
    /// USB device nodes, one folder per bus; optional.
    pub usb: PathBuf,
    /// The kernel mount table, polled for changes.
    pub mountinfo: PathBuf,
    /// The gvfs directory, when there can be one. Its parent is watched for
    /// it to appear.
    pub gvfs: Option<PathBuf>,
}

impl DeviceRoots {
    /// The system's own, with the gvfs directory of `$XDG_RUNTIME_DIR`.
    pub fn system() -> Self {
        Self {
            dev: PathBuf::from("/dev"),
            by_id: PathBuf::from("/dev/disk/by-id"),
            usb: PathBuf::from("/dev/bus/usb"),
            mountinfo: PathBuf::from("/proc/self/mountinfo"),
            gvfs: super::places::gvfs_root_from_env(),
        }
    }
}

/// What a kernel watch of the device watcher stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Armed {
    /// Entries of this folder are the source's news.
    Source(DeviceSource),
    /// `/dev/bus/usb`: a new bus folder is watched too.
    UsbRoot,
    /// The gvfs directory.
    Gvfs,
    /// The gvfs directory's parent: only the entry `gvfs` matters.
    Runtime,
}

/// What every device folder is watched for.
const DEVICE_MASK: WatchFlags = WatchFlags::CREATE
    .union(WatchFlags::DELETE)
    .union(WatchFlags::MOVED_FROM)
    .union(WatchFlags::MOVED_TO)
    .union(WatchFlags::DELETE_SELF)
    .union(WatchFlags::MOVE_SELF)
    .union(WatchFlags::ONLYDIR);

fn watch_dir(inotify: &OwnedFd, path: &Path) -> io::Result<i32> {
    Ok(inotify::add_watch(inotify, path, DEVICE_MASK)?)
}

/// Watches `usb` and every bus folder in it that exists now.
fn arm_usb(inotify: &OwnedFd, usb: &Path, watches: &mut HashMap<i32, Armed>) {
    let Ok(wd) = watch_dir(inotify, usb) else {
        return;
    };
    watches.insert(wd, Armed::UsbRoot);
    for entry in std::fs::read_dir(usb).into_iter().flatten().flatten() {
        if let Ok(wd) = watch_dir(inotify, &entry.path()) {
            watches.insert(wd, Armed::Source(DeviceSource::Usb));
        }
    }
}

/// What the device watcher's thread owns.
struct DeviceLoop {
    inotify: OwnedFd,
    wake: OwnedFd,
    mountinfo: Option<std::fs::File>,
    watches: HashMap<i32, Armed>,
    roots: DeviceRoots,
    sender: Sender<DeviceSource>,
    shared: Arc<DeviceShared>,
}

/// What the watcher handle and its thread share.
#[derive(Debug, Default)]
struct DeviceShared {
    shutdown: AtomicBool,
    degraded: AtomicBool,
}

/// One inotify instance plus the mount-table poll on one dedicated thread,
/// reporting [`DeviceSource`]s whenever something that may change the device
/// list happened. Dropping the watcher stops and joins the thread.
#[derive(Debug)]
pub struct DeviceWatcher {
    shared: Arc<DeviceShared>,
    wake: Arc<OwnedFd>,
    thread: Option<JoinHandle<()>>,
}

impl DeviceWatcher {
    /// Starts watching `roots`; events arrive on the returned channel.
    /// `Err` only when inotify itself is unavailable. A watch that cannot be
    /// established leaves the watcher [`degraded`](Self::degraded) (for
    /// `dev` and the mount table; the others are optional).
    pub fn new(roots: &DeviceRoots) -> io::Result<(Self, Receiver<DeviceSource>)> {
        let inotify = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?;
        let wake = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?;
        let mut watches = HashMap::new();
        let mut degraded = false;

        match watch_dir(&inotify, &roots.dev) {
            Ok(wd) => {
                watches.insert(wd, Armed::Source(DeviceSource::Dev));
            }
            Err(error) => {
                log::warn!("cannot watch {}: {error}", roots.dev.display());
                degraded = true;
            }
        }
        if let Ok(wd) = watch_dir(&inotify, &roots.by_id) {
            watches.insert(wd, Armed::Source(DeviceSource::DiskById));
        }
        arm_usb(&inotify, &roots.usb, &mut watches);
        if let Some(gvfs) = &roots.gvfs {
            if let Ok(wd) = watch_dir(&inotify, gvfs) {
                watches.insert(wd, Armed::Gvfs);
            }
            if let Some(runtime) = gvfs.parent()
                && let Ok(wd) = watch_dir(&inotify, runtime)
            {
                watches.insert(wd, Armed::Runtime);
            }
        }
        let mountinfo = match std::fs::File::open(&roots.mountinfo) {
            Ok(file) => Some(file),
            Err(error) => {
                log::warn!("cannot watch {}: {error}", roots.mountinfo.display());
                degraded = true;
                None
            }
        };

        let shared = Arc::new(DeviceShared {
            shutdown: AtomicBool::new(false),
            degraded: AtomicBool::new(degraded),
        });
        let wake = Arc::new(wake);
        let (sender, receiver) = mpsc::channel();
        let state = DeviceLoop {
            inotify,
            wake: wake.try_clone()?,
            mountinfo,
            watches,
            roots: roots.clone(),
            sender,
            shared: Arc::clone(&shared),
        };
        let thread = std::thread::Builder::new()
            .name("fileflux-devices".to_owned())
            .spawn(move || state.run())?;
        Ok((
            Self {
                shared,
                wake,
                thread: Some(thread),
            },
            receiver,
        ))
    }

    /// Whether a watch that matters could not be established or was lost:
    /// the caller then falls back to the slow poll.
    pub fn degraded(&self) -> bool {
        self.shared.degraded.load(Ordering::SeqCst)
    }
}

impl Drop for DeviceWatcher {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        // A failed write leaves the thread parked: do not join then.
        if let Err(error) = rustix::io::write(&*self.wake, &1u64.to_ne_bytes()) {
            log::warn!("cannot wake the device watcher thread: {error}");
            return;
        }
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            log::warn!("the device watcher thread panicked");
        }
    }
}

impl DeviceLoop {
    /// Sleeps in `poll` until inotify, the mount table or the wake fd has
    /// something, then reports what it means.
    fn run(mut self) {
        loop {
            let mut fds = vec![
                PollFd::new(&self.inotify, PollFlags::IN),
                PollFd::new(&self.wake, PollFlags::IN),
            ];
            if let Some(mountinfo) = &self.mountinfo {
                fds.push(PollFd::new(mountinfo, PollFlags::PRI));
            }
            match poll(&mut fds, None) {
                Ok(_) => {}
                Err(Errno::INTR) => continue,
                Err(error) => {
                    log::warn!("device poll failed, stopping the device watcher: {error}");
                    self.shared.degraded.store(true, Ordering::SeqCst);
                    return;
                }
            }
            let mounts = fds.get(2).map(|fd| mount_poll(fd.revents()));
            drop(fds);
            let mut counter = [0u8; 8];
            rustix::io::read(&self.wake, &mut counter).ok();
            if self.shared.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let mut batch: Vec<DeviceSource> = Vec::new();
            match mounts {
                Some(MountPoll::Changed) => batch.push(DeviceSource::Mounts),
                Some(MountPoll::Broken) => {
                    log::warn!("the mount table is no longer watchable");
                    self.mountinfo = None;
                    self.shared.degraded.store(true, Ordering::SeqCst);
                }
                Some(MountPoll::Quiet) | None => {}
            }
            let healthy = self.drain(&mut batch);
            for source in batch {
                // The receiver is gone when the explorer is.
                self.sender.send(source).ok();
            }
            if !healthy {
                self.shared.degraded.store(true, Ordering::SeqCst);
                return;
            }
        }
    }

    /// Reads every queued inotify event into `batch` (one entry per
    /// source). `false` when reading failed and the watcher cannot go on.
    fn drain(&mut self, batch: &mut Vec<DeviceSource>) -> bool {
        let mut events: Vec<(i32, ReadFlags, Option<Vec<u8>>)> = Vec::new();
        let mut healthy = true;
        {
            let mut buffer = [MaybeUninit::<u8>::uninit(); 4096];
            let mut reader = inotify::Reader::new(&self.inotify, &mut buffer);
            loop {
                match reader.next() {
                    Ok(event) => events.push((
                        event.wd(),
                        event.events(),
                        event.file_name().map(|name| name.to_bytes().to_vec()),
                    )),
                    Err(Errno::AGAIN) => break,
                    Err(error) => {
                        log::warn!("device inotify read failed, stopping: {error}");
                        healthy = false;
                        break;
                    }
                }
            }
        }
        let push = |source: DeviceSource, batch: &mut Vec<DeviceSource>| {
            if !batch.contains(&source) {
                batch.push(source);
            }
        };
        for (wd, flags, name) in events {
            if flags.contains(ReadFlags::QUEUE_OVERFLOW) {
                // Events were lost: everything may have changed.
                for source in [
                    DeviceSource::Dev,
                    DeviceSource::DiskById,
                    DeviceSource::Usb,
                    DeviceSource::Gvfs,
                ] {
                    push(source, batch);
                }
                continue;
            }
            let Some(armed) = self.watches.get(&wd).copied() else {
                continue;
            };
            if flags.contains(ReadFlags::IGNORED) {
                // The kernel dropped the watch: its folder is gone.
                self.watches.remove(&wd);
            }
            match armed {
                Armed::Source(source) => push(source, batch),
                Armed::Gvfs => push(DeviceSource::Gvfs, batch),
                Armed::UsbRoot => {
                    if let Some(name) = &name
                        && flags.intersects(ReadFlags::CREATE | ReadFlags::MOVED_TO)
                        && let Ok(bus) = std::str::from_utf8(name)
                        && let Ok(wd) = watch_dir(&self.inotify, &self.roots.usb.join(bus))
                    {
                        self.watches.insert(wd, Armed::Source(DeviceSource::Usb));
                    }
                    push(DeviceSource::Usb, batch);
                }
                Armed::Runtime => {
                    if name.as_deref() != Some(b"gvfs".as_slice()) {
                        continue;
                    }
                    if flags.intersects(ReadFlags::CREATE | ReadFlags::MOVED_TO)
                        && let Some(gvfs) = &self.roots.gvfs
                        && let Ok(wd) = watch_dir(&self.inotify, gvfs)
                    {
                        self.watches.insert(wd, Armed::Gvfs);
                    }
                    push(DeviceSource::Gvfs, batch);
                }
            }
        }
        healthy
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
    fn a_handle_knows_whether_its_watch_is_still_live() -> TestResult {
        let scratch = Scratch::new("live")?;
        let victim = scratch.0.join("victim");
        fs::create_dir(&victim)?;
        let (hub, rx) = hub()?;
        let handle = hub.watch(&victim)?;
        assert!(handle.is_live());
        fs::remove_dir(&victim)?;
        assert_eq!(next(&rx, LONG), Some((victim.clone(), FolderEvent::Gone)));
        assert!(!handle.is_live(), "released by the Gone");
        fs::create_dir(&victim)?;
        let fresh = hub.watch(&victim)?;
        assert!(fresh.is_live());
        assert!(!handle.is_live(), "a newer watch does not revive the old");
        Ok(())
    }

    #[test]
    fn attach_never_bumps_an_entry_of_a_replaced_folder() -> TestResult {
        let mut registry = Registry::default();
        let old = registry.attach(Path::new("/p"), || Ok(3))?;
        let fresh = registry.attach(Path::new("/p"), || Ok(4))?;
        assert_ne!(old, fresh, "a new watch, not a bumped one");
        let entry = registry.paths.get(Path::new("/p")).ok_or("entry")?;
        assert_eq!(entry.refs, 1);
        assert_eq!(entry.wd, 4);
        assert_eq!(registry.take_dead_watches(), [3], "the old kernel watch");
        assert_eq!(registry.take_evicted(), [PathBuf::from("/p")]);
        assert_eq!(registry.detach(Path::new("/p"), old), None);
        assert_eq!(registry.paths.len(), 1, "the old handle cannot drop it");
        Ok(())
    }

    #[test]
    fn stale_entries_are_released_with_a_gone() -> TestResult {
        let mut registry = Registry::default();
        let keep = registry.attach(Path::new("/keep"), || Ok(1))?;
        let lost = registry.attach(Path::new("/lost"), || Ok(2))?;
        let (events, removals) = registry.release_stale(&[(PathBuf::from("/lost"), lost)]);
        assert_eq!(events, [(PathBuf::from("/lost"), FolderEvent::Gone)]);
        assert_eq!(removals, [2]);
        assert!(registry.paths.contains_key(Path::new("/keep")));
        assert_eq!(
            registry
                .paths
                .get(Path::new("/keep"))
                .map(|entry| entry.serial),
            Some(keep)
        );
        // A newer watch of the same path is not the stale one.
        let again = registry.release_stale(&[(PathBuf::from("/lost"), lost)]);
        assert!(again.0.is_empty() && again.1.is_empty());
        Ok(())
    }

    #[test]
    fn an_overflow_releases_deleted_folders_and_a_new_watch_is_fresh() -> TestResult {
        let scratch = Scratch::new("overflow")?;
        let busy = scratch.0.join("busy");
        let victim = scratch.0.join("victim");
        fs::create_dir(&busy)?;
        fs::create_dir(&victim)?;
        let (hub, rx) = hub()?;
        let _busy = hub.watch(&busy)?;
        let _victim = hub.watch(&victim)?;
        {
            // Stall the hub thread so the kernel queue fills up.
            let _stall = hub.shared.registry();
            for index in 0..10_000 {
                fs::write(busy.join(format!("f{index}")), b"x")?;
            }
            fs::remove_dir_all(&victim)?;
        }
        let mut seen = Vec::new();
        while let Some(event) = next(&rx, LONG) {
            let done = event == (victim.clone(), FolderEvent::Gone);
            seen.push(event);
            if done {
                break;
            }
        }
        assert!(
            seen.contains(&(victim.clone(), FolderEvent::Gone)),
            "the lost Gone is recovered: {seen:?}"
        );
        assert_eq!(hub.watch_count(), 1, "only the busy folder stays watched");

        fs::create_dir(&victim)?;
        let fresh = hub.watch(&victim)?;
        assert!(fresh.is_live());
        while next(&rx, SHORT).is_some() {}
        fs::write(victim.join("x"), b"x")?;
        assert_eq!(
            next(&rx, LONG),
            Some((victim.clone(), FolderEvent::Changed))
        );
        Ok(())
    }

    #[test]
    fn a_folder_replaced_behind_the_hub_gets_a_gone_and_a_fresh_watch() -> TestResult {
        let scratch = Scratch::new("replaced")?;
        let victim = scratch.0.join("victim");
        fs::create_dir(&victim)?;
        let (hub, rx) = hub()?;
        let old = hub.watch(&victim)?;
        {
            let _stall = hub.shared.registry();
            fs::remove_dir(&victim)?;
            fs::create_dir(&victim)?;
        }
        // Held again before the Gone is processed or after: either way the
        // watch must end up fresh and the old holders told.
        let fresh = hub.watch(&victim)?;
        assert!(fresh.is_live());
        let mut gone = false;
        while let Some(event) = next(&rx, SHORT) {
            gone |= event == (victim.clone(), FolderEvent::Gone);
        }
        assert!(gone, "holders of the replaced folder are told");
        assert!(!old.is_live());
        assert!(fresh.is_live(), "the fresh watch survives the late Gone");
        fs::write(victim.join("x"), b"x")?;
        assert_eq!(
            next(&rx, LONG),
            Some((victim.clone(), FolderEvent::Changed))
        );
        Ok(())
    }

    #[test]
    fn only_a_closed_channel_announces_the_stop_once() {
        use std::sync::mpsc::TryRecvError;
        assert!(stop_notice(TryRecvError::Disconnected, false).is_some());
        assert_eq!(
            stop_notice(TryRecvError::Disconnected, false),
            Some("Live updates stopped; press Ctrl+R to reload")
        );
        assert_eq!(stop_notice(TryRecvError::Disconnected, true), None);
        assert_eq!(stop_notice(TryRecvError::Empty, false), None);
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

#[cfg(test)]
mod vprobes {
    use super::*;
    use std::fs;
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};
    type R = Result<(), Box<dyn std::error::Error>>;

    fn dir(label: &str) -> io::Result<PathBuf> {
        let p = std::env::temp_dir().join(format!("vp-{label}-{}", std::process::id()));
        fs::remove_dir_all(&p).ok();
        fs::create_dir_all(&p)?;
        Ok(p)
    }
    fn kernel_watches(hub: &WatchHub) -> usize {
        let fd = hub.shared.inotify.as_raw_fd();
        fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("inotify wd:"))
            .count()
    }
    fn wait_for(
        rx: &Receiver<(PathBuf, FolderEvent)>,
        want: &(PathBuf, FolderEvent),
        t: Duration,
    ) -> bool {
        let end = Instant::now() + t;
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(e) if &e == want => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
        false
    }
    fn drain(rx: &Receiver<(PathBuf, FolderEvent)>) {
        while rx.recv_timeout(Duration::from_millis(300)).is_ok() {}
    }

    // F2: overflow with >16384 events, victim rm -rf'd while hub stalled.
    #[test]
    fn v1_overflow_rmrf_victim_gone_then_fresh() -> R {
        let base = dir("v1")?;
        let busy = base.join("busy");
        let victim = base.join("victim");
        fs::create_dir(&busy)?;
        fs::create_dir_all(victim.join("sub"))?;
        fs::write(victim.join("f"), b"x")?;
        let (hub, rx) = WatchHub::new()?;
        let _b = hub.watch(&busy)?;
        let old = hub.watch(&victim)?;
        {
            let _stall = hub.shared.registry();
            for i in 0..20_000 {
                fs::write(busy.join(format!("f{i}")), b"x")?;
            }
            fs::remove_dir_all(&victim)?;
        }
        assert!(
            wait_for(
                &rx,
                &(victim.clone(), FolderEvent::Gone),
                Duration::from_secs(10)
            ),
            "victim Gone after overflow"
        );
        assert!(!old.is_live());
        assert_eq!(hub.watch_count(), 1);
        assert_eq!(kernel_watches(&hub), 1, "kernel watches");
        fs::create_dir(&victim)?;
        let fresh = hub.watch(&victim)?;
        assert!(fresh.is_live());
        drain(&rx);
        fs::write(victim.join("x"), b"x")?;
        assert!(wait_for(
            &rx,
            &(victim.clone(), FolderEvent::Changed),
            Duration::from_secs(3)
        ));
        drop(old);
        assert_eq!(hub.watch_count(), 2, "old handle drop leaves fresh");
        fs::remove_dir_all(&base).ok();
        Ok(())
    }

    // F2 variant: victim replaced (rm -rf && mkdir) during overflow.
    #[test]
    fn v2_overflow_replaced_victim_gone_then_fresh() -> R {
        let base = dir("v2")?;
        let busy = base.join("busy");
        let victim = base.join("victim");
        fs::create_dir(&busy)?;
        fs::create_dir(&victim)?;
        let (hub, rx) = WatchHub::new()?;
        let _b = hub.watch(&busy)?;
        let old = hub.watch(&victim)?;
        {
            let _stall = hub.shared.registry();
            for i in 0..20_000 {
                fs::write(busy.join(format!("f{i}")), b"x")?;
            }
            fs::remove_dir_all(&victim)?;
            fs::create_dir(&victim)?;
        }
        assert!(
            wait_for(
                &rx,
                &(victim.clone(), FolderEvent::Gone),
                Duration::from_secs(10)
            ),
            "replaced victim Gone after overflow"
        );
        assert!(!old.is_live());
        assert_eq!(kernel_watches(&hub), 1);
        let fresh = hub.watch(&victim)?;
        drain(&rx);
        fs::write(victim.join("x"), b"x")?;
        assert!(wait_for(
            &rx,
            &(victim.clone(), FolderEvent::Changed),
            Duration::from_secs(3)
        ));
        assert!(fresh.is_live());
        fs::remove_dir_all(&base).ok();
        Ok(())
    }

    // Race: folder replaced before watch() takes the registry lock.
    #[test]
    fn v3_identity_recorded_for_the_folder_actually_watched() -> R {
        let base = dir("v3")?;
        let p = base.join("p");
        fs::create_dir(&p)?;
        let (hub, rx) = WatchHub::new()?;
        let hub = Arc::new(hub);
        let first = {
            let stall = hub.shared.registry();
            let h2 = Arc::clone(&hub);
            let p2 = p.clone();
            let t = std::thread::spawn(move || h2.watch(&p2).map_err(|e| e.to_string()));
            std::thread::sleep(Duration::from_millis(300)); // stat done, blocked on lock
            fs::remove_dir(&p)?;
            fs::create_dir(&p)?;
            drop(stall);
            t.join().map_err(|_| "watch thread panicked")??
        };
        drain(&rx);
        let second = hub.watch(&p)?;
        let mut after = Vec::new();
        while let Ok(e) = rx.recv_timeout(Duration::from_millis(500)) {
            after.push(e);
        }
        eprintln!("v3: events after second watch: {after:?}");
        fs::write(p.join("x"), b"x")?;
        let changed = wait_for(
            &rx,
            &(p.clone(), FolderEvent::Changed),
            Duration::from_secs(3),
        );
        eprintln!(
            "v3: first live={} second live={} kernel={} registry={} changed={changed}",
            first.is_live(),
            second.is_live(),
            kernel_watches(&hub),
            hub.watch_count()
        );
        assert!(changed, "a write into the watched folder is reported");
        fs::remove_dir_all(&base).ok();
        Ok(())
    }

    // Eviction wake immediately followed by hub drop: join must not hang.
    #[test]
    fn v4_drop_right_after_eviction_wake() -> R {
        for i in 0..200 {
            let base = dir(&format!("v4-{i}"))?;
            let p = base.join("p");
            fs::create_dir(&p)?;
            let (hub, _rx) = WatchHub::new()?;
            let a = None::<WatchHandle>;
            let _old = hub.watch(&p)?;
            {
                let _stall = hub.shared.registry();
                fs::remove_dir(&p)?;
                fs::create_dir(&p)?;
            }
            let _new = hub.watch(&p)?;
            drop(a);
            let t = Instant::now();
            drop(hub);
            assert!(
                t.elapsed() < Duration::from_secs(1),
                "hub drop took {:?}",
                t.elapsed()
            );
            fs::remove_dir_all(&base).ok();
        }
        Ok(())
    }

    // Stress: 8s of delete/recreate churn, concurrent watch/is_live/drop, overflow floods.
    #[test]
    #[ignore = "8s churn stress; run explicitly"]
    fn v5_stress_churn() -> R {
        use std::sync::atomic::AtomicUsize;
        let secs: u64 = std::env::var("VP_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8);
        let base = dir("v5")?;
        let paths: Vec<PathBuf> = (0..4).map(|i| base.join(format!("d{i}"))).collect();
        for p in &paths {
            fs::create_dir(p)?;
        }
        let busy = base.join("busy");
        fs::create_dir(&busy)?;
        let (hub, rx) = WatchHub::new()?;
        let hub = Arc::new(hub);
        let stop = Arc::new(AtomicBool::new(false));
        let cycles = Arc::new(AtomicUsize::new(0));
        let watches = Arc::new(AtomicUsize::new(0));
        // watchdog
        {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(secs + 30));
                if !stop.load(Ordering::SeqCst) {
                    eprintln!("WATCHDOG: deadlock suspected");
                    std::process::abort();
                }
            });
        }
        let mut threads = Vec::new();
        for _ in 0..2 {
            let (paths, stop, cycles) = (paths.clone(), Arc::clone(&stop), Arc::clone(&cycles));
            threads.push(std::thread::spawn(move || {
                let mut n = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    let Some(p) = paths.get(n % paths.len()) else {
                        break;
                    };
                    fs::remove_dir_all(p).ok();
                    fs::create_dir(p).ok();
                    fs::write(p.join("c"), b"x").ok();
                    cycles.fetch_add(1, Ordering::Relaxed);
                    n += 1;
                }
            }));
        }
        {
            let (busy, stop) = (busy.clone(), Arc::clone(&stop));
            threads.push(std::thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    fs::write(busy.join(format!("f{}", n % 512)), b"x").ok();
                    n += 1;
                }
            }));
        }
        for t in 0..4 {
            let (hub, paths, stop, watches) = (
                Arc::clone(&hub),
                paths.clone(),
                Arc::clone(&stop),
                Arc::clone(&watches),
            );
            threads.push(std::thread::spawn(move || {
                let mut held: Vec<WatchHandle> = Vec::new();
                let mut n = t;
                while !stop.load(Ordering::Relaxed) {
                    let Some(p) = paths.get(n % paths.len()) else {
                        break;
                    };
                    if let Ok(h) = hub.watch(p) {
                        held.push(h);
                        watches.fetch_add(1, Ordering::Relaxed);
                    }
                    held.retain(|h| h.is_live() || n % 3 != 0);
                    if held.len() > 6 {
                        held.remove(0);
                    }
                    n += 7;
                }
                drop(held);
            }));
        }
        let _busy_watch = hub.watch(&busy)?;
        let start = Instant::now();
        let mut events = 0usize;
        while start.elapsed() < Duration::from_secs(secs) {
            while rx.try_recv().is_ok() {
                events += 1;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        stop.store(true, Ordering::SeqCst);
        for t in threads {
            t.join().map_err(|_| "stress thread panicked")?;
        }
        drop(_busy_watch);
        let registry = hub.watch_count();
        // let the hub settle, then count kernel watches
        std::thread::sleep(Duration::from_millis(500));
        while rx.try_recv().is_ok() {
            events += 1;
        }
        let kernel = kernel_watches(&hub);
        eprintln!(
            "v5: cycles={} watches={} events={events} registry_after={registry} kernel_after={kernel}",
            cycles.load(Ordering::Relaxed),
            watches.load(Ordering::Relaxed)
        );
        assert_eq!(registry, 0);
        assert_eq!(kernel, 0, "no leaked kernel watches");
        let hub = Arc::try_unwrap(hub).map_err(|_| "hub still shared")?;
        let t = Instant::now();
        drop(hub);
        eprintln!("v5: hub drop {:?}", t.elapsed());
        assert!(t.elapsed() < Duration::from_secs(1));
        stop.store(true, Ordering::SeqCst);
        fs::remove_dir_all(&base).ok();
        Ok(())
    }
}

#[cfg(test)]
mod device_tests {
    use super::*;
    use std::fs;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const LONG: Duration = Duration::from_secs(3);
    const SHORT: Duration = Duration::from_millis(400);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    // --- the pure decisions ----------------------------------------------------

    #[test]
    fn an_event_is_acted_on_only_after_the_quiet_period() {
        let t0 = Instant::now();
        let mut gate = RefreshGate::default();
        assert!(!gate.pending());
        assert!(!gate.ready(t0), "nothing happened");

        gate.note(t0, DeviceSource::Mounts);
        assert!(gate.pending());
        assert!(!gate.ready(t0 + ms(299)));
        assert!(gate.ready(t0 + DEVICE_QUIET), "300 ms of quiet");
        assert_eq!(DEVICE_QUIET, ms(300));

        gate.start(t0 + ms(300));
        assert!(!gate.pending());
        assert!(!gate.ready(t0 + ms(10_000)), "consumed");
    }

    #[test]
    fn a_burst_merges_into_one_read_and_cannot_postpone_it_forever() {
        let t0 = Instant::now();
        let mut gate = RefreshGate::default();
        // An event every 100 ms keeps resetting the quiet period...
        for step in 0..10 {
            gate.note(t0 + ms(step * 100), DeviceSource::Dev);
            assert!(
                !gate.ready(t0 + ms(step * 100 + 100) - ms(1)),
                "step {step}"
            );
        }
        // ...but the oldest unread one is read after the maximum delay.
        gate.note(t0 + ms(1_400), DeviceSource::Dev);
        assert!(gate.ready(t0 + DEVICE_MAX_DELAY));
        gate.start(t0 + DEVICE_MAX_DELAY);
        assert!(!gate.pending(), "the whole burst was one read");
    }

    #[test]
    fn a_usb_device_is_looked_at_again_when_gvfs_and_udisks_had_time_to_mount_it() {
        let t0 = Instant::now();
        let mut gate = RefreshGate::default();
        gate.note(t0, DeviceSource::Usb);
        assert!(gate.ready(t0 + ms(300)));
        gate.start(t0 + ms(300));
        assert!(!gate.ready(t0 + ms(1_999)));
        assert!(gate.ready(t0 + USB_FOLLOW_UPS[0]), "+2 s");
        gate.start(t0 + USB_FOLLOW_UPS[0]);
        assert!(!gate.ready(t0 + ms(4_999)));
        assert!(gate.ready(t0 + USB_FOLLOW_UPS[1]), "+5 s");
        gate.start(t0 + USB_FOLLOW_UPS[1]);
        assert!(!gate.ready(t0 + ms(60_000)), "and then it stops");
        assert!(!gate.pending());
    }

    #[test]
    fn only_usb_events_ask_for_follow_up_reads() {
        for source in [
            DeviceSource::Dev,
            DeviceSource::DiskById,
            DeviceSource::Mounts,
            DeviceSource::Gvfs,
        ] {
            let t0 = Instant::now();
            let mut gate = RefreshGate::default();
            gate.note(t0, source);
            gate.start(t0 + ms(300));
            assert!(!gate.ready(t0 + ms(2_500)), "{source:?}");
            assert!(!gate.pending(), "{source:?}");
        }
    }

    #[test]
    fn the_slow_poll_runs_only_when_a_watch_is_missing() {
        let now = Instant::now();
        let ago = |seconds| now.checked_sub(Duration::from_secs(seconds));
        assert_eq!(SAFETY_POLL, Duration::from_secs(30));
        assert!(safety_poll_due(None, now, true));
        assert!(safety_poll_due(ago(30), now, true));
        assert!(!safety_poll_due(ago(29), now, true));
        for last in [None, ago(1), ago(3_600)] {
            assert!(!safety_poll_due(last, now, false), "watches are in place");
        }
    }

    #[test]
    fn the_mount_table_poll_reads_pri_and_err_as_a_change() {
        assert_eq!(mount_poll(PollFlags::empty()), MountPoll::Quiet);
        assert_eq!(mount_poll(PollFlags::PRI), MountPoll::Changed);
        assert_eq!(mount_poll(PollFlags::ERR), MountPoll::Changed);
        assert_eq!(
            mount_poll(PollFlags::PRI | PollFlags::ERR),
            MountPoll::Changed
        );
        assert_eq!(mount_poll(PollFlags::HUP), MountPoll::Broken);
        assert_eq!(mount_poll(PollFlags::NVAL), MountPoll::Broken);
        assert_eq!(
            mount_poll(PollFlags::IN | PollFlags::NVAL),
            MountPoll::Broken
        );
    }

    /// The kernel flags `/proc/self/mountinfo` for `poll` only when the
    /// table changed since the last look, and a change is reported once:
    /// the watcher does not spin on it. Mounting needs root, so this checks
    /// the idle behavior of the real file; a mount from another process
    /// during the test only makes the first answer `Changed`.
    #[test]
    fn the_real_mount_table_reports_a_change_once_not_continuously() -> TestResult {
        let table = fs::File::open("/proc/self/mountinfo")?;
        let answer = || -> Result<MountPoll, Box<dyn std::error::Error>> {
            let mut fds = [PollFd::new(&table, PollFlags::PRI)];
            poll(&mut fds, Some(&rustix::event::Timespec::default()))?;
            Ok(mount_poll(fds[0].revents()))
        };
        let first = answer()?;
        assert_ne!(first, MountPoll::Broken);
        assert_eq!(
            answer()?,
            MountPoll::Quiet,
            "nothing new since the last answer"
        );
        assert_eq!(answer()?, MountPoll::Quiet);
        Ok(())
    }

    // --- the real thing on a scratch directory ------------------------------------

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> io::Result<Self> {
            let path = std::env::temp_dir().join(format!(
                "fileflux-devwatch-{label}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            fs::remove_dir_all(&path).ok();
            fs::create_dir_all(&path)?;
            Ok(Self(path))
        }

        fn roots(&self) -> DeviceRoots {
            DeviceRoots {
                dev: self.0.join("dev"),
                by_id: self.0.join("dev/disk/by-id"),
                usb: self.0.join("dev/bus/usb"),
                mountinfo: self.0.join("mountinfo"),
                gvfs: Some(self.0.join("run/gvfs")),
            }
        }

        /// The fake `/dev`, `/dev/disk/by-id`, `/dev/bus/usb/001`, the
        /// runtime directory and a mount table file.
        fn populate(&self) -> io::Result<()> {
            fs::create_dir_all(self.0.join("dev/disk/by-id"))?;
            fs::create_dir_all(self.0.join("dev/bus/usb/001"))?;
            fs::create_dir_all(self.0.join("run"))?;
            fs::write(self.0.join("mountinfo"), b"")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    /// Every source that arrives within `wait`.
    fn sources(rx: &Receiver<DeviceSource>, wait: Duration) -> Vec<DeviceSource> {
        let mut seen = Vec::new();
        while let Ok(source) = rx.recv_timeout(wait) {
            if !seen.contains(&source) {
                seen.push(source);
            }
        }
        seen
    }

    fn expect(rx: &Receiver<DeviceSource>, want: DeviceSource) -> TestResult {
        let end = Instant::now() + LONG;
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(source) if source == want => return Ok(()),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        Err(format!("no {want:?} event within {LONG:?}").into())
    }

    #[test]
    fn nodes_appearing_and_vanishing_in_dev_and_by_id_are_reported() -> TestResult {
        let scratch = Scratch::new("dev")?;
        scratch.populate()?;
        let (watcher, rx) = DeviceWatcher::new(&scratch.roots())?;
        assert!(!watcher.degraded());

        fs::write(scratch.0.join("dev/sdb1"), b"")?;
        expect(&rx, DeviceSource::Dev)?;
        fs::remove_file(scratch.0.join("dev/sdb1"))?;
        expect(&rx, DeviceSource::Dev)?;
        fs::write(scratch.0.join("dev/disk/by-id/usb-Kingston-part1"), b"")?;
        expect(&rx, DeviceSource::DiskById)?;
        fs::remove_file(scratch.0.join("dev/disk/by-id/usb-Kingston-part1"))?;
        expect(&rx, DeviceSource::DiskById)?;
        Ok(())
    }

    #[test]
    fn a_usb_node_is_reported_even_on_a_bus_that_appears_later() -> TestResult {
        let scratch = Scratch::new("usb")?;
        scratch.populate()?;
        let (_watcher, rx) = DeviceWatcher::new(&scratch.roots())?;

        fs::write(scratch.0.join("dev/bus/usb/001/007"), b"")?;
        expect(&rx, DeviceSource::Usb)?;
        sources(&rx, SHORT);
        fs::create_dir(scratch.0.join("dev/bus/usb/002"))?;
        expect(&rx, DeviceSource::Usb)?;
        sources(&rx, SHORT);
        fs::write(scratch.0.join("dev/bus/usb/002/003"), b"")?;
        expect(&rx, DeviceSource::Usb)?;
        Ok(())
    }

    #[test]
    fn gvfs_mounts_appearing_and_vanishing_are_reported() -> TestResult {
        let scratch = Scratch::new("gvfs")?;
        scratch.populate()?;
        let gvfs = scratch.0.join("run/gvfs");
        fs::create_dir(&gvfs)?;
        let (watcher, rx) = DeviceWatcher::new(&scratch.roots())?;
        assert!(!watcher.degraded());

        fs::create_dir_all(gvfs.join("mtp:host=Fake_Phone_123/Internal shared storage/DCIM"))?;
        expect(&rx, DeviceSource::Gvfs)?;
        // Only the mounts themselves matter, not what is inside them.
        sources(&rx, SHORT);
        fs::write(
            gvfs.join("mtp:host=Fake_Phone_123/Internal shared storage/a.jpg"),
            b"",
        )?;
        assert!(
            sources(&rx, SHORT).is_empty(),
            "files inside a mount are not news"
        );
        fs::remove_dir_all(gvfs.join("mtp:host=Fake_Phone_123"))?;
        expect(&rx, DeviceSource::Gvfs)?;
        Ok(())
    }

    #[test]
    fn a_gvfs_directory_that_does_not_exist_yet_is_picked_up_when_it_is_created() -> TestResult {
        let scratch = Scratch::new("gvfs-late")?;
        scratch.populate()?;
        let gvfs = scratch.0.join("run/gvfs");
        let (watcher, rx) = DeviceWatcher::new(&scratch.roots())?;
        assert!(!watcher.degraded(), "no gvfs session yet is normal");

        fs::write(scratch.0.join("run/pipewire-0"), b"")?;
        assert!(
            sources(&rx, SHORT).is_empty(),
            "other runtime files are not news"
        );
        fs::create_dir(&gvfs)?;
        expect(&rx, DeviceSource::Gvfs)?;
        sources(&rx, SHORT);
        fs::create_dir(gvfs.join("sftp:host=x"))?;
        expect(&rx, DeviceSource::Gvfs)?;
        // The gvfs session ends and a new one starts.
        fs::remove_dir_all(&gvfs)?;
        expect(&rx, DeviceSource::Gvfs)?;
        sources(&rx, SHORT);
        fs::create_dir(&gvfs)?;
        expect(&rx, DeviceSource::Gvfs)?;
        sources(&rx, SHORT);
        fs::create_dir(gvfs.join("sftp:host=y"))?;
        expect(&rx, DeviceSource::Gvfs)?;
        Ok(())
    }

    #[test]
    fn the_mount_table_file_is_watched_without_spinning() -> TestResult {
        let scratch = Scratch::new("mountinfo")?;
        scratch.populate()?;
        let (_watcher, rx) = DeviceWatcher::new(&scratch.roots())?;
        // A regular file never signals: the watcher is asleep, not looping.
        assert!(sources(&rx, SHORT).is_empty());
        Ok(())
    }

    #[test]
    fn a_missing_dev_or_mount_table_means_the_slow_poll_takes_over() -> TestResult {
        let scratch = Scratch::new("degraded")?;
        let (watcher, _rx) = DeviceWatcher::new(&scratch.roots())?;
        assert!(watcher.degraded(), "nothing to watch");
        scratch.populate()?;
        let mut roots = scratch.roots();
        roots.mountinfo = scratch.0.join("absent");
        let (watcher, _rx) = DeviceWatcher::new(&roots)?;
        assert!(watcher.degraded(), "no mount table");
        // Optional places do not count: no by-id, no USB, no gvfs.
        let mut roots = scratch.roots();
        roots.by_id = scratch.0.join("absent");
        roots.usb = scratch.0.join("absent");
        roots.gvfs = None;
        let (watcher, _rx) = DeviceWatcher::new(&roots)?;
        assert!(!watcher.degraded());
        Ok(())
    }

    #[test]
    fn dropping_the_watcher_stops_its_thread_promptly() -> TestResult {
        let scratch = Scratch::new("drop")?;
        scratch.populate()?;
        let (watcher, rx) = DeviceWatcher::new(&scratch.roots())?;
        let started = Instant::now();
        drop(watcher);
        assert!(started.elapsed() < LONG, "the join returned");
        assert!(matches!(
            rx.recv_timeout(LONG),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
        Ok(())
    }
}
