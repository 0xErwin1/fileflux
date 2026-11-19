//! Recursive search by file name, read-only.
//!
//! [`search`] walks a folder breadth-first, so shallow matches stream out
//! first, and reports the entries whose display name contains the query,
//! case-insensitively, the same rule as the folder filter. It never changes
//! anything on disk and never follows a symlinked folder (a link is a result
//! when its own name matches, but nothing is searched through it), so a link
//! loop cannot trap it. By default it also stays on the root's device and
//! skips what the listing hides, so a search finds what browsing shows.
//!
//! The walk is blocking and meant for a thread of its own: it checks a
//! cancel flag before every entry, hands matches over in batches (a full
//! batch, or whatever was found when [`BATCH_INTERVAL`] passed) and stops at
//! a cap.

use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::Entry;
use super::fs::{build_entry, read_hidden_names};
use super::ops::is_transfer_temp;

/// The most matches one search reports; the view says it stopped there.
pub const MAX_RESULTS: usize = 10_000;
/// Matches handed over at once, at most.
pub const BATCH_SIZE: usize = 200;
/// How long found matches wait for a full batch.
pub const BATCH_INTERVAL: Duration = Duration::from_millis(100);

/// How many entries pass between two looks at the clock.
const CLOCK_EVERY: usize = 64;

/// The device a folder is on, from its path and its own (not followed)
/// metadata. Injectable so the device boundary is testable on one device.
pub type DeviceOf = fn(&Path, &Metadata) -> u64;

fn metadata_device(_path: &Path, meta: &Metadata) -> u64 {
    meta.dev()
}

/// What to search for and how far to go.
#[derive(Clone, Debug)]
pub struct SearchOptions {
    /// The text a name must contain, case-insensitively. Empty finds nothing.
    pub query: String,
    /// Skip dotfiles and the names a folder's `.hidden` file lists, and do
    /// not enter them: the rule of the folder listing.
    pub hide_hidden: bool,
    /// Enter folders on other devices (mount points still match by name).
    pub cross_devices: bool,
    /// Stop after this many matches.
    pub cap: usize,
    /// Matches handed over at once, at most.
    pub batch_size: usize,
}

impl SearchOptions {
    /// Hidden entries skipped, the root's device only, [`MAX_RESULTS`]
    /// matches in batches of [`BATCH_SIZE`].
    #[must_use]
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            hide_hidden: true,
            cross_devices: false,
            cap: MAX_RESULTS,
            batch_size: BATCH_SIZE,
        }
    }
}

/// How a search ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchSummary {
    /// The matches handed over.
    pub found: usize,
    /// A match past the cap exists: the list stops at the cap.
    pub capped: bool,
    /// The cancel flag stopped the walk.
    pub cancelled: bool,
    /// Folders that could not be read (permissions, vanished): counted, the
    /// search goes on.
    pub unreadable_dirs: usize,
}

/// Searches `root` and its subfolders for [`SearchOptions::query`], handing
/// the matches to `emit` in batches, shallow ones first. See the module
/// docs.
pub fn search(
    root: &Path,
    options: &SearchOptions,
    cancel: &AtomicBool,
    emit: impl FnMut(Vec<Entry>),
) -> SearchSummary {
    search_with_device(root, options, metadata_device, cancel, emit)
}

/// [`search`] with another way to tell which device a folder is on.
pub fn search_with_device(
    root: &Path,
    options: &SearchOptions,
    device_of: DeviceOf,
    cancel: &AtomicBool,
    emit: impl FnMut(Vec<Entry>),
) -> SearchSummary {
    let mut walk = Walk {
        needle: options.query.to_lowercase(),
        options,
        device_of,
        cancel,
        emit,
        batch: Vec::new(),
        last_flush: Instant::now(),
        since_clock: 0,
        summary: SearchSummary::default(),
    };
    if !walk.needle.is_empty() {
        walk.run(root);
    }
    walk.flush();
    walk.summary
}

/// Why the walk stops early.
enum Stop {
    Cancelled,
    Capped,
}

struct Walk<'a, F: FnMut(Vec<Entry>)> {
    needle: String,
    options: &'a SearchOptions,
    device_of: DeviceOf,
    cancel: &'a AtomicBool,
    emit: F,
    batch: Vec<Entry>,
    last_flush: Instant,
    since_clock: usize,
    summary: SearchSummary,
}

impl<F: FnMut(Vec<Entry>)> Walk<'_, F> {
    fn run(&mut self, root: &Path) {
        let root_device = match fs::metadata(root) {
            Ok(meta) => (self.device_of)(root, &meta),
            Err(_) => {
                self.summary.unreadable_dirs += 1;
                return;
            }
        };
        let mut queue = VecDeque::from([root.to_path_buf()]);
        // A folder is read once, even when a bind mount shows it twice.
        let mut seen: HashSet<(u64, u64)> = HashSet::new();
        if let Ok(meta) = fs::metadata(root) {
            seen.insert((meta.dev(), meta.ino()));
        }
        while let Some(dir) = queue.pop_front() {
            if let Err(stop) = self.visit(&dir, root_device, &mut queue, &mut seen) {
                match stop {
                    Stop::Cancelled => self.summary.cancelled = true,
                    Stop::Capped => self.summary.capped = true,
                }
                return;
            }
        }
    }

    /// One folder: its matches are reported and its subfolders queued.
    fn visit(
        &mut self,
        dir: &Path,
        root_device: u64,
        queue: &mut VecDeque<PathBuf>,
        seen: &mut HashSet<(u64, u64)>,
    ) -> Result<(), Stop> {
        self.tick()?;
        let Ok(reader) = fs::read_dir(dir) else {
            self.summary.unreadable_dirs += 1;
            return Ok(());
        };
        let mut items = Vec::new();
        let mut failed = false;
        let mut has_hidden_file = false;
        for item in reader {
            match item {
                Ok(item) => {
                    has_hidden_file |= item.file_name() == ".hidden";
                    items.push(item);
                }
                Err(_) => failed = true,
            }
        }
        if failed {
            self.summary.unreadable_dirs += 1;
        }
        let hidden_names = if self.options.hide_hidden && has_hidden_file {
            read_hidden_names(dir)
        } else {
            std::collections::BTreeSet::<OsString>::new()
        };
        for item in items {
            self.tick()?;
            let name = item.file_name();
            if is_transfer_temp(&name)
                || (self.options.hide_hidden
                    && (name.as_encoded_bytes().first() == Some(&b'.')
                        || hidden_names.contains(&name)))
            {
                continue;
            }
            let Ok(file_type) = item.file_type() else {
                continue;
            };
            // Only a real folder is entered, never a link to one; its
            // metadata is read before the entry is consumed below.
            let folder = if file_type.is_dir() {
                item.metadata().ok().map(|meta| (item.path(), meta))
            } else {
                None
            };
            if name.to_string_lossy().to_lowercase().contains(&self.needle) {
                self.matched(item)?;
            }
            if let Some((path, meta)) = folder {
                let same_device = (self.device_of)(&path, &meta) == root_device;
                if (self.options.cross_devices || same_device)
                    && seen.insert((meta.dev(), meta.ino()))
                {
                    queue.push_back(path);
                }
            }
        }
        Ok(())
    }

    fn matched(&mut self, item: fs::DirEntry) -> Result<(), Stop> {
        if self.summary.found + self.batch.len() >= self.options.cap {
            return Err(Stop::Capped);
        }
        // The entry as a folder listing builds it; one that cannot be
        // described is left out rather than shown half-known.
        let Ok(mut entry) = build_entry(item) else {
            return Ok(());
        };
        entry.origin = entry.path.parent().map(Path::to_path_buf);
        self.batch.push(entry);
        if self.batch.len() >= self.options.batch_size.max(1) {
            self.flush();
        }
        Ok(())
    }

    /// Before each entry: the cancel flag, and a late batch goes out.
    fn tick(&mut self) -> Result<(), Stop> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(Stop::Cancelled);
        }
        self.since_clock += 1;
        if self.since_clock >= CLOCK_EVERY {
            self.since_clock = 0;
            if self.last_flush.elapsed() >= BATCH_INTERVAL {
                self.flush();
            }
        }
        Ok(())
    }

    fn flush(&mut self) {
        self.last_flush = Instant::now();
        if self.batch.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut self.batch);
        self.summary.found += batch.len();
        (self.emit)(batch);
    }
}
