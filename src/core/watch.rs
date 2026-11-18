//! Live directory watching for the explorer (inotify through `rustix`).
//!
//! A [`Watcher`] observes the direct children of one directory. Its
//! [`Watcher::wait`] blocks the calling (background) thread for at most the
//! given timeout and reports what happened since the last call; dropping the
//! watcher closes the inotify instance. [`Debounce`] is the pure decision of
//! when a burst of changes has settled enough to rescan.

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use rustix::io::Errno;

/// The quiet time after the last change before a rescan is due.
const QUIET: Duration = Duration::from_millis(250);
/// The longest a rescan may be postponed by a continuous stream of changes.
const MAX_DELAY: Duration = Duration::from_millis(1_000);

/// What a [`Watcher::wait`] observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchEvent {
    /// A direct child was created, deleted, moved in/out or changed.
    Changed,
    /// Nothing happened within the timeout.
    Idle,
    /// The watched directory itself was deleted or moved away; the watcher
    /// is finished and keeps reporting this immediately.
    Gone,
}

/// An inotify watch on one directory's direct children.
#[derive(Debug)]
pub struct Watcher {
    fd: OwnedFd,
    gone: std::cell::Cell<bool>,
}

impl Watcher {
    /// Starts watching `dir`; fails when it is not a readable directory.
    pub fn new(dir: &Path) -> io::Result<Self> {
        let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?;
        let mask = WatchFlags::CREATE
            | WatchFlags::DELETE
            | WatchFlags::MOVED_FROM
            | WatchFlags::MOVED_TO
            | WatchFlags::ATTRIB
            | WatchFlags::CLOSE_WRITE
            | WatchFlags::DELETE_SELF
            | WatchFlags::MOVE_SELF
            | WatchFlags::ONLYDIR;
        inotify::add_watch(&fd, dir, mask)?;
        Ok(Self {
            fd,
            gone: std::cell::Cell::new(false),
        })
    }

    /// Blocks for at most `timeout` until something happens. All queued
    /// events are drained, so one `Changed` covers a whole burst.
    pub fn wait(&self, timeout: Duration) -> WatchEvent {
        if self.gone.get() {
            return WatchEvent::Gone;
        }
        let deadline = Instant::now() + timeout;
        loop {
            match self.drain() {
                WatchEvent::Idle => {}
                event => return event,
            }
            let now = Instant::now();
            if now >= deadline {
                return WatchEvent::Idle;
            }
            // Sleep in the kernel until the queue has events or the deadline
            // passes, instead of waking up on a fixed cadence.
            let remaining = deadline - now;
            let timeout = rustix::time::Timespec {
                tv_sec: i64::try_from(remaining.as_secs()).unwrap_or(i64::MAX),
                tv_nsec: i64::from(remaining.subsec_nanos()),
            };
            let mut fds = [PollFd::new(&self.fd, PollFlags::IN)];
            match poll(&mut fds, Some(&timeout)) {
                Ok(_) | Err(Errno::INTR) => {}
                Err(error) => {
                    log::warn!("inotify poll failed, stopping the watch: {error}");
                    self.gone.set(true);
                    return WatchEvent::Gone;
                }
            }
        }
    }

    /// Reads every queued event without blocking.
    fn drain(&self) -> WatchEvent {
        let mut buffer = [MaybeUninit::<u8>::uninit(); 4096];
        let mut reader = inotify::Reader::new(&self.fd, &mut buffer);
        let mut outcome = WatchEvent::Idle;
        loop {
            match reader.next() {
                Ok(event) => {
                    let flags = event.events();
                    if flags.intersects(
                        ReadFlags::DELETE_SELF | ReadFlags::MOVE_SELF | ReadFlags::IGNORED,
                    ) {
                        self.gone.set(true);
                        return WatchEvent::Gone;
                    }
                    // Anything else, including a queue overflow, means the
                    // listing may be stale.
                    outcome = WatchEvent::Changed;
                }
                Err(Errno::AGAIN) => return outcome,
                Err(error) => {
                    log::warn!("inotify read failed, stopping the watch: {error}");
                    self.gone.set(true);
                    return WatchEvent::Gone;
                }
            }
        }
    }
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

    #[test]
    fn create_rename_and_delete_are_reported() -> TestResult {
        let scratch = Scratch::new("crud")?;
        let watcher = Watcher::new(&scratch.0)?;
        fs::write(scratch.0.join("a.txt"), b"x")?;
        assert_eq!(watcher.wait(LONG), WatchEvent::Changed, "create");
        fs::rename(scratch.0.join("a.txt"), scratch.0.join("b.txt"))?;
        assert_eq!(watcher.wait(LONG), WatchEvent::Changed, "rename");
        fs::remove_file(scratch.0.join("b.txt"))?;
        assert_eq!(watcher.wait(LONG), WatchEvent::Changed, "delete");
        Ok(())
    }

    #[test]
    fn an_idle_directory_reports_nothing() -> TestResult {
        let scratch = Scratch::new("idle")?;
        let watcher = Watcher::new(&scratch.0)?;
        let started = Instant::now();
        assert_eq!(watcher.wait(SHORT), WatchEvent::Idle);
        assert!(started.elapsed() >= SHORT, "wait honors its timeout");
        Ok(())
    }

    #[test]
    fn a_deleted_directory_is_reported_gone_without_spinning() -> TestResult {
        let scratch = Scratch::new("gone")?;
        let victim = scratch.0.join("victim");
        fs::create_dir(&victim)?;
        let watcher = Watcher::new(&victim)?;
        fs::remove_dir(&victim)?;
        let mut gone = false;
        for _ in 0..4 {
            if watcher.wait(LONG) == WatchEvent::Gone {
                gone = true;
                break;
            }
        }
        assert!(gone, "the removed directory is reported");
        // Once gone it stays gone, immediately.
        let started = Instant::now();
        assert_eq!(watcher.wait(LONG), WatchEvent::Gone);
        assert!(started.elapsed() < SHORT);
        Ok(())
    }

    #[test]
    fn watching_a_missing_directory_fails() -> TestResult {
        let scratch = Scratch::new("missing")?;
        assert!(Watcher::new(&scratch.0.join("nope")).is_err());
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
